use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, ChildStdin, Command},
    sync::Mutex,
    time::timeout,
};
use url::Url;
use uuid::Uuid;

use crate::{
    error::AppError,
    model::{EgressMode, NavigationCommand, StreamProfile},
};

#[async_trait]
pub trait BrowserBackend: Send + Sync {
    async fn start(
        &self,
        session_id: Uuid,
        url: &Url,
        profile: StreamProfile,
        egress: EgressMode,
    ) -> Result<(), AppError>;
    async fn stop(&self) -> Result<(), AppError>;
    async fn navigate(&self, command: &NavigationCommand) -> Result<(), AppError>;
}

pub struct ProcessBrowser {
    command: PathBuf,
    stop_timeout: Duration,
    process: Mutex<Option<BrowserProcess>>,
}

struct BrowserProcess {
    child: Child,
    stdin: ChildStdin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum BrowserCommand {
    Start {
        session_id: Uuid,
        url: Url,
        stream_profile: StreamProfile,
        egress: EgressMode,
    },
    Navigate {
        navigation: NavigationCommand,
    },
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserResponse {
    pub ok: bool,
    pub detail: String,
}

#[derive(Clone)]
pub struct SocketBrowser {
    socket: PathBuf,
    request_timeout: Duration,
}

impl SocketBrowser {
    pub fn new(socket: PathBuf, request_timeout: Duration) -> Self {
        Self {
            socket,
            request_timeout,
        }
    }

    async fn request(&self, command: BrowserCommand) -> Result<(), AppError> {
        self.with_timeout(self.request_unbounded(command)).await
    }

    async fn with_timeout<F>(&self, request: F) -> Result<(), AppError>
    where
        F: std::future::Future<Output = Result<(), AppError>>,
    {
        timeout(self.request_timeout, request)
            .await
            .map_err(|_| AppError::ServiceUnavailable("browser helper timed out".into()))?
    }

    async fn request_unbounded(&self, command: BrowserCommand) -> Result<(), AppError> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|error| AppError::ServiceUnavailable(format!("browser connect: {error}")))?;
        let mut payload = serde_json::to_vec(&command).map_err(|_| AppError::Internal)?;
        payload.push(b'\n');
        stream
            .write_all(&payload)
            .await
            .map_err(|error| AppError::ServiceUnavailable(format!("browser request: {error}")))?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .await
            .map_err(|error| AppError::ServiceUnavailable(format!("browser response: {error}")))?;
        let response: BrowserResponse = serde_json::from_str(&line).map_err(|error| {
            AppError::ServiceUnavailable(format!("invalid browser response: {error}"))
        })?;
        if response.ok {
            Ok(())
        } else {
            Err(AppError::ServiceUnavailable(response.detail))
        }
    }
}

#[async_trait]
impl BrowserBackend for SocketBrowser {
    async fn start(
        &self,
        session_id: Uuid,
        url: &Url,
        profile: StreamProfile,
        egress: EgressMode,
    ) -> Result<(), AppError> {
        self.request(BrowserCommand::Start {
            session_id,
            url: url.clone(),
            stream_profile: profile,
            egress,
        })
        .await
    }

    async fn stop(&self) -> Result<(), AppError> {
        self.request(BrowserCommand::Stop).await
    }

    async fn navigate(&self, command: &NavigationCommand) -> Result<(), AppError> {
        self.request(BrowserCommand::Navigate {
            navigation: command.clone(),
        })
        .await
    }
}

impl ProcessBrowser {
    pub fn new(command: PathBuf, stop_timeout: Duration) -> Self {
        Self {
            command,
            stop_timeout,
            process: Mutex::new(None),
        }
    }

    async fn send<T: Serialize>(process: &mut BrowserProcess, message: &T) -> Result<(), AppError> {
        let mut payload = serde_json::to_vec(message).map_err(|_| AppError::Internal)?;
        payload.push(b'\n');
        process
            .stdin
            .write_all(&payload)
            .await
            .map_err(|error| AppError::ServiceUnavailable(format!("browser bridge: {error}")))
    }
}

#[async_trait]
impl BrowserBackend for ProcessBrowser {
    async fn start(
        &self,
        session_id: Uuid,
        url: &Url,
        profile: StreamProfile,
        egress: EgressMode,
    ) -> Result<(), AppError> {
        self.stop().await?;
        let mut child = Command::new(&self.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| AppError::ServiceUnavailable(format!("browser spawn: {error}")))?;
        let stdin = child.stdin.take().ok_or(AppError::Internal)?;
        let mut process = BrowserProcess { child, stdin };
        Self::send(
            &mut process,
            &BrowserCommand::Start {
                session_id,
                url: url.clone(),
                stream_profile: profile,
                egress,
            },
        )
        .await?;
        *self.process.lock().await = Some(process);
        Ok(())
    }

    async fn stop(&self) -> Result<(), AppError> {
        let Some(mut process) = self.process.lock().await.take() else {
            return Ok(());
        };
        let _ = Self::send(&mut process, &BrowserCommand::Stop).await;
        if timeout(self.stop_timeout, process.child.wait())
            .await
            .is_err()
        {
            process
                .child
                .start_kill()
                .map_err(|error| AppError::ServiceUnavailable(format!("browser kill: {error}")))?;
            let _ = process.child.wait().await;
        }
        Ok(())
    }

    async fn navigate(&self, command: &NavigationCommand) -> Result<(), AppError> {
        let mut guard = self.process.lock().await;
        let process = guard.as_mut().ok_or(AppError::SessionNotActive)?;
        Self::send(
            process,
            &BrowserCommand::Navigate {
                navigation: command.clone(),
            },
        )
        .await
    }
}

#[derive(Clone, Default)]
pub struct MockBrowser {
    calls: Arc<Mutex<Vec<String>>>,
}

impl MockBrowser {
    pub async fn calls(&self) -> Vec<String> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl BrowserBackend for MockBrowser {
    async fn start(
        &self,
        _session_id: Uuid,
        url: &Url,
        profile: StreamProfile,
        egress: EgressMode,
    ) -> Result<(), AppError> {
        self.calls
            .lock()
            .await
            .push(format!("start:{url}:{profile}:{egress}"));
        Ok(())
    }

    async fn stop(&self) -> Result<(), AppError> {
        self.calls.lock().await.push("stop".into());
        Ok(())
    }

    async fn navigate(&self, command: &NavigationCommand) -> Result<(), AppError> {
        self.calls
            .lock()
            .await
            .push(format!("navigate:{command:?}"));
        Ok(())
    }
}

#[cfg(test)]
mod socket_tests {
    use super::*;

    #[tokio::test]
    async fn socket_browser_times_out_when_helper_never_replies() {
        let browser = SocketBrowser::new("/unused/browser.sock".into(), Duration::from_millis(20));

        let error = browser
            .with_timeout(std::future::pending::<Result<(), AppError>>())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("browser helper timed out"));
    }
}
