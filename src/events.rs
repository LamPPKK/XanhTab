use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::model::SessionSnapshot;

#[derive(Debug, Clone, Serialize)]
pub struct ServerEvent {
    pub event: &'static str,
    pub at: DateTime<Utc>,
    pub session: SessionSnapshot,
}

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<ServerEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn publish(&self, event: &'static str, session: SessionSnapshot) {
        let _ = self.sender.send(ServerEvent {
            event,
            at: Utc::now(),
            session,
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.sender.subscribe()
    }
}
