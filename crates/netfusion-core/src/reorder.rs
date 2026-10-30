use bytes::Bytes;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_CAPACITY: usize = 2048;
pub const DEFAULT_MIN_REORDER_TIMEOUT_MS: u64 = 8;
pub const DEFAULT_MAX_REORDER_TIMEOUT_MS: u64 = 40;

/// An item stored in the reorder buffer
pub struct ReorderItem {
    pub seq_num: u64,
    pub payload: Bytes,
    pub arrived_at: Instant,
}

/// Adaptive packet reorder and jitter buffer to eliminate out-of-order delivery
/// caused by latency differences between different WAN paths.
pub struct ReorderBuffer {
    expected_seq: u64,
    buffer: BTreeMap<u64, (Bytes, Instant)>,
    max_capacity: usize,
    reorder_timeout: Duration,
    hole_detected_at: Option<Instant>,
    stats: ReorderStats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReorderStats {
    pub packets_in_order: u64,
    pub packets_reordered: u64,
    pub packets_dropped_duplicate: u64,
    pub packets_dropped_expired: u64,
    pub holes_timed_out: u64,
}

impl ReorderBuffer {
    pub fn new(initial_seq: u64) -> Self {
        Self {
            expected_seq: initial_seq,
            buffer: BTreeMap::new(),
            max_capacity: DEFAULT_MAX_CAPACITY,
            reorder_timeout: Duration::from_millis(DEFAULT_MIN_REORDER_TIMEOUT_MS),
            hole_detected_at: None,
            stats: ReorderStats::default(),
        }
    }

    /// Dynamically update the reorder timeout based on measured path delay jitter
    pub fn update_timeout(&mut self, delay_spread: Duration) {
        let clamped = delay_spread.clamp(
            Duration::from_millis(DEFAULT_MIN_REORDER_TIMEOUT_MS),
            Duration::from_millis(DEFAULT_MAX_REORDER_TIMEOUT_MS),
        );
        self.reorder_timeout = clamped;
    }

    pub fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    pub fn stats(&self) -> ReorderStats {
        self.stats
    }

    /// Insert an arriving packet with its global sequence number.
    /// Returns a list of in-order packet payloads ready to be injected into the TUN adapter.
    pub fn push(&mut self, seq_num: u64, payload: Bytes) -> Vec<Bytes> {
        let mut ready = Vec::new();
        let now = Instant::now();

        // 1. Packet is older than expected: duplicate or late arrival
        if seq_num < self.expected_seq {
            self.stats.packets_dropped_duplicate += 1;
            return ready;
        }

        // 2. Exactly the expected packet
        if seq_num == self.expected_seq {
            self.stats.packets_in_order += 1;
            ready.push(payload);
            self.expected_seq += 1;
            self.hole_detected_at = None;

            // Drain any subsequent contiguous packets already in buffer
            while let Some(entry) = self.buffer.remove(&self.expected_seq) {
                self.stats.packets_reordered += 1;
                ready.push(entry.0);
                self.expected_seq += 1;
            }

            // If there's still items in the buffer, mark hole start
            if !self.buffer.is_empty() {
                self.hole_detected_at = Some(now);
            }

            return ready;
        }

        // 3. Out-of-order packet (seq_num > expected_seq)
        // Check if buffer capacity exceeded
        if self.buffer.len() >= self.max_capacity {
            // Buffer full: force flush the lowest packets to prevent starvation
            self.stats.packets_dropped_expired += 1;
            if let Some((&first_key, _)) = self.buffer.iter().next() {
                self.expected_seq = first_key;
                while let Some(entry) = self.buffer.remove(&self.expected_seq) {
                    ready.push(entry.0);
                    self.expected_seq += 1;
                }
            }
        }

        // Store out-of-order packet
        self.buffer.insert(seq_num, (payload, now));
        if self.hole_detected_at.is_none() {
            self.hole_detected_at = Some(now);
        }

        ready
    }

    /// Periodic tick to check for hole timeouts.
    /// If a missing packet has not arrived within the adaptive reorder timeout,
    /// we skip the hole to avoid head-of-line blocking and flush buffered packets.
    pub fn drain_expired(&mut self) -> Vec<Bytes> {
        let mut ready = Vec::new();
        let now = Instant::now();

        if let Some(hole_start) = self.hole_detected_at {
            if now.duration_since(hole_start) >= self.reorder_timeout {
                self.stats.holes_timed_out += 1;

                // Advance expected_seq to the lowest key currently in buffer
                if let Some((&first_seq, _)) = self.buffer.iter().next() {
                    self.expected_seq = first_seq;

                    while let Some(entry) = self.buffer.remove(&self.expected_seq) {
                        self.stats.packets_reordered += 1;
                        ready.push(entry.0);
                        self.expected_seq += 1;
                    }

                    if !self.buffer.is_empty() {
                        self.hole_detected_at = Some(now);
                    } else {
                        self.hole_detected_at = None;
                    }
                } else {
                    self.hole_detected_at = None;
                }
            }
        }

        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_order_delivery() {
        let mut rb = ReorderBuffer::new(1);
        let p1 = rb.push(1, Bytes::from_static(b"packet 1"));
        assert_eq!(p1.len(), 1);
        assert_eq!(p1[0], Bytes::from_static(b"packet 1"));

        let p2 = rb.push(2, Bytes::from_static(b"packet 2"));
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0], Bytes::from_static(b"packet 2"));
    }

    #[test]
    fn test_reordering_out_of_order() {
        let mut rb = ReorderBuffer::new(1);

        // Packet 2 arrives first (e.g. Jio path faster for this packet)
        let r2 = rb.push(2, Bytes::from_static(b"packet 2"));
        assert!(r2.is_empty(), "Packet 2 should be buffered waiting for 1");

        // Packet 3 arrives second
        let r3 = rb.push(3, Bytes::from_static(b"packet 3"));
        assert!(r3.is_empty(), "Packet 3 should be buffered");

        // Packet 1 arrives last
        let r1 = rb.push(1, Bytes::from_static(b"packet 1"));
        assert_eq!(r1.len(), 3, "All 3 packets should now be flushed in order");
        assert_eq!(r1[0], Bytes::from_static(b"packet 1"));
        assert_eq!(r1[1], Bytes::from_static(b"packet 2"));
        assert_eq!(r1[2], Bytes::from_static(b"packet 3"));
    }

    #[test]
    fn test_hole_timeout() {
        let mut rb = ReorderBuffer::new(1);
        rb.update_timeout(Duration::from_millis(5));

        // Packet 1 is lost on the wire! Packet 2 arrives
        let r = rb.push(2, Bytes::from_static(b"packet 2"));
        assert!(r.is_empty());

        // Wait for timeout to expire
        std::thread::sleep(Duration::from_millis(15));
        let drained = rb.drain_expired();
        assert_eq!(drained.len(), 1, "Packet 2 should be flushed after hole timeout");
        assert_eq!(drained[0], Bytes::from_static(b"packet 2"));
        assert_eq!(rb.expected_seq(), 3);
    }
}
