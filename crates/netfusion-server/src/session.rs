use bytes::Bytes;
use netfusion_core::crypto::TunnelCrypto;
use netfusion_core::reorder::ReorderBuffer;
use netfusion_core::scheduler::{AdaptiveScheduler, PathMetrics};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Represents an active authenticated client bonding session
pub struct ClientSession {
    pub session_id: u32,
    pub assigned_ip: Ipv4Addr,
    pub crypto: Arc<TunnelCrypto>,
    pub client_paths: RwLock<HashMap<u8, SocketAddr>>,
    pub scheduler: RwLock<AdaptiveScheduler>,
    pub reorder_buf: Mutex<ReorderBuffer>,
    pub last_seen: Mutex<Instant>,
}

impl ClientSession {
    pub fn new(
        session_id: u32,
        assigned_ip: Ipv4Addr,
        crypto: Arc<TunnelCrypto>,
        initial_reorder_timeout_ms: u64,
    ) -> Self {
        let mut reorder = ReorderBuffer::new(1);
        reorder.update_timeout(Duration::from_millis(initial_reorder_timeout_ms));

        Self {
            session_id,
            assigned_ip,
            crypto,
            client_paths: RwLock::new(HashMap::new()),
            scheduler: RwLock::new(AdaptiveScheduler::new(1500.0)),
            reorder_buf: Mutex::new(reorder),
            last_seen: Mutex::new(Instant::now()),
        }
    }

    /// Register or update an active WAN uplink from the client
    pub fn update_client_path(&self, path_id: u8, remote_addr: SocketAddr) {
        *self.last_seen.lock() = Instant::now();
        let mut paths = self.client_paths.write();
        let is_new = !paths.contains_key(&path_id);
        paths.insert(path_id, remote_addr);

        if is_new {
            let mut sched = self.scheduler.write();
            let mut pm = PathMetrics::new(path_id, format!("ClientPath-{}", path_id), remote_addr.to_string());
            pm.capacity_weight = 1.0;
            sched.add_path(pm);
        }
    }

    /// Select optimal downlink WAN path to send a return packet of size `len`
    pub fn select_downlink_path(&self, len: usize) -> Option<(u8, SocketAddr)> {
        let mut sched = self.scheduler.write();
        if let Some(path_id) = sched.select_path(len) {
            let paths = self.client_paths.read();
            if let Some(&addr) = paths.get(&path_id) {
                return Some((path_id, addr));
            }
        }
        // Fallback: send via first available path
        let paths = self.client_paths.read();
        paths.iter().next().map(|(&pid, &addr)| (pid, addr))
    }
}

/// Global session repository
pub struct SessionManager {
    sessions_by_id: RwLock<HashMap<u32, Arc<ClientSession>>>,
    sessions_by_ip: RwLock<HashMap<Ipv4Addr, Arc<ClientSession>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions_by_id: RwLock::new(HashMap::new()),
            sessions_by_ip: RwLock::new(HashMap::new()),
        }
    }

    pub fn insert(&self, session: Arc<ClientSession>) {
        self.sessions_by_id.write().insert(session.session_id, session.clone());
        self.sessions_by_ip.write().insert(session.assigned_ip, session);
    }

    pub fn get_by_id(&self, session_id: u32) -> Option<Arc<ClientSession>> {
        self.sessions_by_id.read().get(&session_id).cloned()
    }

    pub fn get_by_ip(&self, ip: &Ipv4Addr) -> Option<Arc<ClientSession>> {
        self.sessions_by_ip.read().get(ip).cloned()
    }
}
