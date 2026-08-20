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

use auth::{AuthContext, AuthManager};
use browser::BrowserBackend;
use config::Config;
use error::AppError;
use events::EventBus;
use metrics::MetricsCollector;
use model::{EgressMode, NavigationCommand, SessionPhase, SessionSnapshot};
use netd::EgressBackend;
use session::SessionManager;
use tokio::sync::Mutex;
use url::Url;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub auth: AuthManager,
    pub sessions: SessionManager,
    pub metrics: MetricsCollector,
    pub events: EventBus,
    pub browser: Arc<dyn BrowserBackend>,
    pub egress: Arc<dyn EgressBackend>,
    pub lifecycle: Arc<Mutex<()>>,
}

impl AppState {
    pub async fn start_session(
        &self,
        context: &AuthContext,
        url: Url,
    ) -> Result<SessionSnapshot, AppError> {
        let _guard = self.lifecycle.lock().await;
        self.auth.validate_context(context)?;
        self.sessions.start(context.client_id, url).await
    }

    pub async fn navigate(
        &self,
        context: &AuthContext,
        expected_id: Uuid,
        command: NavigationCommand,
    ) -> Result<SessionSnapshot, AppError> {
        let _guard = self.lifecycle.lock().await;
        self.auth.validate_context(context)?;
        self.require_session_id(expected_id).await?;
        self.sessions.navigate(context.client_id, command).await
    }

    pub async fn switch_egress(
        &self,
        context: &AuthContext,
        expected_id: Uuid,
        mode: EgressMode,
    ) -> Result<SessionSnapshot, AppError> {
        let _guard = self.lifecycle.lock().await;
        self.auth.validate_context(context)?;
        self.require_session_id(expected_id).await?;
        self.sessions.switch_egress(context.client_id, mode).await
    }

    pub async fn burn_controller_session(
        &self,
        context: &AuthContext,
        expected_id: Uuid,
    ) -> Result<SessionSnapshot, AppError> {
        let _guard = self.lifecycle.lock().await;
        self.auth.validate_context(context)?;
        self.require_session_id(expected_id).await?;
        self.burn_and_rotate(Some(context.client_id)).await
    }

    pub async fn auto_burn_if_due(&self) -> Result<bool, AppError> {
        let _guard = self.lifecycle.lock().await;
        let snapshot = self.sessions.snapshot().await;
        let threshold = std::time::Duration::from_secs(snapshot.auto_burn_seconds);
        if snapshot.phase != SessionPhase::Active
            || threshold.is_zero()
            || self.sessions.idle_for().await < threshold
        {
            return Ok(false);
        }
        self.burn_and_rotate(None).await?;
        Ok(true)
    }

    /// Recovers a consumed one-time pairing after its controller cookie
    /// expires. Any still-running browser session is burned before new
    /// pairing material is published.
    pub async fn recover_expired_auth(&self) -> Result<bool, AppError> {
        let _guard = self.lifecycle.lock().await;
        if !self.auth.pairing_recovery_required() {
            return Ok(false);
        }

        let snapshot = self.sessions.snapshot().await;
        let burn_result = if snapshot.phase == SessionPhase::Idle {
            Ok(snapshot)
        } else {
            self.sessions.force_burn().await
        };
        self.auth.revoke_all();
        let pairing_result = self.rotate_pairing_material();
        burn_result?;
        pairing_result?;
        Ok(true)
    }

    async fn burn_and_rotate(&self, controller: Option<Uuid>) -> Result<SessionSnapshot, AppError> {
        let burn_result = match controller {
            Some(client_id) => self.sessions.burn(client_id).await,
            None => self.sessions.force_burn().await,
        };
        self.auth.revoke_all();
        let pairing_result = self.rotate_pairing_material();
        let snapshot = burn_result?;
        pairing_result?;
        Ok(snapshot)
    }

    fn rotate_pairing_material(&self) -> Result<(), AppError> {
        let pairing = self.auth.rotate_pairing().map_err(|_| AppError::Internal)?;
        if self
            .auth
            .write_pairing_file(
                &pairing,
                &self.config.session.pairing_file,
                &self.config.server.public_base_url,
            )
            .is_err()
        {
            self.auth.invalidate_unpublished_pairing();
            return Err(AppError::Internal);
        }
        Ok(())
    }

    async fn require_session_id(&self, expected_id: Uuid) -> Result<(), AppError> {
        if self.sessions.snapshot().await.id == Some(expected_id) {
            Ok(())
        } else {
            Err(AppError::NotFound)
        }
    }
}
