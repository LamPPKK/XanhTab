use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpStream, UnixListener, UnixStream},
    process::Command,
    sync::Mutex,
    time::timeout,
};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use xanhtab::{
    config::{Config, NetworkConfig},
    egress::{validate_proxy_url, validate_proxy_url_file, validate_wireguard_config_file},
    model::EgressMode,
    netd::{
        CommandSpec, EgressCommand, EgressRequest, EgressResponse, IP_PROGRAM, NFT_PROGRAM,
        WIREGUARD_INTERFACE, WIREGUARD_ROUTE_TABLE, active_policy_plan, kill_switch_plan,
        wireguard_setup_plan, wireguard_teardown_plan,
    },
};

#[derive(Parser)]
#[command(version, about = "Privileged XanhTab egress policy helper")]
struct Args {
    #[arg(long, env = "XANHTAB_CONFIG", default_value = "config/xanhtab.toml")]
    config: PathBuf,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    check_wireguard_config: Option<PathBuf>,
    #[arg(long)]
    check_proxy_url: Option<PathBuf>,
    #[arg(long, requires = "check_proxy_url")]
    print_proxy_endpoint: bool,
    #[arg(
        long,
        conflicts_with_all = ["check_wireguard_config", "check_proxy_url", "print_proxy_endpoint"]
    )]
    cleanup: bool,
}

#[derive(Debug, Default)]
struct NetworkState {
    active_mode: Option<EgressMode>,
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
    if let Some(path) = &args.check_wireguard_config {
        validate_wireguard_config_file(path, None)?;
    }
    if let Some(path) = &args.check_proxy_url {
        let expected = if args.print_proxy_endpoint {
            None
        } else {
            Some(config.network.proxy_endpoint.parse::<SocketAddr>()?)
        };
        let proxy = validate_proxy_url_file(path, expected, None)?;
        if args.print_proxy_endpoint {
            println!("{}", proxy.endpoint);
        }
    }
    if args.check_wireguard_config.is_some() || args.check_proxy_url.is_some() {
        return Ok(());
    }
    if args.cleanup {
        return cleanup_network(&config.network, args.dry_run).await;
    }
    initialize_network(&config.network, args.dry_run).await?;
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
    let state = Arc::new(Mutex::new(NetworkState::default()));
    loop {
        let (stream, _) = listener.accept().await?;
        let network = config.network.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, network, dry_run, state).await {
                error!(%error, "netd request failed");
            }
        });
    }
}

