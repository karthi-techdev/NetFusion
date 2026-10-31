use log::{info, warn};
use netfusion_core::scheduler::AdaptiveScheduler;
use parking_lot::RwLock;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// High-speed lock-free atomic bandwidth accounting for proxy paths
pub static PROXY_PATH_RX_BYTES: [AtomicU64; 16] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];
pub static PROXY_PATH_TX_BYTES: [AtomicU64; 16] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];
pub static PROXY_PATH_RX_PACKETS: [AtomicU64; 16] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];
pub static PROXY_PATH_TX_PACKETS: [AtomicU64; 16] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];

// In-memory DNS cache to eliminate duplicate DNS queries across parallel streams
struct DnsEntry {
    addrs: Vec<SocketAddr>,
    expires_at: Instant,
}
static DNS_CACHE: RwLock<Option<HashMap<String, DnsEntry>>> = RwLock::new(None);

async fn cached_resolve(host: &str, port: u16, v6_covers_all: bool) -> Vec<SocketAddr> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return vec![SocketAddr::new(ip, port)];
    }

    let key = format!("{}:{}:{}", host, port, v6_covers_all);
    {
        let cache_lock = DNS_CACHE.read();
        if let Some(ref cache) = *cache_lock {
            if let Some(entry) = cache.get(&key) {
                if Instant::now() < entry.expires_at {
                    return entry.addrs.clone();
                }
            }
        }
    }

    let lookup_target = format!("{}:{}", host, port);
    let raw_addrs: Vec<SocketAddr> = match tokio::time::timeout(
        std::time::Duration::from_millis(6000),
        tokio::net::lookup_host(&lookup_target),
    ).await {
        Ok(Ok(iter)) => iter.collect(),
        _ => Vec::new(),
    };

    let addrs: Vec<SocketAddr> = if v6_covers_all {
        raw_addrs
    } else {
        // If not every active adapter has verified IPv6 routing,
        // filter out IPv6 addresses so dual-stack browsers fall back to IPv4.
        // Because IPv4 is supported on 100% of adapters, traffic is evenly bonded
        // across all adapters without single-adapter funneling.
        let v4_only: Vec<SocketAddr> = raw_addrs.iter().filter(|a| a.is_ipv4()).copied().collect();
        if v4_only.is_empty() {
            raw_addrs
        } else {
            v4_only
        }
    };

    if !addrs.is_empty() {
        let mut cache_lock = DNS_CACHE.write();
        let map = cache_lock.get_or_insert_with(HashMap::new);
        map.insert(
            key,
            DnsEntry {
                addrs: addrs.clone(),
                expires_at: Instant::now() + std::time::Duration::from_secs(120),
            },
        );
    }

    addrs
}

#[derive(Clone, Debug)]
pub struct WanInterfaceInfo {
    pub path_id: u8,
    pub name: String,
    pub ipv4: Option<Ipv4Addr>,
    pub ipv6: Option<Ipv6Addr>,
    pub gateway: Option<Ipv4Addr>,
    pub ipv6_gateway: Option<Ipv6Addr>,
    pub if_index: u32,
}

/// RAII Guard that automatically tracks and releases active flows on WAN paths
struct FlowGuard {
    scheduler: Arc<RwLock<AdaptiveScheduler>>,
    path_id: u8,
}

impl Drop for FlowGuard {
    fn drop(&mut self) {
        let mut sched = self.scheduler.write();
        sched.release_flow(self.path_id);
    }
}

/// High-performance local Multi-WAN dispatcher proxy
/// Automatically distributes parallel web connections (Google speed test, Chrome, Edge)
/// across independent physical network interfaces with zero remote VPS required.
pub struct LocalDispatcher {
    listen_port: u16,
    scheduler: Arc<RwLock<AdaptiveScheduler>>,
    wan_interfaces: Arc<RwLock<Vec<WanInterfaceInfo>>>,
    bonding_active: Arc<AtomicBool>,
}

