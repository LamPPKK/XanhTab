use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::Mutex,
    time::timeout,
};

use crate::{config::NetworkConfig, error::AppError, model::EgressMode};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EgressRequest {
    pub version: u8,
    pub command: EgressCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum EgressCommand {
    Apply { mode: EgressMode },
    Reset,
    Status,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressResponse {
    pub ok: bool,
    pub active_mode: EgressMode,
    pub proxy_url: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: &'static str,
    pub args: Vec<String>,
    pub stdin: Option<String>,
}

#[async_trait]
pub trait EgressBackend: Send + Sync {
    async fn apply(&self, mode: EgressMode) -> Result<EgressResponse, AppError>;
    async fn reset(&self) -> Result<EgressResponse, AppError>;
}

pub struct SocketEgress {
    socket: PathBuf,
    request_timeout: Duration,
}

impl SocketEgress {
    pub fn new(socket: PathBuf, request_timeout: Duration) -> Self {
        Self {
            socket,
            request_timeout,
        }
    }

    async fn request(&self, command: EgressCommand) -> Result<EgressResponse, AppError> {
        self.with_timeout(self.request_unbounded(command)).await
    }

    async fn with_timeout<F>(&self, request: F) -> Result<EgressResponse, AppError>
    where
        F: std::future::Future<Output = Result<EgressResponse, AppError>>,
    {
        timeout(self.request_timeout, request)
            .await
            .map_err(|_| AppError::ServiceUnavailable("network helper timed out".into()))?
    }

    async fn request_unbounded(&self, command: EgressCommand) -> Result<EgressResponse, AppError> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|error| AppError::ServiceUnavailable(format!("netd connect: {error}")))?;
        let mut payload = serde_json::to_vec(&EgressRequest {
            version: 1,
            command,
        })
        .map_err(|_| AppError::Internal)?;
        payload.push(b'\n');
        stream
            .write_all(&payload)
            .await
            .map_err(|error| AppError::ServiceUnavailable(format!("netd write: {error}")))?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .await
            .map_err(|error| AppError::ServiceUnavailable(format!("netd read: {error}")))?;
        let response: EgressResponse = serde_json::from_str(&line)
            .map_err(|error| AppError::ServiceUnavailable(format!("netd response: {error}")))?;
        if !response.ok {
            return Err(AppError::ServiceUnavailable(response.detail));
        }
        Ok(response)
    }
}

#[async_trait]
impl EgressBackend for SocketEgress {
    async fn apply(&self, mode: EgressMode) -> Result<EgressResponse, AppError> {
        self.request(EgressCommand::Apply { mode }).await
    }

    async fn reset(&self) -> Result<EgressResponse, AppError> {
        self.request(EgressCommand::Reset).await
    }
}

#[derive(Clone)]
pub struct MockEgress {
    active: Arc<Mutex<EgressMode>>,
}

impl Default for MockEgress {
    fn default() -> Self {
        Self {
            active: Arc::new(Mutex::new(EgressMode::Direct)),
        }
    }
}

#[async_trait]
impl EgressBackend for MockEgress {
    async fn apply(&self, mode: EgressMode) -> Result<EgressResponse, AppError> {
        *self.active.lock().await = mode;
        Ok(EgressResponse {
            ok: true,
            active_mode: mode,
            proxy_url: None,
            detail: "mock egress applied".into(),
        })
    }

    async fn reset(&self) -> Result<EgressResponse, AppError> {
        self.apply(EgressMode::Direct).await
    }
}

pub fn command_plan(config: &NetworkConfig, mode: EgressMode) -> Vec<CommandSpec> {
    let mut plan = vec![CommandSpec {
        program: "nft",
        args: vec![
            "add".into(),
            "table".into(),
            "inet".into(),
            "xanhtab".into(),
        ],
        stdin: None,
    }];
    match mode {
        EgressMode::Direct | EgressMode::Tor | EgressMode::Warp | EgressMode::Proxy => {}
        EgressMode::WireGuard => {
            plan.push(CommandSpec {
                program: "wg-quick",
                args: vec!["up".into(), config.wireguard_config.display().to_string()],
                stdin: None,
            });
        }
    }
    plan.push(nft_transaction(config, mode));
    plan
}

