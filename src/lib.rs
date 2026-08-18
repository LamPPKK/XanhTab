pub mod api;
pub mod auth;
pub mod blocklist;
pub mod browser;
pub mod config;
pub mod error;
pub mod events;
pub mod metrics;
pub mod model;
pub mod netd;
pub mod session;

use std::sync::Arc;

use auth::AuthManager;
use browser::BrowserBackend;
use config::Config;
use events::EventBus;
use metrics::MetricsCollector;
use netd::EgressBackend;
use session::SessionManager;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub auth: AuthManager,
    pub sessions: SessionManager,
    pub metrics: MetricsCollector,
    pub events: EventBus,
    pub browser: Arc<dyn BrowserBackend>,
    pub egress: Arc<dyn EgressBackend>,
}