impl LocalDispatcher {
    pub fn new(
        listen_port: u16,
        scheduler: Arc<RwLock<AdaptiveScheduler>>,
        wan_interfaces: Arc<RwLock<Vec<WanInterfaceInfo>>>,
        bonding_active: Arc<AtomicBool>,
    ) -> Self {
        Self {
            listen_port,
            scheduler,
            wan_interfaces,
            bonding_active,
        }
    }

    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        let bind_addr = format!("127.0.0.1:{}", self.listen_port);
        let listener = TcpListener::bind(&bind_addr).await?;
        info!("NetFusion Local Multi-WAN Dispatcher listening on {}", bind_addr);

        loop {
            match listener.accept().await {
                Ok((client_stream, _client_addr)) => {
                    let _ = client_stream.set_nodelay(true);
                    let scheduler = self.scheduler.clone();
                    let wan_interfaces = self.wan_interfaces.clone();
                    let is_active = self.bonding_active.load(Ordering::Relaxed);

                    tokio::spawn(async move {
                        let _ = handle_client(client_stream, scheduler, wan_interfaces, is_active).await;
                    });
                }
                Err(e) => {
                    warn!("Local dispatcher accept error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        }
    }
}

async fn handle_client(
    mut client: TcpStream,
    scheduler: Arc<RwLock<AdaptiveScheduler>>,
    wan_interfaces: Arc<RwLock<Vec<WanInterfaceInfo>>>,
    is_active: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut buf = vec![0u8; 4096];
    let n = client.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    let req_str = String::from_utf8_lossy(&buf[..n]);
    let first_line = req_str.lines().next().unwrap_or("");

    if first_line.starts_with("CONNECT ") {
        // HTTPS Tunnel: CONNECT host:port HTTP/1.1
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Ok(());
        }
        let target = parts[1];
        let (target_host, target_port) = parse_host_port(target, 443);
        let is_target_v6 = target_host.starts_with('[') || target_host.parse::<Ipv6Addr>().is_ok();

        // Select optimal WAN path via least active flows and address family availability
        let (path_id, v6_covers_all) = if is_active {
            let mut sched = scheduler.write();
            let covers = sched.v6_covers_all();
            let pid = sched.select_next_flow_path(is_target_v6).unwrap_or(0);
            (pid, covers)
        } else {
            (0, true)
        };

        let target_wan = {
            let wans = wan_interfaces.read();
            wans.iter()
                .find(|w| w.path_id == path_id)
                .cloned()
                .or_else(|| wans.first().cloned())
        };

        if let Some(wan) = target_wan {
            let target_addrs = cached_resolve(&target_host, target_port, v6_covers_all).await;
            let connect_res = connect_with_failover(&wan, &target_addrs, &wan_interfaces).await;

            match connect_res {
                Ok((remote_stream, active_pid)) => {
                    let _flow_guard = FlowGuard {
                        scheduler: scheduler.clone(),
                        path_id: active_pid,
                    };

                    client
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await?;

                    splice_bidirectional(client, remote_stream, active_pid).await;
                }
                Err(e) => {
                    warn!("CONNECT tunnel failed to {}:{}: {}", target_host, target_port, e);
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                }
            }
        } else {
            warn!("No available WAN interface for CONNECT to {}:{}", target_host, target_port);
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
        }
    } else {
        // Plain HTTP proxying
        let mut host_header = None;
        for line in req_str.lines() {
            if line.to_ascii_lowercase().starts_with("host:") {
                host_header = Some(line[5..].trim().to_string());
                break;
            }
        }

        if let Some(host) = host_header {
            let (target_host, target_port) = parse_host_port(&host, 80);
            let is_target_v6 = target_host.starts_with('[') || target_host.parse::<Ipv6Addr>().is_ok();

            let (path_id, v6_covers_all) = if is_active {
                let mut sched = scheduler.write();
                let covers = sched.v6_covers_all();
                let pid = sched.select_next_flow_path(is_target_v6).unwrap_or(0);
                (pid, covers)
            } else {
                (0, true)
            };

            let target_wan = {
                let wans = wan_interfaces.read();
                wans.iter()
                    .find(|w| w.path_id == path_id)
                    .cloned()
                    .or_else(|| wans.first().cloned())
            };

            if let Some(wan) = target_wan {
                let target_addrs = cached_resolve(&target_host, target_port, v6_covers_all).await;
                let connect_res = connect_with_failover(&wan, &target_addrs, &wan_interfaces).await;

                if let Ok((mut remote_stream, active_pid)) = connect_res {
                    let _flow_guard = FlowGuard {
                        scheduler: scheduler.clone(),
                        path_id: active_pid,
                    };

                    let payload = if first_line.contains(" http://") {
                        let parts: Vec<&str> = first_line.split_whitespace().collect();
                        if parts.len() >= 3 {
                            let method = parts[0];
                            let uri = parts[1];
                            let version = parts[2];
                            let path_and_query = if let Some(stripped) = uri.strip_prefix("http://") {
                                if let Some(slash) = stripped.find('/') {
                                    &stripped[slash..]
                                } else if let Some(q) = stripped.find('?') {
                                    &stripped[q..]
                                } else {
                                    "/"
                                }
                            } else {
                                "/"
                            };
                            let new_first = format!("{} {} {}\r\n", method, path_and_query, version);
                            let rest = &req_str[first_line.len()..];
                            let rest = rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix("\n")).unwrap_or(rest);
                            format!("{}{}", new_first, rest).into_bytes()
                        } else {
                            buf[..n].to_vec()
                        }
                    } else {
                        buf[..n].to_vec()
                    };

                    remote_stream.write_all(&payload).await?;
                    let pid = (active_pid as usize).min(15);
                    PROXY_PATH_TX_BYTES[pid].fetch_add(payload.len() as u64, Ordering::Relaxed);
                    PROXY_PATH_TX_PACKETS[pid].fetch_add(1, Ordering::Relaxed);

                    splice_bidirectional(client, remote_stream, active_pid).await;
                } else {
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                }
            } else {
                let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
            }
        }
    }

    Ok(())
}

fn parse_host_port(target: &str, default_port: u16) -> (String, u16) {
    if target.starts_with('[') {
        if let Some(end_bracket) = target.find(']') {
            let host = &target[1..end_bracket];
            let port = if let Some(colon) = target[end_bracket..].find(':') {
                target[end_bracket + colon + 1..].parse().unwrap_or(default_port)
            } else {
                default_port
            };
            return (host.to_string(), port);
        }
    }
    if let Some(colon) = target.rfind(':') {
        let host = &target[..colon];
        let port = target[colon + 1..].parse().unwrap_or(default_port);
        (host.to_string(), port)
    } else {
        (target.to_string(), default_port)
    }
}

async fn connect_with_failover(
    wan: &WanInterfaceInfo,
    target_addrs: &[SocketAddr],
    wan_interfaces: &Arc<RwLock<Vec<WanInterfaceInfo>>>,
) -> std::io::Result<(TcpStream, u8)> {
    if target_addrs.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "No resolved IP addresses"));
    }

    // Try chosen interface first
    match try_wan_connect(wan, target_addrs).await {
        Ok(stream) => return Ok((stream, wan.path_id)),
        Err(e) => {
            info!("Primary WAN Path {} (idx: {}, ip: {:?}) failed to connect: {}", wan.path_id, wan.if_index, wan.ipv4, e);
        }
    }

    // Failover: Try any other available WAN interfaces
    let wans = wan_interfaces.read().clone();
    for alt_wan in &wans {
        if alt_wan.path_id != wan.path_id {
            match try_wan_connect(alt_wan, target_addrs).await {
                Ok(stream) => {
                    info!("Failover to WAN Path {} (idx: {}, ip: {:?}) succeeded", alt_wan.path_id, alt_wan.if_index, alt_wan.ipv4);
                    return Ok((stream, alt_wan.path_id));
                }
                Err(e) => {
                    info!("Alt WAN Path {} (idx: {}, ip: {:?}) failed: {}", alt_wan.path_id, alt_wan.if_index, alt_wan.ipv4, e);
                }
            }
        }
    }

    Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "All WAN interfaces failed to connect"))
}

