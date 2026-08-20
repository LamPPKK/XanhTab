use std::{fs::OpenOptions, io::Write, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use xanhtab::{
    AppState, api,
    auth::AuthManager,
    blocklist::Blocklist,
    browser::{BrowserBackend, MockBrowser, SocketBrowser},
    config::Config,
    events::EventBus,
    metrics::MetricsCollector,
    netd::{EgressBackend, MockEgress, SocketEgress},
    remote_config::{apply_public_config_file, validate_public_config_dir},
    session::SessionManager,
};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, env = "XANHTAB_CONFIG", default_value = "config/xanhtab.toml")]
    config: PathBuf,
    /// Validate configuration and exit without starting any service.
    #[arg(long)]
    check_config: bool,
    /// Validate a Git-backed non-secret configuration directory and exit.
    #[arg(long, value_name = "DIR", conflicts_with = "check_config")]
    check_public_config_dir: Option<PathBuf>,
    /// Merge reviewed non-secret session defaults and write a new TOML file.
    #[arg(
        long,
        value_name = "FILE",
        requires = "write_config",
        conflicts_with_all = ["check_config", "check_public_config_dir"]
    )]
    apply_public_config: Option<PathBuf>,
    /// New output path used only with --apply-public-config; it must not exist.
    #[arg(long, value_name = "FILE", requires = "apply_public_config")]
    write_config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "xanhtab=info,tower_http=info".into()),
        )
        .compact()
        .init();

    let args = Args::parse();
    if let Some(directory) = &args.check_public_config_dir {
        validate_public_config_dir(directory)?;
        info!(directory = %directory.display(), "public configuration is valid");
        return Ok(());
    }
    let mut config = Config::load(&args.config)?;
    if let Some(public_config) = &args.apply_public_config {
        apply_public_config_file(&mut config, public_config)?;
        let output = args
            .write_config
            .as_ref()
            .context("--write-config is required with --apply-public-config")?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)
            .with_context(|| format!("failed to create merged config {}", output.display()))?;
        file.write_all(toml::to_string_pretty(&config)?.as_bytes())?;
        file.sync_all()?;
        info!(output = %output.display(), "merged public configuration is valid");
        return Ok(());
    }
    if args.check_config {
        info!(config = %args.config.display(), "configuration is valid");
        return Ok(());
    }
    config.server.static_dir = api::resolve_static_dir(&config.server.static_dir);
    let config = Arc::new(config);

    let blocklist = Blocklist::open(&config.blocklist.fst_path)?;
    let metrics = MetricsCollector::new(blocklist);
    let events = EventBus::new(128);
    let browser: Arc<dyn BrowserBackend> = if config.browser.enabled {
        Arc::new(SocketBrowser::new(
            config.browser.socket.clone(),
            Duration::from_secs(config.browser.ipc_timeout_seconds),
        ))
    } else {
        warn!("browser backend is disabled; using deterministic mock backend");
        Arc::new(MockBrowser::default())
    };
    let egress: Arc<dyn EgressBackend> = if config.network.enabled {
        Arc::new(SocketEgress::new(
            config.network.netd_socket.clone(),
            Duration::from_secs(config.network.ipc_timeout_seconds),
        ))
    } else {
        warn!("network helper is disabled; using deterministic mock backend");
        Arc::new(MockEgress::default())
    };
    let sessions = SessionManager::new(
        events.clone(),
        browser.clone(),
        egress.clone(),
        config.session.runtime_dir.clone(),
        config.network.initial_mode,
        config.session.initial_profile,
        config.session.auto_burn_seconds,
    );
    let auth = AuthManager::new(config.auth_ttl(), config.ticket_ttl());
    let pairing = auth.rotate_pairing()?;
    auth.write_pairing_file(
        &pairing,
        &config.session.pairing_file,
        &config.server.public_base_url,
    )?;
    info!(pairing_file = %config.session.pairing_file.display(), "new zero-account pairing material generated");

    let state = AppState {
        config: config.clone(),
        auth,
        sessions,
        metrics,
        events,
        browser,
        egress,
        lifecycle: Arc::new(tokio::sync::Mutex::new(())),
    };
    spawn_lifecycle_watchdog(state.clone());
    let app = api::router(state);
    let listen = config.server.listen;
    info!(%listen, "xanhtabd listening");

    match (&config.server.tls_cert, &config.server.tls_key) {
        (Some(cert), Some(key)) => {
            let tls = RustlsConfig::from_pem_file(cert, key)
                .await
                .context("failed to load TLS certificate")?;
            axum_server::bind_rustls(listen, tls)
                .serve(app.into_make_service())
                .await?;
        }
        _ if config.server.allow_insecure_http => {
            axum_server::bind(listen)
                .serve(app.into_make_service())
                .await?;
        }
        _ => anyhow::bail!("TLS is required by configuration"),
    }
    Ok(())
}

fn spawn_lifecycle_watchdog(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match state.recover_expired_auth().await {
                Ok(true) => warn!("expired controller auth recovered with Burn and fresh pairing"),
                Ok(false) => {}
                Err(error) => {
                    warn!(%error, "expired controller auth recovery did not complete cleanly")
                }
            }

            match state.auto_burn_if_due().await {
                Ok(true) => warn!("auto-burn inactivity threshold reached; session destroyed"),
                Ok(false) => {}
                Err(error) => warn!(%error, "auto-burn lifecycle did not complete cleanly"),
            }
        }
    });
}
