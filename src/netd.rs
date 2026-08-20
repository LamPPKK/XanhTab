use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::Mutex,
    time::timeout,
};

use crate::{config::NetworkConfig, error::AppError, model::EgressMode};

pub const WIREGUARD_INTERFACE: &str = "wg0";
pub const WIREGUARD_ROUTE_TABLE: &str = "51820";
pub const WIREGUARD_RULE_PRIORITY: &str = "10000";
pub const NFT_PROGRAM: &str = "/usr/sbin/nft";
pub const IP_PROGRAM: &str = "/usr/sbin/ip";
pub const WG_PROGRAM: &str = "/usr/bin/wg";
pub const WG_QUICK_PROGRAM: &str = "/usr/bin/wg-quick";

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

pub fn kill_switch_plan(config: &NetworkConfig) -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            program: NFT_PROGRAM,
            args: vec![
                "add".into(),
                "table".into(),
                "inet".into(),
                "xanhtab".into(),
            ],
            stdin: None,
        },
        nft_transaction(config, None, None),
    ]
}

pub fn active_policy_plan(
    config: &NetworkConfig,
    mode: EgressMode,
    proxy_endpoint: Option<SocketAddr>,
) -> Result<Vec<CommandSpec>> {
    if matches!(mode, EgressMode::Tor | EgressMode::Warp | EgressMode::Proxy)
        && proxy_endpoint.is_none()
    {
        bail!("proxy egress requires a validated endpoint");
    }
    Ok(vec![nft_transaction(config, Some(mode), proxy_endpoint)])
}

pub fn wireguard_setup_plan(config: &NetworkConfig) -> Vec<CommandSpec> {
    let uid_range = format!("{0}-{0}", config.browser_uid);
    vec![
        CommandSpec {
            program: WG_QUICK_PROGRAM,
            args: vec!["up".into(), config.wireguard_config.display().to_string()],
            stdin: None,
        },
        CommandSpec {
            program: IP_PROGRAM,
            args: vec![
                "route".into(),
                "replace".into(),
                "default".into(),
                "dev".into(),
                WIREGUARD_INTERFACE.into(),
                "table".into(),
                WIREGUARD_ROUTE_TABLE.into(),
            ],
            stdin: None,
        },
        CommandSpec {
            program: IP_PROGRAM,
            args: vec![
                "-6".into(),
                "route".into(),
                "replace".into(),
                "default".into(),
                "dev".into(),
                WIREGUARD_INTERFACE.into(),
                "table".into(),
                WIREGUARD_ROUTE_TABLE.into(),
            ],
            stdin: None,
        },
        CommandSpec {
            program: IP_PROGRAM,
            args: vec![
                "rule".into(),
                "add".into(),
                "priority".into(),
                WIREGUARD_RULE_PRIORITY.into(),
                "uidrange".into(),
                uid_range.clone(),
                "lookup".into(),
                WIREGUARD_ROUTE_TABLE.into(),
            ],
            stdin: None,
        },
        CommandSpec {
            program: IP_PROGRAM,
            args: vec![
                "-6".into(),
                "rule".into(),
                "add".into(),
                "priority".into(),
                WIREGUARD_RULE_PRIORITY.into(),
                "uidrange".into(),
                uid_range,
                "lookup".into(),
                WIREGUARD_ROUTE_TABLE.into(),
            ],
            stdin: None,
        },
        CommandSpec {
            program: WG_PROGRAM,
            args: vec!["show".into(), WIREGUARD_INTERFACE.into()],
            stdin: None,
        },
    ]
}

pub fn wireguard_teardown_plan(config: &NetworkConfig) -> Vec<CommandSpec> {
    let uid_range = format!("{0}-{0}", config.browser_uid);
    vec![
        CommandSpec {
            program: IP_PROGRAM,
            args: vec![
                "rule".into(),
                "del".into(),
                "priority".into(),
                WIREGUARD_RULE_PRIORITY.into(),
                "uidrange".into(),
                uid_range.clone(),
                "lookup".into(),
                WIREGUARD_ROUTE_TABLE.into(),
            ],
            stdin: None,
        },
        CommandSpec {
            program: IP_PROGRAM,
            args: vec![
                "-6".into(),
                "rule".into(),
                "del".into(),
                "priority".into(),
                WIREGUARD_RULE_PRIORITY.into(),
                "uidrange".into(),
                uid_range,
                "lookup".into(),
                WIREGUARD_ROUTE_TABLE.into(),
            ],
            stdin: None,
        },
        CommandSpec {
            program: IP_PROGRAM,
            args: vec![
                "route".into(),
                "flush".into(),
                "table".into(),
                WIREGUARD_ROUTE_TABLE.into(),
            ],
            stdin: None,
        },
        CommandSpec {
            program: IP_PROGRAM,
            args: vec![
                "-6".into(),
                "route".into(),
                "flush".into(),
                "table".into(),
                WIREGUARD_ROUTE_TABLE.into(),
            ],
            stdin: None,
        },
        CommandSpec {
            program: IP_PROGRAM,
            args: vec![
                "link".into(),
                "delete".into(),
                "dev".into(),
                WIREGUARD_INTERFACE.into(),
            ],
            stdin: None,
        },
    ]
}