async fn try_wan_connect(
    wan: &WanInterfaceInfo,
    target_addrs: &[SocketAddr],
) -> std::io::Result<TcpStream> {
    let mut candidates = Vec::new();
    for &addr in target_addrs {
        if addr.is_ipv4() && wan.ipv4.is_some() {
            candidates.push(addr);
        } else if addr.is_ipv6() && wan.ipv6.is_some() {
            candidates.push(addr);
        }
    }

    if candidates.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Interface lacks matching IP version for target",
        ));
    }

    let mut last_err = None;
    for &remote in &candidates {
        let wan_ipv4 = wan.ipv4;
        let wan_ipv6 = wan.ipv6;
        let if_index = wan.if_index;

        let connect_timeout = if remote.is_ipv6() {
            std::time::Duration::from_millis(1500)
        } else {
            std::time::Duration::from_millis(2500)
        };

        match tokio::time::timeout(
            connect_timeout,
            bind_and_connect(wan_ipv4, wan_ipv6, if_index, remote),
        ).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => {
                info!("bind_and_connect failed for WAN {} ({:?}) to {}: {}", wan.path_id, wan_ipv4, remote, e);
                last_err = Some(e);
            }
            Err(_) => {
                info!("bind_and_connect timed out for WAN {} ({:?}) to {}", wan.path_id, wan_ipv4, remote);
                last_err = Some(std::io::Error::new(std::io::ErrorKind::TimedOut, "Connect timed out"));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "All candidates failed")))
}

