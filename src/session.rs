use std::{
    collections::VecDeque,
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use serde::Serialize;
use tokio::sync::RwLock;
use url::Url;
use uuid::Uuid;

use crate::{
    browser::BrowserBackend,
    error::AppError,
    events::EventBus,
    model::{EgressMode, NavigationCommand, SessionPhase, SessionSnapshot, StreamProfile},
    netd::EgressBackend,
};

#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub url: Url,
    pub visited_at: chrono::DateTime<Utc>,
}

struct SessionData {
    snapshot: SessionSnapshot,
    controller: Option<Uuid>,
    history: VecDeque<HistoryEntry>,
    last_activity: Instant,
}

#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<RwLock<SessionData>>,
    events: EventBus,
    browser: Arc<dyn BrowserBackend>,
    egress: Arc<dyn EgressBackend>,
    runtime_dir: Arc<std::path::PathBuf>,
    initial_egress: EgressMode,
    initial_profile: StreamProfile,
    initial_auto_burn_seconds: u64,
}

impl SessionManager {
    pub fn new(
        events: EventBus,
        browser: Arc<dyn BrowserBackend>,
        egress: Arc<dyn EgressBackend>,
        runtime_dir: std::path::PathBuf,
        initial_egress: EgressMode,
        initial_profile: StreamProfile,
        initial_auto_burn_seconds: u64,
    ) -> Self {
        let snapshot = SessionSnapshot {
            egress: initial_egress,
            stream_profile: initial_profile,
            auto_burn_seconds: initial_auto_burn_seconds,
            ..SessionSnapshot::default()
        };
        Self {
            inner: Arc::new(RwLock::new(SessionData {
                snapshot,
                controller: None,
                history: VecDeque::with_capacity(128),
                last_activity: Instant::now(),
            })),
            events,
            browser,
            egress,
            runtime_dir: Arc::new(runtime_dir),
            initial_egress,
            initial_profile,
            initial_auto_burn_seconds,
        }
    }

    pub async fn snapshot(&self) -> SessionSnapshot {
        self.inner.read().await.snapshot.clone()
    }

    pub async fn history(&self) -> Vec<HistoryEntry> {
        self.inner.read().await.history.iter().cloned().collect()
    }

    pub async fn start(&self, client_id: Uuid, url: Url) -> Result<SessionSnapshot, AppError> {
        let (id, profile, egress) = {
            let mut data = self.inner.write().await;
            if !matches!(
                data.snapshot.phase,
                SessionPhase::Idle | SessionPhase::Failed
            ) {
                return Err(AppError::InvalidTransition);
            }
            acquire_lease(&mut data, client_id)?;
            let id = Uuid::new_v4();
            data.snapshot.id = Some(id);
            data.snapshot.phase = SessionPhase::Starting;
            data.snapshot.url = Some(url.clone());
            data.snapshot.started_at = Some(Utc::now());
            data.snapshot.failure = None;
            data.snapshot.controller_attached = true;
            data.last_activity = Instant::now();
            (id, data.snapshot.stream_profile, data.snapshot.egress)
        };
        self.publish("session.starting").await;

        if let Err(error) = self.egress.apply(egress).await {
            return self.fail(error.to_string()).await;
        }
        if let Err(error) = self.browser.start(id, &url, profile, egress).await {
            let _ = self.egress.reset().await;
            return self.fail(error.to_string()).await;
        }

        let mut data = self.inner.write().await;
        data.snapshot.phase = SessionPhase::Active;
        push_history(&mut data.history, url);
        let snapshot = data.snapshot.clone();
        drop(data);
        self.events.publish("session.active", snapshot.clone());
        Ok(snapshot)
    }

    pub async fn burn(&self, client_id: Uuid) -> Result<SessionSnapshot, AppError> {
        {
            let data = self.inner.read().await;
            ensure_lease(&data, client_id)?;
        }
        self.burn_unchecked().await
    }

    /// Burns an abandoned session without requiring a browser-held lease.
    /// This is reserved for the daemon's inactivity watchdog and shutdown path.
    pub async fn force_burn(&self) -> Result<SessionSnapshot, AppError> {
        self.burn_unchecked().await
    }

