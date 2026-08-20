use std::{fs, path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::Mutex,
    time::timeout,
};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use xanhtab::{
    config::{Config, NetworkConfig},
    model::EgressMode,
    netd::{CommandSpec, EgressCommand, EgressRequest, EgressResponse, command_plan},
};

#[derive(Parser)]
#[command(version, about = "Privileged XanhTab egress policy helper")]
struct Args {
    #[arg(long, env = "XANHTAB_CONFIG", default_value = "config/xanhtab.toml")]
    config: PathBuf,
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "xanhtab=info".into()),
        )
        .compact()
        .init();
    let args = Args::parse();
    let config = Config::load(&args.config)?;
    if let Some(parent) = config.network.netd_socket.parent() {
        fs::create_dir_all(parent)?;
    }
    if config.network.netd_socket.exists() {
        fs::remove_file(&config.network.netd_socket)?;
    }
    let listener = UnixListener::bind(&config.network.netd_socket)
        .with_context(|| format!("failed to bind {}", config.network.netd_socket.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            &config.network.netd_socket,
            fs::Permissions::from_mode(0o660),
        )?;
    }
    info!(socket = %config.network.netd_socket.display(), "xanhtab-netd ready");
    let dry_run = args.dry_run;
    let operation_lock = Arc::new(Mutex::new(()));
    loop {
        let (stream, _) = listener.accept().await?;
        let network = config.network.clone();
        let operation_lock = operation_lock.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, network, dry_run, operation_lock).await {
                error!(%error, "netd request failed");
            }
        });
    }
}

async fn handle(
    stream: UnixStream,
    config: NetworkConfig,
    dry_run: bool,
    operation_lock: Arc<Mutex<()>>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let client_deadline = Duration::from_secs(config.ipc_timeout_seconds);
    timeout(client_deadline, BufReader::new(reader).read_line(&mut line))
        .await
        .context("netd request read timed out")??;
    let request: EgressRequest = serde_json::from_str(&line).context("invalid request")?;
    if request.version != 1 {
        anyhow::bail!("unsupported netd protocol version");
    }
    let _guard = operation_lock.lock().await;
    let result = match timeout(helper_deadline(config.ipc_timeout_seconds), async {
        match request.command {
            EgressCommand::Apply { mode } => apply(&config, mode, dry_run).await,
            EgressCommand::Reset => apply(&config, EgressMode::Direct, dry_run).await,
            EgressCommand::Status => Ok(EgressResponse {
                ok: true,
                active_mode: EgressMode::Direct,
                proxy_url: None,
                detail: "status is intentionally stateless in v1".into(),
            }),
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("netd command timed out")),
    };
    let response = result.unwrap_or_else(|error| EgressResponse {
        ok: false,
        active_mode: EgressMode::Direct,
        proxy_url: None,
        detail: error.to_string(),
    });
    let mut payload = serde_json::to_vec(&response)?;
    payload.push(b'\n');
    writer.write_all(&payload).await?;
    Ok(())
}

fn helper_deadline(ipc_timeout_seconds: u64) -> Duration {
    Duration::from_millis(
        ipc_timeout_seconds
            .saturating_mul(1_000)
            .saturating_sub(250)
            .max(1),
    )
}

async fn apply(config: &NetworkConfig, mode: EgressMode, dry_run: bool) -> Result<EgressResponse> {
    let _ = run_plan(&reset_wireguard_plan(config), dry_run, true).await;
    run_plan(&command_plan(config, mode), dry_run, true).await?;
    let proxy_url = match mode {
        EgressMode::Tor => Some(config.tor_proxy.clone()),
        EgressMode::Warp => Some(config.warp_proxy.clone()),
        EgressMode::Proxy => Some(
            fs::read_to_string(&config.proxy_url_file)?
                .trim()
                .to_string(),
        ),
        _ => None,
    };
    Ok(EgressResponse {
        ok: true,
        active_mode: mode,
        proxy_url,
        detail: "egress policy applied".into(),
    })
}

fn reset_wireguard_plan(config: &NetworkConfig) -> Vec<CommandSpec> {
    vec![CommandSpec {
        program: "wg-quick",
        args: vec!["down".into(), config.wireguard_config.display().to_string()],
        stdin: None,
    }]
}

async fn run_plan(plan: &[CommandSpec], dry_run: bool, allow_existing_table: bool) -> Result<()> {
    for spec in plan {
        info!(program = spec.program, args = ?spec.args, dry_run, "egress command");
        if dry_run {
            continue;
        }
        let mut command = Command::new(spec.program);
        command
            .args(&spec.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to run {}", spec.program))?;
        if let Some(input) = &spec.stdin {
            child
                .stdin
                .take()
                .context("command stdin unavailable")?
                .write_all(input.as_bytes())
                .await?;
        }
        let output = child
            .wait_with_output()
            .await
            .with_context(|| format!("failed to wait for {}", spec.program))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let ignorable_existing_table = allow_existing_table
                && spec.program == "nft"
                && spec.args == ["add", "table", "inet", "xanhtab"]
                && stderr.contains("File exists");
            if ignorable_existing_table {
                warn!(%stderr, "reusing existing XanhTab nft table before atomic replacement");
                continue;
            }
            anyhow::bail!("{} failed: {}", spec.program, stderr.trim());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_deadline_leaves_time_for_the_response() {
        assert_eq!(helper_deadline(1), Duration::from_millis(750));
        assert_eq!(helper_deadline(2), Duration::from_millis(1_750));
    }
}
