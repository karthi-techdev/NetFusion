use std::time::{Duration, Instant};

/// Metrics and quality indicators for an individual WAN path
#[derive(Debug, Clone)]
pub struct PathMetrics {
    pub path_id: u8,
    pub name: String,
    pub local_ip: String,
    pub network_name: String,
    pub rtt: Duration,
    pub smoothed_rtt: Duration,
    pub rtt_variance: Duration,
    pub loss_rate: f32,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub tx_packets: u64,
    pub rx_packets: u64,
    pub last_seen: Instant,
    pub is_alive: bool,
    pub capacity_weight: f32,
    pub dynamic_credit: f32,
    pub active_flows: u32,
    pub is_v4_up: bool,
    pub is_v6_up: bool,
}

impl PathMetrics {
    pub fn new(path_id: u8, name: String, local_ip: String) -> Self {
        Self {
            path_id,
            name,
            local_ip,
            network_name: String::new(),
            rtt: Duration::from_millis(20),
            smoothed_rtt: Duration::from_millis(20),
            rtt_variance: Duration::from_millis(5),
            loss_rate: 0.0,
            tx_bytes: 0,
            rx_bytes: 0,
            tx_packets: 0,
            rx_packets: 0,
            last_seen: Instant::now(),
            is_alive: true,
            capacity_weight: 1.0,
            dynamic_credit: 1.0,
            active_flows: 0,
            is_v4_up: true,
            is_v6_up: false,
        }
    }

    /// Update RTT using standard RFC 6298 EWMA smoothing
    pub fn update_rtt(&mut self, sample: Duration) {
        self.rtt = sample;
        self.last_seen = Instant::now();
        self.is_alive = true;

        if self.smoothed_rtt.as_micros() == 0 {
            self.smoothed_rtt = sample;
            self.rtt_variance = sample / 2;
        } else {
            let diff = if sample > self.smoothed_rtt {
                sample - self.smoothed_rtt
            } else {
                self.smoothed_rtt - sample
            };

            // RTTVAR <- (3/4) * RTTVAR + (1/4) * |SRTT - R'|
            self.rtt_variance = (self.rtt_variance * 3 + diff) / 4;
            // SRTT <- (7/8) * SRTT + (1/8) * R'
            self.smoothed_rtt = (self.smoothed_rtt * 7 + sample) / 8;
        }
    }

    /// Update packet loss rate with EWMA
    pub fn update_loss(&mut self, loss_fraction: f32) {
        self.loss_rate = self.loss_rate * 0.8 + loss_fraction * 0.2;
    }
}

/// Dynamic Weighted Fair Queueing / Deficit Round Robin Scheduler.
/// Ensures packet distribution matches dynamic path capacity, RTT, and loss characteristics
/// to achieve genuine aggregated single-connection bandwidth without head-of-line blocking.
pub struct AdaptiveScheduler {
    paths: Vec<PathMetrics>,
    current_index: usize,
    quantum: f32,
}

impl AdaptiveScheduler {
    pub fn new(quantum: f32) -> Self {
        Self {
            paths: Vec::new(),
            current_index: 0,
            quantum: if quantum > 0.0 { quantum } else { 1500.0 },
        }
    }

    pub fn add_path(&mut self, path: PathMetrics) {
        self.paths.push(path);
    }

    pub fn paths(&self) -> &[PathMetrics] {
        &self.paths
    }

    pub fn paths_mut(&mut self) -> &mut [PathMetrics] {
        &mut self.paths
    }

    /// Recalculate weights based on bandwidth, RTT, and packet loss
    /// Formula: Weight = Capacity * (1 - LossRate) / (SRTT_ms ^ 0.5)
    pub fn recalculate_weights(&mut self) {
        if self.paths.is_empty() {
            return;
        }

        let mut total_score = 0.0f32;
        let mut scores = Vec::with_capacity(self.paths.len());

        for p in &self.paths {
            if !p.is_alive {
                scores.push(0.0);
                continue;
            }

            let rtt_ms = (p.smoothed_rtt.as_micros() as f32 / 1000.0).max(1.0);
            let loss_penalty = (1.0 - p.loss_rate).clamp(0.05, 1.0);

            // Score incorporates bandwidth capacity, inverse latency square root, and loss penalty
            let score = (p.capacity_weight * loss_penalty) / rtt_ms.sqrt();
            scores.push(score);
            total_score += score;
        }

        if total_score > 0.0 {
            for (i, p) in self.paths.iter_mut().enumerate() {
                p.dynamic_credit = (scores[i] / total_score).max(0.0);
            }
        }
    }