    async fn burn_unchecked(&self) -> Result<SessionSnapshot, AppError> {
        {
            let mut data = self.inner.write().await;
            if data.snapshot.phase == SessionPhase::Idle {
                return Ok(data.snapshot.clone());
            }
            data.snapshot.phase = SessionPhase::Burning;
        }
        self.publish("session.burning").await;

        let mut failures = Vec::new();
        if let Err(error) = self.browser.stop().await {
            failures.push(format!("browser stop: {error}"));
        }
        if let Err(error) = self.egress.reset().await {
            failures.push(format!("egress reset: {error}"));
        }
        if let Err(error) = clear_runtime_dir(&self.runtime_dir) {
            failures.push(format!("session cleanup: {error}"));
        }

        let mut data = self.inner.write().await;
        data.snapshot = SessionSnapshot {
            egress: self.initial_egress,
            stream_profile: self.initial_profile,
            auto_burn_seconds: self.initial_auto_burn_seconds,
            ..SessionSnapshot::default()
        };
        data.controller = None;
        data.history.clear();
        data.last_activity = Instant::now();
        if !failures.is_empty() {
            let message = failures.join("; ");
            data.snapshot.phase = SessionPhase::Failed;
            data.snapshot.failure = Some(message.clone());
            let snapshot = data.snapshot.clone();
            drop(data);
            self.events.publish("session.burn_failed", snapshot);
            return Err(AppError::ServiceUnavailable(message));
        }
        let snapshot = data.snapshot.clone();
        drop(data);
        self.events.publish("session.idle", snapshot.clone());
        Ok(snapshot)
    }

    pub async fn navigate(
        &self,
        client_id: Uuid,
        command: NavigationCommand,
    ) -> Result<SessionSnapshot, AppError> {
        {
            let mut data = self.inner.write().await;
            ensure_active_lease(&data, client_id)?;
            data.last_activity = Instant::now();
        }
        self.browser.navigate(&command).await?;
        let mut data = self.inner.write().await;
        if let NavigationCommand::Navigate { url } = command {
            data.snapshot.url = Some(url.clone());
            push_history(&mut data.history, url);
        }
        let snapshot = data.snapshot.clone();
        drop(data);
        self.events.publish("session.navigation", snapshot.clone());
        Ok(snapshot)
    }

    pub async fn set_stream_profile(
        &self,
        client_id: Uuid,
        profile: StreamProfile,
    ) -> Result<SessionSnapshot, AppError> {
        let mut data = self.inner.write().await;
        ensure_active_lease(&data, client_id)?;
        data.snapshot.stream_profile = profile;
        data.last_activity = Instant::now();
        let snapshot = data.snapshot.clone();
        drop(data);
        self.events.publish("stream.profile", snapshot.clone());
        Ok(snapshot)
    }

    pub async fn set_blocklist_enabled(
        &self,
        client_id: Uuid,
        enabled: bool,
    ) -> Result<SessionSnapshot, AppError> {
        let mut data = self.inner.write().await;
        ensure_active_lease(&data, client_id)?;
        data.snapshot.blocklist_enabled = enabled;
        data.last_activity = Instant::now();
        let snapshot = data.snapshot.clone();
        drop(data);
        self.events.publish("blocklist.setting", snapshot.clone());
        Ok(snapshot)
    }

    pub async fn set_auto_burn_seconds(
        &self,
        client_id: Uuid,
        seconds: u64,
    ) -> Result<SessionSnapshot, AppError> {
        if seconds > 86_400 {
            return Err(AppError::InvalidRequest(
                "auto-burn must be zero or no more than 86400 seconds".into(),
            ));
        }
        let mut data = self.inner.write().await;
        ensure_active_lease(&data, client_id)?;
        data.snapshot.auto_burn_seconds = seconds;
        data.last_activity = Instant::now();
        let snapshot = data.snapshot.clone();
        drop(data);
        self.events.publish("auto_burn.setting", snapshot.clone());
        Ok(snapshot)
    }

    pub async fn ensure_controller(&self, client_id: Uuid) -> Result<(), AppError> {
        let data = self.inner.read().await;
        ensure_active_lease(&data, client_id)
    }