/// Ultra-high performance lock-free zero-contention bidirectional stream splicer.
/// Implements RFC-compliant full-duplex TCP half-close with owned read/write halves
/// so downloads never stall and completed streams terminate immediately without hanging.
async fn splice_bidirectional(
    client: TcpStream,
    remote: TcpStream,
    active_pid: u8,
) {
    let _ = client.set_nodelay(true);
    let _ = remote.set_nodelay(true);

    let (mut cr, mut cw) = client.into_split();
    let (mut rr, mut rw) = remote.into_split();

    let pid = (active_pid as usize).min(15);

    let client_to_remote = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match cr.read(&mut buf).await {
                Ok(0) => {
                    let _ = rw.shutdown().await;
                    break;
                }
                Ok(n) => {
                    if rw.write_all(&buf[..n]).await.is_err() {
                        let _ = rw.shutdown().await;
                        break;
                    }
                    PROXY_PATH_TX_BYTES[pid].fetch_add(n as u64, Ordering::Relaxed);
                    PROXY_PATH_TX_PACKETS[pid].fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    let _ = rw.shutdown().await;
                    break;
                }
            }
        }
    });

    let remote_to_client = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match rr.read(&mut buf).await {
                Ok(0) => {
                    let _ = cw.shutdown().await;
                    break;
                }
                Ok(n) => {
                    if cw.write_all(&buf[..n]).await.is_err() {
                        let _ = cw.shutdown().await;
                        break;
                    }
                    PROXY_PATH_RX_BYTES[pid].fetch_add(n as u64, Ordering::Relaxed);
                    PROXY_PATH_RX_PACKETS[pid].fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    let _ = cw.shutdown().await;
                    break;
                }
            }
        }
    });

    let _ = tokio::join!(client_to_remote, remote_to_client);
}

