use std::{
    fs,
    sync::{Arc, RwLock},
};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::{
    blocklist::Blocklist,
    model::{EgressMode, StreamProfile},
};

#[derive(Debug, Clone, Default, Serialize)]
pub struct StreamMetrics {
    pub fps: f32,
    pub bitrate_kbps: u32,
    pub packet_loss_percent: f32,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceMetrics {
    pub sampled_at: DateTime<Utc>,
    pub memory_total_mib: u64,
    pub memory_available_mib: u64,
    pub temperature_celsius: Option<f32>,
    pub stream: StreamMetrics,
    pub profile: StreamProfile,
    pub egress: EgressMode,
    pub blocklist_entries: usize,
    pub blocked_requests: u64,
}

#[derive(Clone)]
pub struct MetricsCollector {
    stream: Arc<RwLock<StreamMetrics>>,
    blocklist: Blocklist,
}

impl MetricsCollector {
    pub fn new(blocklist: Blocklist) -> Self {
        Self {
            stream: Arc::new(RwLock::new(StreamMetrics::default())),
            blocklist,
        }
    }

    pub fn update_stream(&self, metrics: StreamMetrics) {
        *self.stream.write().expect("metrics lock poisoned") = metrics;
    }

    pub fn sample(&self, profile: StreamProfile, egress: EgressMode) -> DeviceMetrics {
        let (memory_total_mib, memory_available_mib) = read_memory();
        DeviceMetrics {
            sampled_at: Utc::now(),
            memory_total_mib,
            memory_available_mib,
            temperature_celsius: read_temperature(),
            stream: self.stream.read().expect("metrics lock poisoned").clone(),
            profile,
            egress,
            blocklist_entries: self.blocklist.len(),
            blocked_requests: self.blocklist.hits(),
        }
    }
}

fn read_memory() -> (u64, u64) {
    let Ok(raw) = fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };
    let mut total = 0;
    let mut available = 0;
    for line in raw.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("MemTotal:") => {
                total = parts
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
                    / 1024
            }
            Some("MemAvailable:") => {
                available = parts
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
                    / 1024
            }
            _ => {}
        }
    }
    (total, available)
}

fn read_temperature() -> Option<f32> {
    let raw = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    raw.trim().parse::<f32>().ok().map(|value| value / 1000.0)
}