    pub async fn switch_egress(
        &self,
        client_id: Uuid,
        mode: EgressMode,
    ) -> Result<SessionSnapshot, AppError> {
        let (url, profile, previous_mode) = {
            let mut data = self.inner.write().await;
            ensure_active_lease(&data, client_id)?;
            data.snapshot.phase = SessionPhase::Burning;
            (
                data.snapshot
                    .url
                    .clone()
                    .ok_or(AppError::SessionNotActive)?,
                data.snapshot.stream_profile,
                data.snapshot.egress,
            )
        };
        self.publish("egress.switching").await;
        let new_id = Uuid::new_v4();
        let switch_result: Result<(), AppError> = async {
            self.browser.stop().await?;
            self.egress.reset().await?;
            clear_runtime_dir(&self.runtime_dir).map_err(|error| {
                AppError::ServiceUnavailable(format!("session cleanup: {error}"))
            })?;
            self.egress.apply(mode).await?;
            self.browser.start(new_id, &url, profile, mode).await
        }
        .await;

        if let Err(switch_error) = switch_result {
            let rollback_id = Uuid::new_v4();
            let rollback_result: Result<(), AppError> = async {
                let _ = self.browser.stop().await;
                self.egress.reset().await?;
                clear_runtime_dir(&self.runtime_dir).map_err(|error| {
                    AppError::ServiceUnavailable(format!("rollback cleanup: {error}"))
                })?;
                self.egress.apply(previous_mode).await?;
                self.browser
                    .start(rollback_id, &url, profile, previous_mode)
                    .await
            }
            .await;
            let mut data = self.inner.write().await;
            data.history.clear();
            if let Err(rollback_error) = rollback_result {
                let message = format!(
                    "egress switch failed: {switch_error}; rollback failed: {rollback_error}"
                );
                data.snapshot.phase = SessionPhase::Failed;
                data.snapshot.failure = Some(message.clone());
                let snapshot = data.snapshot.clone();
                drop(data);
                self.events.publish("egress.failed", snapshot);
                return Err(AppError::ServiceUnavailable(message));
            }
            data.snapshot.id = Some(rollback_id);
            data.snapshot.phase = SessionPhase::Active;
            data.snapshot.egress = previous_mode;
            data.snapshot.started_at = Some(Utc::now());
            data.snapshot.failure = None;
            push_history(&mut data.history, url);
            data.last_activity = Instant::now();
            let snapshot = data.snapshot.clone();
            drop(data);
            self.events.publish("egress.rolled_back", snapshot);
            return Err(AppError::ServiceUnavailable(format!(
                "egress switch failed and was rolled back: {switch_error}"
            )));
        }

        let mut data = self.inner.write().await;
        data.snapshot.id = Some(new_id);
        data.snapshot.egress = mode;
        data.snapshot.phase = SessionPhase::Active;
        data.snapshot.started_at = Some(Utc::now());
        data.history.clear();
        push_history(&mut data.history, url);
        data.last_activity = Instant::now();
        let snapshot = data.snapshot.clone();
        drop(data);
        self.events.publish("egress.active", snapshot.clone());
        Ok(snapshot)
    }

    pub async fn idle_for(&self) -> Duration {
        self.inner.read().await.last_activity.elapsed()
    }

    async fn publish(&self, event: &'static str) {
        self.events.publish(event, self.snapshot().await);
    }

    async fn fail<T>(&self, message: String) -> Result<T, AppError> {
        let mut data = self.inner.write().await;
        data.snapshot.phase = SessionPhase::Failed;
        data.snapshot.failure = Some(message.clone());
        let snapshot = data.snapshot.clone();
        drop(data);
        self.events.publish("session.failed", snapshot);
        Err(AppError::ServiceUnavailable(message))
    }
}

fn acquire_lease(data: &mut SessionData, client_id: Uuid) -> Result<(), AppError> {
    if data.controller.is_some_and(|current| current != client_id) {
        return Err(AppError::LeaseConflict);
    }
    data.controller = Some(client_id);
    Ok(())
}

fn ensure_lease(data: &SessionData, client_id: Uuid) -> Result<(), AppError> {
    if data.controller == Some(client_id) {
        Ok(())
    } else {
        Err(AppError::LeaseConflict)
    }
}

fn ensure_active_lease(data: &SessionData, client_id: Uuid) -> Result<(), AppError> {
    ensure_lease(data, client_id)?;
    if data.snapshot.phase != SessionPhase::Active {
        return Err(AppError::SessionNotActive);
    }
    Ok(())
}

fn push_history(history: &mut VecDeque<HistoryEntry>, url: Url) {
    if history.len() == 128 {
        history.pop_front();
    }
    history.push_back(HistoryEntry {
        url,
        visited_at: Utc::now(),
    });
}