fn nft_transaction(config: &NetworkConfig, mode: EgressMode) -> CommandSpec {
    let uid = config.browser_uid;
    let mut script = String::from(
        "flush table inet xanhtab\n\
         add chain inet xanhtab browser_output { type filter hook output priority filter; policy accept; }\n",
    );
    match mode {
        EgressMode::Direct => {}
        EgressMode::Tor => add_proxy_guard(&mut script, uid, "127.0.0.1", 9050),
        EgressMode::Warp => add_proxy_guard(&mut script, uid, "127.0.0.1", 40000),
        EgressMode::Proxy => {
            let endpoint = config
                .proxy_endpoint
                .parse::<std::net::SocketAddr>()
                .expect("validated proxy endpoint");
            add_proxy_guard(
                &mut script,
                uid,
                &endpoint.ip().to_string(),
                endpoint.port(),
            );
        }
        EgressMode::WireGuard => {
            script.push_str(&format!(
                "add rule inet xanhtab browser_output meta skuid {uid} oifname \"wg0\" accept\n"
            ));
            add_drop(&mut script, uid);
        }
    }
    CommandSpec {
        program: "nft",
        args: vec!["-f".into(), "-".into()],
        stdin: Some(script),
    }
}

fn add_proxy_guard(script: &mut String, uid: u32, address: &str, port: u16) {
    let family = if address.contains(':') { "ip6" } else { "ip" };
    script.push_str(&format!(
        "add rule inet xanhtab browser_output meta skuid {uid} {family} daddr {address} tcp dport {port} accept\n"
    ));
    add_drop(script, uid);
}

fn add_drop(script: &mut String, uid: u32) {
    script.push_str(&format!(
        "add rule inet xanhtab browser_output meta skuid {uid} counter drop\n"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socket_egress_times_out_when_helper_never_replies() {
        let egress = SocketEgress::new("/unused/netd.sock".into(), Duration::from_millis(20));

        let error = egress
            .with_timeout(std::future::pending::<Result<EgressResponse, AppError>>())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("network helper timed out"));
    }

    #[test]
    fn egress_plan_only_uses_allowlisted_programs_without_a_shell() {
        let config = NetworkConfig::default();
        for mode in [
            EgressMode::Direct,
            EgressMode::Tor,
            EgressMode::Warp,
            EgressMode::WireGuard,
            EgressMode::Proxy,
        ] {
            for command in command_plan(&config, mode) {
                assert!(["nft", "wg-quick"].contains(&command.program));
                assert_ne!(command.program, "sh");
                assert_ne!(command.program, "bash");
            }
        }
    }

    #[test]
    fn firewall_policy_is_one_nft_transaction() {
        let config = NetworkConfig::default();
        let plan = command_plan(&config, EgressMode::Tor);
        let nft = plan
            .iter()
            .filter(|command| command.program == "nft" && command.stdin.is_some())
            .collect::<Vec<_>>();
        assert_eq!(nft.len(), 1);
        let script = nft[0].stdin.as_ref().unwrap();
        assert!(script.contains("flush table inet xanhtab"));
        assert!(script.contains("tcp dport 9050 accept"));
        assert!(script.contains("counter drop"));
    }

    #[test]
    fn wireguard_path_is_one_argument() {
        let config = NetworkConfig {
            wireguard_config: "/root/a config.conf".into(),
            ..NetworkConfig::default()
        };
        let plan = command_plan(&config, EgressMode::WireGuard);
        let command = plan
            .iter()
            .find(|command| command.program == "wg-quick")
            .unwrap();
        assert_eq!(command.args[1], "/root/a config.conf");
    }
}
