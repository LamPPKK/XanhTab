use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    #[default]
    Idle,
    Starting,
    Active,
    Burning,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressMode {
    #[default]
    Direct,
    Tor,
    Warp,
    #[serde(rename = "wireguard")]
    WireGuard,
    Proxy,
}

impl fmt::Display for EgressMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Direct => "direct",
            Self::Tor => "tor",
            Self::Warp => "warp",
            Self::WireGuard => "wireguard",
            Self::Proxy => "proxy",
        })
    }
}

impl FromStr for EgressMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "direct" => Ok(Self::Direct),
            "tor" => Ok(Self::Tor),
            "warp" => Ok(Self::Warp),
            "wireguard" | "wg" => Ok(Self::WireGuard),
            "proxy" => Ok(Self::Proxy),
            other => Err(format!("unsupported egress mode: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    #[default]
    #[serde(rename = "1080p30")]
    FullHd30,
    #[serde(rename = "720p15")]
    Hd15,
    #[serde(rename = "480p10")]
    Sd10,
}

impl StreamProfile {
    pub const fn dimensions(self) -> (u16, u16, u8) {
        match self {
            Self::FullHd30 => (1920, 1080, 30),
            Self::Hd15 => (1280, 720, 15),
            Self::Sd10 => (854, 480, 10),
        }
    }
}

impl fmt::Display for StreamProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FullHd30 => "1080p30",
            Self::Hd15 => "720p15",
            Self::Sd10 => "480p10",
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    pub id: Option<Uuid>,
    pub phase: SessionPhase,
    pub url: Option<Url>,
    pub title: Option<String>,
    pub egress: EgressMode,
    pub stream_profile: StreamProfile,
    pub controller_attached: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub failure: Option<String>,
}

impl Default for SessionSnapshot {
    fn default() -> Self {
        Self {
            id: None,
            phase: SessionPhase::Idle,
            url: None,
            title: None,
            egress: EgressMode::Direct,
            stream_profile: StreamProfile::FullHd30,
            controller_attached: false,
            started_at: None,
            failure: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationCommand {
    Navigate { url: Url },
    Back,
    Forward,
    Reload,
    Stop,
}