fn clear_runtime_dir(path: &Path) -> anyhow::Result<()> {
    if path == Path::new("/") || path.components().count() < 2 {
        anyhow::bail!("refusing to clear unsafe runtime path {}", path.display());
    }
    fs::create_dir_all(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::{browser::MockBrowser, netd::MockEgress};

    use super::*;

    #[tokio::test]
    async fn lifecycle_enforces_lease_and_clears_runtime() {
        let runtime = tempdir().unwrap();
        fs::write(runtime.path().join("cookie.db"), "secret").unwrap();
        let browser = Arc::new(MockBrowser::default());
        let manager = SessionManager::new(
            EventBus::new(16),
            browser.clone(),
            Arc::new(MockEgress::default()),
            runtime.path().to_path_buf(),
            EgressMode::Direct,
            StreamProfile::FullHd30,
            1_800,
        );
        let owner = Uuid::new_v4();
        manager
            .start(owner, Url::parse("https://example.com").unwrap())
            .await
            .unwrap();
        assert!(
            manager
                .navigate(Uuid::new_v4(), NavigationCommand::Reload)
                .await
                .is_err()
        );
        manager.burn(owner).await.unwrap();
        assert_eq!(manager.snapshot().await.phase, SessionPhase::Idle);
        assert_eq!(fs::read_dir(runtime.path()).unwrap().count(), 0);
        assert_eq!(
            browser.calls().await,
            vec!["start:https://example.com/:1080p30:direct", "stop"]
        );
    }

    #[tokio::test]
    async fn egress_switch_restarts_browser_transactionally() {
        let runtime = tempdir().unwrap();
        let browser = Arc::new(MockBrowser::default());
        let manager = SessionManager::new(
            EventBus::new(16),
            browser.clone(),
            Arc::new(MockEgress::default()),
            runtime.path().to_path_buf(),
            EgressMode::Direct,
            StreamProfile::Hd15,
            1_800,
        );
        let owner = Uuid::new_v4();
        let original = manager
            .start(owner, Url::parse("https://example.com").unwrap())
            .await
            .unwrap();
        let snapshot = manager.switch_egress(owner, EgressMode::Tor).await.unwrap();
        assert_ne!(snapshot.id, original.id);
        assert_eq!(snapshot.egress, EgressMode::Tor);
        assert_eq!(snapshot.phase, SessionPhase::Active);
        assert_eq!(manager.history().await.len(), 1);
        assert_eq!(browser.calls().await.len(), 3);
    }

    #[tokio::test]
    async fn session_policy_changes_are_lease_bound_and_reset_on_burn() {
        let runtime = tempdir().unwrap();
        let manager = SessionManager::new(
            EventBus::new(16),
            Arc::new(MockBrowser::default()),
            Arc::new(MockEgress::default()),
            runtime.path().to_path_buf(),
            EgressMode::Direct,
            StreamProfile::Hd15,
            1_800,
        );
        let owner = Uuid::new_v4();
        manager
            .start(owner, Url::parse("https://example.com").unwrap())
            .await
            .unwrap();
        assert!(
            manager
                .set_blocklist_enabled(Uuid::new_v4(), false)
                .await
                .is_err()
        );
        manager.set_blocklist_enabled(owner, false).await.unwrap();
        let changed = manager.set_auto_burn_seconds(owner, 300).await.unwrap();
        assert!(!changed.blocklist_enabled);
        assert_eq!(changed.auto_burn_seconds, 300);
        let burned = manager.burn(owner).await.unwrap();
        assert!(burned.blocklist_enabled);
        assert_eq!(burned.auto_burn_seconds, 1_800);
    }

    #[derive(Default)]
    struct RejectTor;

    #[async_trait::async_trait]
    impl EgressBackend for RejectTor {
        async fn apply(&self, mode: EgressMode) -> Result<crate::netd::EgressResponse, AppError> {
            if mode == EgressMode::Tor {
                Err(AppError::ServiceUnavailable("probe failed".into()))
            } else {
                Ok(crate::netd::EgressResponse {
                    ok: true,
                    active_mode: mode,
                    proxy_url: None,
                    detail: "applied".into(),
                })
            }
        }

        async fn reset(&self) -> Result<crate::netd::EgressResponse, AppError> {
            self.apply(EgressMode::Direct).await
        }
    }

    #[tokio::test]
    async fn failed_egress_switch_rolls_back_to_a_fresh_session() {
        let runtime = tempdir().unwrap();
        let manager = SessionManager::new(
            EventBus::new(16),
            Arc::new(MockBrowser::default()),
            Arc::new(RejectTor),
            runtime.path().to_path_buf(),
            EgressMode::Direct,
            StreamProfile::Hd15,
            1_800,
        );
        let owner = Uuid::new_v4();
        let original = manager
            .start(owner, Url::parse("https://example.com").unwrap())
            .await
            .unwrap();
        assert!(manager.switch_egress(owner, EgressMode::Tor).await.is_err());
        let rolled_back = manager.snapshot().await;
        assert_eq!(rolled_back.phase, SessionPhase::Active);
        assert_eq!(rolled_back.egress, EgressMode::Direct);
        assert_ne!(rolled_back.id, original.id);
    }
}