/// Asynchronously bind a TCP socket strictly to a physical network adapter's local IP and interface index
/// using Winsock IP_UNICAST_IF / IPV6_UNICAST_IF with TCP_NODELAY and native Windows dynamic autotuning.
async fn bind_and_connect(
    local_ipv4: Option<Ipv4Addr>,
    local_ipv6: Option<Ipv6Addr>,
    if_index: u32,
    remote_addr: SocketAddr,
) -> std::io::Result<TcpStream> {
    let domain = match remote_addr {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;

    // 1. Critical for low latency: Disable Nagle's algorithm (TCP_NODELAY)
    let _ = socket.set_nodelay(true);

    // 2. Force outbound interface selection via IP_UNICAST_IF / IPV6_UNICAST_IF on Windows
    if if_index > 0 {
        unsafe {
            match remote_addr {
                SocketAddr::V4(_) => {
                    let net_if_index = if_index.to_be();
                    windows_sys::Win32::Networking::WinSock::setsockopt(
                        socket.as_raw_socket() as _,
                        0,  // IPPROTO_IP
                        31, // IP_UNICAST_IF
                        &net_if_index as *const _ as *const u8,
                        std::mem::size_of::<u32>() as i32,
                    );
                }
                SocketAddr::V6(_) => {
                    let host_if_index = if_index;
                    windows_sys::Win32::Networking::WinSock::setsockopt(
                        socket.as_raw_socket() as _,
                        41, // IPPROTO_IPV6
                        31, // IPV6_UNICAST_IF
                        &host_if_index as *const _ as *const u8,
                        std::mem::size_of::<u32>() as i32,
                    );
                }
            }
        }
    }

    // 3. Bind to source interface address
    match remote_addr {
        SocketAddr::V4(_) => {
            let bind_addr = local_ipv4
                .map(|ip| SocketAddrV4::new(ip, 0))
                .unwrap_or(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
            if socket.bind(&bind_addr.into()).is_err() {
                let any = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
                let _ = socket.bind(&any.into());
            }
        }
        SocketAddr::V6(_) => {
            let bind_addr = local_ipv6
                .map(|ip| SocketAddrV6::new(ip, 0, 0, 0))
                .unwrap_or(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0));
            if socket.bind(&bind_addr.into()).is_err() {
                let any = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
                let _ = socket.bind(&any.into());
            }
        }
    }

    match socket.connect(&remote_addr.into()) {
        Ok(()) => {
            let std_stream: std::net::TcpStream =
                unsafe { std::net::TcpStream::from_raw_socket(socket.as_raw_socket() as _) };
            std::mem::forget(socket);
            let s = TcpStream::from_std(std_stream)?;
            let _ = s.set_nodelay(true);
            Ok(s)
        }
        Err(err)
            if err.raw_os_error() == Some(10035) // WSAEWOULDBLOCK
                || err.kind() == std::io::ErrorKind::WouldBlock =>
        {
            let raw_sock = socket.as_raw_socket() as usize;
            let timeout_ms: u32 = if remote_addr.is_ipv6() { 1500 } else { 2500 };

            // On Windows, non-blocking connect does NOT generate IOCP completion events.
            // Winsock select() must be used to reliably wait for FD_CONNECT completion in milliseconds.
            let wait_res = tokio::task::spawn_blocking(move || {
                unsafe {
                    use windows_sys::Win32::Networking::WinSock::{
                        getsockopt, select, FD_SET, TIMEVAL, SOL_SOCKET, SO_ERROR,
                    };
                    let mut write_fds: FD_SET = std::mem::zeroed();
                    write_fds.fd_count = 1;
                    write_fds.fd_array[0] = raw_sock;

                    let mut except_fds: FD_SET = std::mem::zeroed();
                    except_fds.fd_count = 1;
                    except_fds.fd_array[0] = raw_sock;

                    let tv = TIMEVAL {
                        tv_sec: (timeout_ms / 1000) as i32,
                        tv_usec: ((timeout_ms % 1000) * 1000) as i32,
                    };

                    let sel = select(0, std::ptr::null_mut(), &mut write_fds, &mut except_fds, &tv);
                    if sel <= 0 {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "TCP connect timeout"));
                    }

                    let mut err_code: i32 = 0;
                    let mut err_len = std::mem::size_of::<i32>() as i32;
                    getsockopt(
                        raw_sock as _,
                        SOL_SOCKET,
                        SO_ERROR,
                        &mut err_code as *mut _ as *mut u8,
                        &mut err_len,
                    );

                    if err_code != 0 {
                        return Err(std::io::Error::from_raw_os_error(err_code));
                    }

                    Ok(())
                }
            }).await.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

            wait_res?;

            let std_stream: std::net::TcpStream =
                unsafe { std::net::TcpStream::from_raw_socket(socket.as_raw_socket() as _) };
            std::mem::forget(socket);
            let stream = TcpStream::from_std(std_stream)?;
            let _ = stream.set_nodelay(true);
            Ok(stream)
        }
        Err(err) => Err(err),
    }
}

