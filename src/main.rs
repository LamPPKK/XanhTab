use std::{path::PathBuf, sync::Arc, time::Duration};

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
    let mut config = Config::load(&args.config)?;
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
        Arc::new(SocketBrowser::new(config.browser.socket.clone()))
    } else {
        warn!("browser backend is disabled; using deterministic mock backend");
        Arc::new(MockBrowser::default())
    };
    let egress: Arc<dyn EgressBackend> = if config.network.enabled {
        Arc::new(SocketEgress::new(config.network.netd_socket.clone()))
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
    };
    spawn_auto_burn(state.clone());
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

fn spawn_auto_burn(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let snapshot = state.sessions.snapshot().await;
            let threshold = Duration::from_secs(snapshot.auto_burn_seconds);
            if snapshot.phase != xanhtab::model::SessionPhase::Active
                || threshold.is_zero()
                || state.sessions.idle_for().await < threshold
            {
                continue;
            }
            warn!(session_id = ?snapshot.id, "auto-burn inactivity threshold reached");
            let burn_result = state.sessions.force_burn().await;
            state.auth.revoke_all();
            match state.auth.rotate_pairing().and_then(|pairing| {
                state.auth.write_pairing_file(
                    &pairing,
                    &state.config.session.pairing_file,
                    &state.config.server.public_base_url,
                )
            }) {
                Ok(()) => {}
                Err(error) => warn!(%error, "failed to rotate pairing material after auto-burn"),
            }
            if let Err(error) = burn_result {
                warn!(%error, "auto-burn cleanup did not complete cleanly");
            }
        }
    });
}
