use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::{EgressMode, StreamProfile};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub session: SessionConfig,
    pub browser: BrowserConfig,
    pub network: NetworkConfig,
    pub blocklist: BlocklistConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub public_base_url: String,
    pub static_dir: PathBuf,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub secure_cookies: bool,
    pub allow_insecure_http: bool,
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    pub runtime_dir: PathBuf,
    pub pairing_file: PathBuf,
    pub auth_ttl_seconds: u64,
    pub ticket_ttl_seconds: u64,
    pub auto_burn_seconds: u64,
    pub initial_url: String,
    pub initial_profile: StreamProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    pub enabled: bool,
    pub command: PathBuf,
    pub socket: PathBuf,
    pub stop_timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    pub enabled: bool,
    pub initial_mode: EgressMode,
    pub netd_socket: PathBuf,
    pub browser_uid: u32,
    pub tor_proxy: String,
    pub warp_proxy: String,
    pub wireguard_config: PathBuf,
    pub proxy_url_file: PathBuf,
    pub proxy_endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BlocklistConfig {
    pub fst_path: PathBuf,
    pub custom_hosts_path: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8088"
                .parse()
                .expect("valid default listen address"),
            public_base_url: "http://127.0.0.1:8088".into(),
            static_dir: PathBuf::from("web"),
            tls_cert: None,
            tls_key: None,
            secure_cookies: false,
            allow_insecure_http: true,
            allowed_origins: vec!["http://127.0.0.1:8088".into()],
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            runtime_dir: PathBuf::from("/run/xanhtab-session"),
            pairing_file: PathBuf::from("/run/xanhtab/pairing.txt"),
            auth_ttl_seconds: 28_800,
            ticket_ttl_seconds: 60,
            auto_burn_seconds: 1_800,
            initial_url: "xanhtab://home".into(),
            initial_profile: StreamProfile::FullHd30,
        }
    }
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: PathBuf::from("/usr/local/libexec/xanhtab-browser"),
            socket: PathBuf::from("/run/xanhtab/browser.sock"),
            stop_timeout_seconds: 5,
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            initial_mode: EgressMode::Direct,
            netd_socket: PathBuf::from("/run/xanhtab/netd.sock"),
            browser_uid: 988,
            tor_proxy: "socks5://127.0.0.1:9050".into(),
            warp_proxy: "socks5://127.0.0.1:40000".into(),
            wireguard_config: PathBuf::from("/etc/xanhtab/secrets/wg0.conf"),
            proxy_url_file: PathBuf::from("/etc/xanhtab/secrets/proxy-url"),
            proxy_endpoint: "127.0.0.1:1080".into(),
        }
    }
}

impl Default for BlocklistConfig {
    fn default() -> Self {
        Self {
            fst_path: PathBuf::from("/var/lib/xanhtab/blocklist.fst"),
            custom_hosts_path: PathBuf::from("/etc/xanhtab/custom_hosts.txt"),
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self =
            toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let public_url = self
            .server
            .public_base_url
            .parse::<url::Url>()
            .context("public_base_url must be an absolute URL")?;
        if !matches!(public_url.scheme(), "http" | "https") || public_url.host_str().is_none() {
            bail!("public_base_url must be an HTTP(S) origin");
        }
        if self.server.public_base_url.starts_with("http://") && !self.server.allow_insecure_http {
            bail!("insecure public_base_url requires allow_insecure_http=true");
        }
        if !self.server.allow_insecure_http
            && (self.server.tls_cert.is_none() || self.server.tls_key.is_none())
        {
            bail!("TLS certificate and key are required outside development mode");
        }
        if self.session.auth_ttl_seconds < 300 {
            bail!("auth_ttl_seconds must be at least 300");
        }
        if self.session.ticket_ttl_seconds == 0 || self.session.ticket_ttl_seconds > 300 {
            bail!("ticket_ttl_seconds must be between 1 and 300");
        }
        if self.network.browser_uid == 0 {
            bail!("browser_uid must not be root");
        }
        if !self.session.runtime_dir.is_absolute()
            || self.session.runtime_dir == Path::new("/")
            || self.session.runtime_dir.components().count() < 3
        {
            bail!("runtime_dir must be a specific absolute directory");
        }
        if !self.session.pairing_file.is_absolute()
            || !self.browser.socket.is_absolute()
            || !self.network.netd_socket.is_absolute()
        {
            bail!("pairing and service socket paths must be absolute");
        }
        if self.session.initial_url.parse::<url::Url>().is_err() {
            bail!("initial_url must be an absolute URL");
        }
        if self
            .network
            .proxy_endpoint
            .parse::<std::net::SocketAddr>()
            .is_err()
        {
            bail!("proxy_endpoint must be an IP address and port");
        }
        Ok(())
    }

    pub fn auth_ttl(&self) -> Duration {
        Duration::from_secs(self.session.auth_ttl_seconds)
    }

    pub fn ticket_ttl(&self) -> Duration {
        Duration::from_secs(self.session.ticket_ttl_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_requires_tls() {
        let mut config = Config::default();
        config.server.allow_insecure_http = false;
        assert!(config.validate().is_err());
    }

    #[test]
    fn browser_must_be_unprivileged() {
        let mut config = Config::default();
        config.network.browser_uid = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn runtime_directory_cannot_be_a_broad_path() {
        let mut config = Config::default();
        config.session.runtime_dir = PathBuf::from("/");
        assert!(config.validate().is_err());
    }
}