/// Spawns the background link reachability prober.
/// Periodically verifies IPv4 and IPv6 internet routing on each adapter
/// to ensure that asymmetric IPv6 cannot funnel traffic to a single interface.
pub fn spawn_link_prober(
    wan_interfaces: Arc<RwLock<Vec<WanInterfaceInfo>>>,
    scheduler: Arc<RwLock<AdaptiveScheduler>>,
) {
    tokio::spawn(async move {
        // Immediate first probe on startup
        probe_all_wans(&wan_interfaces, &scheduler).await;

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            probe_all_wans(&wan_interfaces, &scheduler).await;
        }
    });
}

async fn probe_all_wans(
    wan_interfaces: &Arc<RwLock<Vec<WanInterfaceInfo>>>,
    scheduler: &Arc<RwLock<AdaptiveScheduler>>,
) {
    let wans = wan_interfaces.read().clone();
    for wan in wans {
        let wan_v4 = wan.ipv4;
        let wan_v6 = wan.ipv6;
        let if_idx = wan.if_index;
        let pid = wan.path_id;

        let v4_up = if wan_v4.is_some() {
            probe_target_v4(wan_v4, if_idx, "1.1.1.1:443").await
                || probe_target_v4(wan_v4, if_idx, "8.8.8.8:443").await
        } else {
            false
        };

        let v6_up = if wan_v6.is_some() {
            probe_target_v6(wan_v6, if_idx, "[2606:4700:4700::1111]:443").await
                || probe_target_v6(wan_v6, if_idx, "[2001:4860:4860::8888]:443").await
        } else {
            false
        };

        {
            let mut sched = scheduler.write();
            if let Some(p) = sched.paths_mut().iter_mut().find(|p| p.path_id == pid) {
                p.is_v4_up = v4_up;
                p.is_v6_up = v6_up;
            }
        }
    }
}