    /// Check whether every active IPv4 path also has verified IPv6 internet routing.
    /// If even one active path lacks working IPv6, returns false so dual-stack destinations
    /// fall back to IPv4 and utilize all physical adapters evenly without single-adapter funneling.
    pub fn v6_covers_all(&self) -> bool {
        let active_wans: Vec<&PathMetrics> = self.paths.iter().filter(|p| p.is_alive).collect();
        if active_wans.is_empty() {
            return false;
        }
        let any_v6 = active_wans.iter().any(|p| p.is_v6_up);
        if !any_v6 {
            return false;
        }
        for p in active_wans {
            if p.is_v4_up && !p.is_v6_up {
                return false;
            }
        }
        true
    }

    /// Select the next optimal WAN path for a new flow (connection) based on least active flows
    /// and address family availability.
    pub fn select_next_flow_path(&mut self, is_v6: bool) -> Option<u8> {
        let eligible: Vec<usize> = self.paths
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_alive && if is_v6 { p.is_v6_up } else { p.is_v4_up })
            .map(|(i, _)| i)
            .collect();

        if eligible.is_empty() {
            // Fallback: any alive path
            return self.select_next_path();
        }

        // Find candidate with lowest score: (active_flows + 1) / capacity_weight
        let mut best_idx = eligible[0];
        let mut min_score = f32::MAX;

        let start = self.current_index % eligible.len();
        for offset in 0..eligible.len() {
            let idx = eligible[(start + offset) % eligible.len()];
            let p = &self.paths[idx];
            let weight = if p.capacity_weight > 0.05 { p.capacity_weight } else { 1.0 };
            let score = ((p.active_flows + 1) as f32) / weight;
            if score < min_score {
                min_score = score;
                best_idx = idx;
            }
        }

