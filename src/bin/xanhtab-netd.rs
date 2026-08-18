use std::{fs, path::PathBuf, process::Stdio};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
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
    loop {
        let (stream, _) = listener.accept().await?;
        let network = config.network.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, network, dry_run).await {
                error!(%error, "netd request failed");
            }
        });
    }
}

async fn handle(stream: UnixStream, config: NetworkConfig, dry_run: bool) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).await?;
    let request: EgressRequest = serde_json::from_str(&line).context("invalid request")?;
    if request.version != 1 {
        anyhow::bail!("unsupported netd protocol version");
    }
    let result = match request.command {
        EgressCommand::Apply { mode } => apply(&config, mode, dry_run).await,
        EgressCommand::Reset => apply(&config, EgressMode::Direct, dry_run).await,
        EgressCommand::Status => Ok(EgressResponse {
            ok: true,
            active_mode: EgressMode::Direct,
            proxy_url: None,
            detail: "status is intentionally stateless in v1".into(),
        }),
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
    }]
}

async fn run_plan(plan: &[CommandSpec], dry_run: bool, allow_delete_missing: bool) -> Result<()> {
    for spec in plan {
        info!(program = spec.program, args = ?spec.args, dry_run, "egress command");
        if dry_run {
            continue;
        }
        let output = Command::new(spec.program)
            .args(&spec.args)
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("failed to run {}", spec.program))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let ignorable_delete = allow_delete_missing
                && spec.args.first().is_some_and(|arg| arg == "delete")
                && stderr.contains("No such file or directory");
            if ignorable_delete {
                warn!(%stderr, "ignoring absent XanhTab nft table");
                continue;
            }
            anyhow::bail!("{} failed: {}", spec.program, stderr.trim());
        }
    }
    Ok(())
}