async fn probe_target_v4(local_ip: Option<Ipv4Addr>, if_index: u32, target: &str) -> bool {
    let remote: SocketAddr = match target.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    tokio::time::timeout(
        std::time::Duration::from_millis(2000),
        bind_and_connect(local_ip, None, if_index, remote),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

async fn probe_target_v6(local_ip: Option<Ipv6Addr>, if_index: u32, target: &str) -> bool {
    let remote: SocketAddr = match target.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    tokio::time::timeout(
        std::time::Duration::from_millis(2000),
        bind_and_connect(None, local_ip, if_index, remote),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// Enable Windows system proxy for 127.0.0.1:8080 and refresh WinINet via native Win32 APIs (no cmd flicker)
pub fn enable_windows_proxy(port: u16) {
    let proxy_server = format!("127.0.0.1:{}", port);
    info!("Enabling Windows system proxy: {}", proxy_server);

    unsafe {
        let subkey = to_wide_proxy_str(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");
        let mut hkey: windows_sys::Win32::System::Registry::HKEY = 0;
        if windows_sys::Win32::System::Registry::RegOpenKeyExW(
            windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            windows_sys::Win32::System::Registry::KEY_SET_VALUE,
            &mut hkey,
        ) == 0 {
            let enable_val: u32 = 1;
            let val_name_enable = to_wide_proxy_str("ProxyEnable");
            let _ = windows_sys::Win32::System::Registry::RegSetValueExW(
                hkey,
                val_name_enable.as_ptr(),
                0,
                windows_sys::Win32::System::Registry::REG_DWORD,
                &enable_val as *const u32 as *const u8,
                4,
            );

            let val_name_server = to_wide_proxy_str("ProxyServer");
            let server_w = to_wide_proxy_str(&proxy_server);
            let _ = windows_sys::Win32::System::Registry::RegSetValueExW(
                hkey,
                val_name_server.as_ptr(),
                0,
                windows_sys::Win32::System::Registry::REG_SZ,
                server_w.as_ptr() as *const u8,
                (server_w.len() * 2) as u32,
            );

            // Bypass proxy for local addresses to prevent loopback recursion
            let val_name_override = to_wide_proxy_str("ProxyOverride");
            let override_w = to_wide_proxy_str("<local>;127.*;10.*;192.168.*");
            let _ = windows_sys::Win32::System::Registry::RegSetValueExW(
                hkey,
                val_name_override.as_ptr(),
                0,
                windows_sys::Win32::System::Registry::REG_SZ,
                override_w.as_ptr() as *const u8,
                (override_w.len() * 2) as u32,
            );

            windows_sys::Win32::System::Registry::RegCloseKey(hkey);
        }

        refresh_wininet();
    }
}

/// Disable Windows system proxy and restore direct internet access via native Win32 APIs
pub fn disable_windows_proxy() {
    info!("Disabling Windows system proxy. Restoring direct internet.");

    unsafe {
        let subkey = to_wide_proxy_str(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");
        let mut hkey: windows_sys::Win32::System::Registry::HKEY = 0;
        if windows_sys::Win32::System::Registry::RegOpenKeyExW(
            windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            windows_sys::Win32::System::Registry::KEY_SET_VALUE,
            &mut hkey,
        ) == 0 {
            let enable_val: u32 = 0;
            let val_name_enable = to_wide_proxy_str("ProxyEnable");
            let _ = windows_sys::Win32::System::Registry::RegSetValueExW(
                hkey,
                val_name_enable.as_ptr(),
                0,
                windows_sys::Win32::System::Registry::REG_DWORD,
                &enable_val as *const u32 as *const u8,
                4,
            );
            windows_sys::Win32::System::Registry::RegCloseKey(hkey);
        }

        refresh_wininet();
    }
}

fn refresh_wininet() {
    unsafe {
        windows_sys::Win32::Networking::WinInet::InternetSetOptionW(
            std::ptr::null(),
            windows_sys::Win32::Networking::WinInet::INTERNET_OPTION_SETTINGS_CHANGED,
            std::ptr::null(),
            0,
        );
        windows_sys::Win32::Networking::WinInet::InternetSetOptionW(
            std::ptr::null(),
            windows_sys::Win32::Networking::WinInet::INTERNET_OPTION_REFRESH,
            std::ptr::null(),
            0,
        );
    }
}

fn to_wide_proxy_str(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Independent watchdog process monitor that waits for parent process termination.
/// If NetFusion crashes, is force-killed, or closes, this immediately restores direct internet.
pub fn run_watchdog_loop(parent_pid: u32) {
    unsafe {
        const SYNCHRONIZE: u32 = 0x00100000;
        let h_proc = windows_sys::Win32::System::Threading::OpenProcess(
            SYNCHRONIZE,
            0,
            parent_pid,
        );
        if h_proc != 0 {
            windows_sys::Win32::System::Threading::WaitForSingleObject(
                h_proc,
                windows_sys::Win32::System::Threading::INFINITE,
            );
            windows_sys::Win32::Foundation::CloseHandle(h_proc);
            disable_windows_proxy();
        }
    }
}