        self.current_index = (self.current_index + 1) % self.paths.len();
        self.paths[best_idx].active_flows += 1;
        self.paths[best_idx].tx_packets += 1;
        Some(self.paths[best_idx].path_id)
    }

    /// Release an active flow on a path when connection closes
    pub fn release_flow(&mut self, path_id: u8) {
        if let Some(p) = self.paths.iter_mut().find(|p| p.path_id == path_id) {
            p.active_flows = p.active_flows.saturating_sub(1);
        }
    }

    /// Select the next active WAN path using strict fair round-robin alternation.
    /// Guarantees that multiple WAN interfaces (e.g. Ethernet and Wi-Fi) are actively and
    /// simultaneously utilized across concurrent connections.
    pub fn select_next_path(&mut self) -> Option<u8> {
        let n = self.paths.len();
        if n == 0 {
            return None;
        }
        for _ in 0..n {
            let idx = self.current_index;
            self.current_index = (self.current_index + 1) % n;
            if self.paths[idx].is_alive {
                self.paths[idx].tx_packets += 1;
                return Some(self.paths[idx].path_id);
            }
        }
        self.paths.first().map(|p| p.path_id)
    }

    /// Select the next optimal WAN path for an outbound packet of size `packet_len` bytes.
    /// Returns the chosen path_id, or None if no paths are alive.
    pub fn select_path(&mut self, packet_len: usize) -> Option<u8> {
        let active_count = self.paths.iter().filter(|p| p.is_alive).count();
        if active_count == 0 {
            return None;
        }
        if active_count == 1 {
            return self.paths.iter().find(|p| p.is_alive).map(|p| p.path_id);
        }

        let len_f = packet_len as f32;
        let n = self.paths.len();

        // Perform Deficit Weighted Round Robin over active paths
        for _ in 0..(n * 2) {
            let path = &mut self.paths[self.current_index];
            if path.is_alive {
                if path.dynamic_credit >= len_f {
                    path.dynamic_credit -= len_f;
                    path.tx_packets += 1;
                    path.tx_bytes += packet_len as u64;
                    let chosen_id = path.path_id;
                    if path.dynamic_credit < len_f {
                        self.current_index = (self.current_index + 1) % n;
                    }
                    return Some(chosen_id);
                } else {
                    // Refill deficit credits according to capacity weight
                    path.dynamic_credit += self.quantum * path.capacity_weight;
                }
            }
            self.current_index = (self.current_index + 1) % n;
        }

        // Fallback: select path with lowest current RTT among alive paths
        self.paths
            .iter_mut()
            .filter(|p| p.is_alive)
            .min_by_key(|p| p.smoothed_rtt)
            .map(|p| {
                p.tx_packets += 1;
                p.tx_bytes += packet_len as u64;
                p.path_id
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scheduler_weighted_distribution() {
        let mut sched = AdaptiveScheduler::new(1500.0);

        // Path 0: WAN Primary (40 Mbps, weight 4.0, RTT 15ms)
        let mut p0 = PathMetrics::new(0, "WAN_Primary".to_string(), "192.0.2.1".to_string());
        p0.capacity_weight = 4.0;
        p0.smoothed_rtt = Duration::from_millis(15);
        sched.add_path(p0);

        // Path 1: WAN Secondary (10 Mbps, weight 1.0, RTT 25ms)
        let mut p1 = PathMetrics::new(1, "WAN_Secondary".to_string(), "198.51.100.1".to_string());
        p1.capacity_weight = 1.0;
        p1.smoothed_rtt = Duration::from_millis(25);
        sched.add_path(p1);

        sched.recalculate_weights();

        let mut counts = [0u32; 2];
        for _ in 0..100 {
            if let Some(path_id) = sched.select_path(1400) {
                counts[path_id as usize] += 1;
            }
        }

        // WAN Primary should receive significantly more packets than WAN Secondary (expected ~4:1 ratio)
        assert!(
            counts[0] > counts[1] * 2,
            "WAN Primary ({}) should receive at least 2x more than Secondary ({})",
            counts[0],
            counts[1]
        );
    }

    #[test]
    fn test_v6_covers_all_asymmetry_detection() {
        let mut sched = AdaptiveScheduler::new(1500.0);

        // Path 0: Ethernet (v4 up, v6 down)
        let mut p0 = PathMetrics::new(0, "Ethernet".to_string(), "192.168.29.7".to_string());
        p0.is_v4_up = true;
        p0.is_v6_up = false;
        sched.add_path(p0);

        // Path 1: WiFi (v4 up, v6 up)
        let mut p1 = PathMetrics::new(1, "WiFi".to_string(), "192.168.29.224".to_string());
        p1.is_v4_up = true;
        p1.is_v6_up = true;
        sched.add_path(p1);

        // Crucial test: V6 does NOT cover all adapters!
        assert!(!sched.v6_covers_all(), "When Ethernet lacks working IPv6, v6_covers_all must be false");

        // When Ethernet IPv6 is fixed, v6_covers_all becomes true!
        sched.paths_mut()[0].is_v6_up = true;
        assert!(sched.v6_covers_all(), "When all adapters have IPv6, v6_covers_all must be true");
    }

    #[test]
    fn test_least_active_flows_balanced_distribution() {
        let mut sched = AdaptiveScheduler::new(1500.0);

        let mut p0 = PathMetrics::new(0, "Ethernet".to_string(), "192.168.29.7".to_string());
        p0.is_v4_up = true;
        sched.add_path(p0);

        let mut p1 = PathMetrics::new(1, "WiFi".to_string(), "192.168.29.224".to_string());
        p1.is_v4_up = true;
        sched.add_path(p1);

        // Flow 1 -> Path 0
        let f1 = sched.select_next_flow_path(false).unwrap();
        // Flow 2 -> Path 1 (Path 0 is busy with f1)
        let f2 = sched.select_next_flow_path(false).unwrap();
        assert_ne!(f1, f2, "Concurrent flows must be distributed to different adapters");

        // Release flow 1 on Path 0
        sched.release_flow(f1);

        // Flow 3 should now pick Path 0 because Path 1 is still busy with f2!
        let f3 = sched.select_next_flow_path(false).unwrap();
        assert_eq!(f3, f1, "New flow must be assigned to the idle adapter");
    }
}
