use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Real-time throughput and performance telemetry snapshot for GUI & CLI diagnostics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineMetricsSnapshot {
    pub timestamp_ms: u64,
    pub total_rx_bps: f64,
    pub total_tx_bps: f64,
    pub total_rx_bytes: u64,
    pub total_tx_bytes: u64,
    pub cpu_usage_percent: f32,
    pub memory_rss_mb: f32,
    pub active_bonding: bool,
    pub paths: Vec<PathSnapshot>,
}

impl Default for EngineMetricsSnapshot {
    fn default() -> Self {
        Self {
            timestamp_ms: 0,
            total_rx_bps: 0.0,
            total_tx_bps: 0.0,
            total_rx_bytes: 0,
            total_tx_bytes: 0,
            cpu_usage_percent: 0.0,
            memory_rss_mb: 0.0,
            active_bonding: true,
            paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathSnapshot {
    pub path_id: u8,
    pub name: String,
    pub local_ip: String,
    #[serde(default)]
    pub network_name: String,
    pub is_alive: bool,
    pub rtt_ms: f32,
    pub loss_percent: f32,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub share_percent: f32,
}

/// Thread-safe lock-free engine rate tracker
pub struct RateTracker {
    last_sample_time: parking_lot::Mutex<Instant>,
    last_rx_bytes: AtomicU64,
    last_tx_bytes: AtomicU64,
    pub current_rx_bps: parking_lot::Mutex<f64>,
    pub current_tx_bps: parking_lot::Mutex<f64>,
}

impl RateTracker {
    pub fn new() -> Self {
        Self {
            last_sample_time: parking_lot::Mutex::new(Instant::now()),
            last_rx_bytes: AtomicU64::new(0),
            last_tx_bytes: AtomicU64::new(0),
            current_rx_bps: parking_lot::Mutex::new(0.0),
            current_tx_bps: parking_lot::Mutex::new(0.0),
        }
    }

    pub fn tick(&self, total_rx: u64, total_tx: u64) {
        let mut last_time = self.last_sample_time.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(*last_time).as_secs_f64();

        if elapsed >= 0.5 {
            let prev_rx = self.last_rx_bytes.swap(total_rx, Ordering::Relaxed);
            let prev_tx = self.last_tx_bytes.swap(total_tx, Ordering::Relaxed);

            let rx_diff = total_rx.saturating_sub(prev_rx) as f64;
            let tx_diff = total_tx.saturating_sub(prev_tx) as f64;

            let rx_bps = (rx_diff * 8.0) / elapsed;
            let tx_bps = (tx_diff * 8.0) / elapsed;

            *self.current_rx_bps.lock() = rx_bps;
            *self.current_tx_bps.lock() = tx_bps;
            *last_time = now;
        }
    }
}
