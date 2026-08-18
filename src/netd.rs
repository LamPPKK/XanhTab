use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::Mutex,
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
}

#[async_trait]
pub trait EgressBackend: Send + Sync {
    async fn apply(&self, mode: EgressMode) -> Result<EgressResponse, AppError>;
    async fn reset(&self) -> Result<EgressResponse, AppError>;
}

pub struct SocketEgress {
    socket: PathBuf,
}

impl SocketEgress {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    async fn request(&self, command: EgressCommand) -> Result<EgressResponse, AppError> {
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
    let uid = config.browser_uid.to_string();
    let mut plan = reset_firewall_plan();
    match mode {
        EgressMode::Direct => {}
        EgressMode::Tor => plan.extend(proxy_guard(uid, "127.0.0.1", 9050)),
        EgressMode::Warp => plan.extend(proxy_guard(uid, "127.0.0.1", 40000)),
        EgressMode::WireGuard => {
            plan.push(CommandSpec {
                program: "wg-quick",
                args: vec!["up".into(), config.wireguard_config.display().to_string()],
            });
            plan.extend(interface_guard(uid, "wg0"));
        }
        EgressMode::Proxy => {
            let endpoint = config
                .proxy_endpoint
                .parse::<std::net::SocketAddr>()
                .expect("validated proxy endpoint");
            plan.extend(proxy_guard(
                uid,
                &endpoint.ip().to_string(),
                endpoint.port(),
            ));
        }
    }
    plan
}

pub fn reset_firewall_plan() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            program: "nft",
            args: vec![
                "delete".into(),
                "table".into(),
                "inet".into(),
                "xanhtab".into(),
            ],
        },
        CommandSpec {
            program: "nft",
            args: vec![
                "add".into(),
                "table".into(),
                "inet".into(),
                "xanhtab".into(),
            ],
        },
        CommandSpec {
            program: "nft",
            args: vec![
                "add".into(),
                "chain".into(),
                "inet".into(),
                "xanhtab".into(),
                "browser_output".into(),
                "{ type filter hook output priority filter; policy accept; }".into(),
            ],
        },
    ]
}

fn proxy_guard(uid: String, address: &str, port: u16) -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            program: "nft",
            args: vec![
                "add".into(),
                "rule".into(),
                "inet".into(),
                "xanhtab".into(),
                "browser_output".into(),
                "meta".into(),
                "skuid".into(),
                uid.clone(),
                "ip".into(),
                "daddr".into(),
                address.into(),
                "tcp".into(),
                "dport".into(),
                port.to_string(),
                "accept".into(),
            ],
        },
        browser_drop(uid),
    ]
}

fn interface_guard(uid: String, interface: &str) -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            program: "nft",
            args: vec![
                "add".into(),
                "rule".into(),
                "inet".into(),
                "xanhtab".into(),
                "browser_output".into(),
                "meta".into(),
                "skuid".into(),
                uid.clone(),
                "oifname".into(),
                interface.into(),
                "accept".into(),
            ],
        },
        browser_drop(uid),
    ]
}

fn browser_drop(uid: String) -> CommandSpec {
    CommandSpec {
        program: "nft",
        args: vec![
            "add".into(),
            "rule".into(),
            "inet".into(),
            "xanhtab".into(),
            "browser_output".into(),
            "meta".into(),
            "skuid".into(),
            uid,
            "counter".into(),
            "drop".into(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