async fn handle(
    stream: UnixStream,
    config: NetworkConfig,
    dry_run: bool,
    state: Arc<Mutex<NetworkState>>,
) -> Result<()> {
    let peer_uid = stream
        .peer_cred()
        .context("failed to read netd peer credentials")?
        .uid();
    authorize_peer_uid(peer_uid, config.control_uid, config.browser_uid)?;
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
    let mut state = state.lock().await;
    let result = match timeout(helper_deadline(config.ipc_timeout_seconds), async {
        match request.command {
            EgressCommand::Apply { mode } => transition(&config, mode, dry_run, &mut state).await,
            EgressCommand::Reset => {
                transition(&config, EgressMode::Direct, dry_run, &mut state).await
            }
            EgressCommand::Status => Ok(status_response(&state)),
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

fn authorize_peer_uid(peer_uid: u32, control_uid: Option<u32>, browser_uid: u32) -> Result<()> {
    match control_uid {
        Some(expected) if peer_uid == expected => Ok(()),
        Some(_) => anyhow::bail!("netd peer is not the configured control-plane UID"),
        None if peer_uid != browser_uid => Ok(()),
        None => anyhow::bail!("browser UID is not authorized to call netd"),
    }
}

fn helper_deadline(ipc_timeout_seconds: u64) -> Duration {
    Duration::from_millis(
        ipc_timeout_seconds
            .saturating_mul(1_000)
            .saturating_sub(250)
            .max(1),
    )
}

fn status_response(state: &NetworkState) -> EgressResponse {
    let active_mode = state.active_mode.unwrap_or(EgressMode::Direct);
    EgressResponse {
        ok: state.active_mode.is_some(),
        active_mode,
        proxy_url: None,
        detail: if state.active_mode.is_some() {
            "egress policy active".into()
        } else {
            "browser UID is held by the transition kill-switch".into()
        },
    }
}

async fn initialize_network(config: &NetworkConfig, dry_run: bool) -> Result<()> {
    run_plan(&kill_switch_plan(config), dry_run, true, false).await?;
    teardown_wireguard(config, dry_run).await
}

async fn cleanup_network(config: &NetworkConfig, dry_run: bool) -> Result<()> {
    run_plan(&kill_switch_plan(config), dry_run, true, false).await?;
    teardown_wireguard(config, dry_run).await?;
    let delete_table = CommandSpec {
        program: NFT_PROGRAM,
        args: vec![
            "delete".into(),
            "table".into(),
            "inet".into(),
            "xanhtab".into(),
        ],
        stdin: None,
    };
    run_plan(&[delete_table], dry_run, false, true).await
}

async fn transition(
    config: &NetworkConfig,
    mode: EgressMode,
    dry_run: bool,
    state: &mut NetworkState,
) -> Result<EgressResponse> {
    state.active_mode = None;
    run_plan(&kill_switch_plan(config), dry_run, true, false).await?;
    let result = transition_inner(config, mode, dry_run).await;
    if let Err(error) = result {
        let mut failures = vec![error.to_string()];
        if let Err(cleanup_error) = teardown_wireguard(config, dry_run).await {
            failures.push(format!("WireGuard cleanup: {cleanup_error}"));
        }
        if let Err(kill_error) = run_plan(&kill_switch_plan(config), dry_run, true, false).await {
            failures.push(format!("kill-switch restore: {kill_error}"));
        }
        anyhow::bail!(failures.join("; "));
    }
    state.active_mode = Some(mode);
    Ok(EgressResponse {
        ok: true,
        active_mode: mode,
        proxy_url: None,
        detail: "egress policy applied after fail-closed transition".into(),
    })
}

async fn transition_inner(config: &NetworkConfig, mode: EgressMode, dry_run: bool) -> Result<()> {
    teardown_wireguard(config, dry_run).await?;
    let endpoint = match mode {
        EgressMode::Direct | EgressMode::WireGuard => None,
        EgressMode::Tor => Some(validate_proxy_url(&config.tor_proxy, false)?.endpoint),
        EgressMode::Warp => Some(validate_proxy_url(&config.warp_proxy, false)?.endpoint),
        EgressMode::Proxy => Some(
            validate_proxy_url_file(
                &config.proxy_url_file,
                Some(config.proxy_endpoint.parse()?),
                Some(0),
            )?
            .endpoint,
        ),
    };
    if mode == EgressMode::WireGuard {
        validate_wireguard_config_file(&config.wireguard_config, Some(0))?;
        run_plan(&wireguard_setup_plan(config), dry_run, false, false).await?;
    }
    if let Some(endpoint) = endpoint {
        probe_proxy(endpoint, dry_run).await?;
    }
    run_plan(
        &active_policy_plan(config, mode, endpoint)?,
        dry_run,
        false,
        false,
    )
    .await
}

async fn teardown_wireguard(config: &NetworkConfig, dry_run: bool) -> Result<()> {
    run_plan(&wireguard_teardown_plan(config), dry_run, false, true).await?;
    if dry_run {
        return Ok(());
    }
    if Path::new("/sys/class/net")
        .join(WIREGUARD_INTERFACE)
        .exists()
    {
        anyhow::bail!("dedicated WireGuard interface remained after teardown");
    }
    for family in [false, true] {
        if policy_rule_remains(config, family).await? {
            anyhow::bail!("dedicated WireGuard UID policy rule remained after teardown");
        }
    }
    Ok(())
}

async fn policy_rule_remains(config: &NetworkConfig, ipv6: bool) -> Result<bool> {
    let mut command = Command::new(IP_PROGRAM);
    if ipv6 {
        command.arg("-6");
    }
    let output = command
        .args(["rule", "show"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to inspect policy routing rules")?;
    if !output.status.success() {
        anyhow::bail!("failed to inspect policy routing rules");
    }
    let rules = String::from_utf8_lossy(&output.stdout);
    let uid_range = format!("uidrange {0}-{0}", config.browser_uid);
    Ok(rules.lines().any(|line| {
        line.contains(&uid_range) && line.contains(&format!("lookup {WIREGUARD_ROUTE_TABLE}"))
    }))
}

async fn probe_proxy(endpoint: SocketAddr, dry_run: bool) -> Result<()> {
    info!(%endpoint, dry_run, "checking egress proxy listener");
    if dry_run {
        return Ok(());
    }
    timeout(Duration::from_millis(750), TcpStream::connect(endpoint))
        .await
        .context("egress proxy readiness check timed out")?
        .with_context(|| format!("egress proxy endpoint {endpoint} is unavailable"))?;
    Ok(())
}

async fn run_plan(
    plan: &[CommandSpec],
    dry_run: bool,
    allow_existing_table: bool,
    ignore_failures: bool,
) -> Result<()> {
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
                && spec.program == NFT_PROGRAM
                && spec.args == ["add", "table", "inet", "xanhtab"]
                && stderr.contains("File exists");
            if ignorable_existing_table {
                warn!(%stderr, "reusing existing XanhTab nft table before atomic replacement");
                continue;
            }
            if ignore_failures {
                warn!(
                    program = spec.program,
                    "ignoring idempotent teardown command failure"
                );
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

    #[test]
    fn production_peer_allowlist_rejects_browser_and_other_uids() {
        assert!(authorize_peer_uid(987, Some(987), 988).is_ok());
        assert!(authorize_peer_uid(988, Some(987), 988).is_err());
        assert!(authorize_peer_uid(1000, Some(987), 988).is_err());
        assert!(authorize_peer_uid(0, Some(987), 988).is_err());
    }

    #[test]
    fn development_fallback_still_rejects_browser_uid() {
        assert!(authorize_peer_uid(501, None, 988).is_ok());
        assert!(authorize_peer_uid(988, None, 988).is_err());
    }

    #[test]
    fn status_never_claims_direct_while_kill_switch_is_uncommitted() {
        let pending = status_response(&NetworkState::default());
        assert!(!pending.ok);
        assert!(pending.detail.contains("kill-switch"));

        let active = status_response(&NetworkState {
            active_mode: Some(EgressMode::Tor),
        });
        assert!(active.ok);
        assert_eq!(active.active_mode, EgressMode::Tor);
        assert!(active.proxy_url.is_none());
    }
}