fn nft_transaction(
    config: &NetworkConfig,
    mode: Option<EgressMode>,
    proxy_endpoint: Option<SocketAddr>,
) -> CommandSpec {
    let uid = config.browser_uid;
    let mut script = String::from(
        "flush table inet xanhtab\n\
         add chain inet xanhtab browser_output { type filter hook output priority filter; policy accept; }\n",
    );
    match mode {
        None => add_drop(&mut script, uid),
        Some(EgressMode::Direct) => {}
        Some(EgressMode::Tor | EgressMode::Warp | EgressMode::Proxy) => {
            let endpoint = proxy_endpoint.expect("validated proxy policy endpoint");
            add_proxy_guard(
                &mut script,
                uid,
                &endpoint.ip().to_string(),
                endpoint.port(),
            );
        }
        Some(EgressMode::WireGuard) => {
            script.push_str(&format!(
                "add rule inet xanhtab browser_output meta skuid {uid} oifname \"{WIREGUARD_INTERFACE}\" accept\n"
            ));
            add_drop(&mut script, uid);
        }
    }
    CommandSpec {
        program: NFT_PROGRAM,
        args: vec!["-f".into(), "-".into()],
        stdin: Some(script),
    }
}

pub fn command_plan(config: &NetworkConfig, mode: EgressMode) -> Vec<CommandSpec> {
    let endpoint = match mode {
        EgressMode::Tor => Some("127.0.0.1:9050".parse().expect("valid endpoint")),
        EgressMode::Warp => Some("127.0.0.1:40000".parse().expect("valid endpoint")),
        EgressMode::Proxy => Some(
            config
                .proxy_endpoint
                .parse()
                .expect("validated proxy endpoint"),
        ),
        _ => None,
    };
    let mut plan = kill_switch_plan(config);
    if mode == EgressMode::WireGuard {
        plan.extend(wireguard_setup_plan(config));
    }
    plan.extend(active_policy_plan(config, mode, endpoint).expect("valid policy"));
    plan
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
                assert!(
                    [NFT_PROGRAM, WG_QUICK_PROGRAM, WG_PROGRAM, IP_PROGRAM]
                        .contains(&command.program)
                );
                assert!(command.program.starts_with('/'));
            }
        }
    }

    #[test]
    fn firewall_policy_is_one_nft_transaction() {
        let config = NetworkConfig::default();
        let plan = active_policy_plan(
            &config,
            EgressMode::Tor,
            Some("127.0.0.1:9050".parse().unwrap()),
        )
        .unwrap();
        let nft = plan
            .iter()
            .filter(|command| command.program == NFT_PROGRAM && command.stdin.is_some())
            .collect::<Vec<_>>();
        assert_eq!(nft.len(), 1);
        let script = nft[0].stdin.as_ref().unwrap();
        assert!(script.contains("flush table inet xanhtab"));
        assert!(script.contains("tcp dport 9050 accept"));
        assert!(script.contains("counter drop"));
    }

    #[test]
    fn every_transition_can_start_with_a_uid_drop() {
        let config = NetworkConfig::default();
        let plan = kill_switch_plan(&config);
        let script = plan.last().unwrap().stdin.as_ref().unwrap();
        assert!(script.contains("meta skuid 988 counter drop"));
    }

    #[test]
    fn wireguard_uses_a_dedicated_uid_policy_table() {
        let config = NetworkConfig::default();
        let plan = wireguard_setup_plan(&config);
        assert!(plan.iter().any(|command| {
            command.program == IP_PROGRAM
                && command.args
                    == [
                        "rule",
                        "add",
                        "priority",
                        WIREGUARD_RULE_PRIORITY,
                        "uidrange",
                        "988-988",
                        "lookup",
                        WIREGUARD_ROUTE_TABLE,
                    ]
        }));
        let active = active_policy_plan(&config, EgressMode::WireGuard, None).unwrap();
        let script = active[0].stdin.as_ref().unwrap();
        assert!(script.contains("oifname \"wg0\" accept"));
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
            .find(|command| command.program == WG_QUICK_PROGRAM)
            .unwrap();
        assert_eq!(command.args[1], "/root/a config.conf");
    }
}
