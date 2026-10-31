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

async fn cached_resolve(host: &str, port: u16) -> Vec<SocketAddr> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return vec![SocketAddr::new(ip, port)];
    }

    let key = format!("{}:{}", host, port);
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

    let addrs: Vec<SocketAddr> = match tokio::time::timeout(
        std::time::Duration::from_millis(2000),
        tokio::net::lookup_host(&key),
    ).await {
        Ok(Ok(iter)) => iter.collect(),
        _ => Vec::new(),
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

#[allow(dead_code)]
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

        // Select WAN path using fair round-robin alternation across active links
        let path_id = if is_active {
            let mut sched = scheduler.write();
            sched.select_next_path().unwrap_or(0)
        } else {
            0
        };

        let target_wan = {
            let wans = wan_interfaces.read();
            wans.iter()
                .find(|w| w.path_id == path_id)
                .cloned()
                .or_else(|| wans.first().cloned())
        };

        if let Some(wan) = target_wan {
            let target_addrs = cached_resolve(&target_host, target_port).await;
            let connect_res = connect_with_failover(&wan, &target_addrs, &wan_interfaces).await;

            match connect_res {
                Ok((remote_stream, active_pid)) => {
                    client
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await?;

                    splice_bidirectional(client, remote_stream, active_pid).await;
                }
                Err(_) => {
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                }
            }
        } else {
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

            let path_id = if is_active {
                let mut sched = scheduler.write();
                sched.select_next_path().unwrap_or(0)
            } else {
                0
            };

            let target_wan = {
                let wans = wan_interfaces.read();
                wans.iter()
                    .find(|w| w.path_id == path_id)
                    .cloned()
                    .or_else(|| wans.first().cloned())
            };

            if let Some(wan) = target_wan {
                let target_addrs = cached_resolve(&target_host, target_port).await;
                let connect_res = connect_with_failover(&wan, &target_addrs, &wan_interfaces).await;

                if let Ok((mut remote_stream, active_pid)) = connect_res {
                    let payload = if first_line.contains(" http://") {
                        let parts: Vec<&str> = first_line.split_whitespace().collect();
                        if parts.len() >= 3 {
                            let method = parts[0];
                            let uri = parts[1];
                            let version = parts[2];
                            if let Some(slash_idx) = uri[7..].find('/') {
                                let path = &uri[7 + slash_idx..];
                                let new_first = format!("{} {} {}\r\n", method, path, version);
                                let rest = &req_str[first_line.len()..];
                                let rest = rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix("\n")).unwrap_or(rest);
                                format!("{}{}", new_first, rest).into_bytes()
                            } else {
                                buf[..n].to_vec()
                            }
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
    if let Ok(stream) = try_wan_connect(wan, target_addrs).await {
        return Ok((stream, wan.path_id));
    }

    // Failover: Try any other available WAN interfaces
    let wans = wan_interfaces.read().clone();
    for alt_wan in &wans {
        if alt_wan.path_id != wan.path_id {
            if let Ok(stream) = try_wan_connect(alt_wan, target_addrs).await {
                return Ok((stream, alt_wan.path_id));
            }
        }
    }

    Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "All WAN interfaces failed to connect"))
}

async fn try_wan_connect(
    wan: &WanInterfaceInfo,
    target_addrs: &[SocketAddr],
) -> std::io::Result<TcpStream> {
    // Filter candidates according to WAN interface capabilities
    let mut candidates: Vec<SocketAddr> = target_addrs
        .iter()
        .copied()
        .filter(|a| {
            (a.is_ipv4() && wan.ipv4.is_some()) || (a.is_ipv6() && wan.ipv6.is_some())
        })
        .collect();

    if candidates.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "Interface lacks matching IP version"));
    }

    // High performance connection prioritization:
    // When dual-stack targets are resolved, prioritize IPv4 for instant (<15ms) zero-latency connect.
    // If the target is IPv6-only or has IPv6 candidates, IPv6 is still tried with a fast timeout.
    candidates.sort_by_key(|a| if a.is_ipv4() { 0 } else { 1 });

    for remote in candidates {
        let connect_timeout = if remote.is_ipv6() {
            std::time::Duration::from_millis(350)
        } else {
            std::time::Duration::from_millis(1500)
        };

        match tokio::time::timeout(
            connect_timeout,
            bind_and_connect(wan.ipv4, wan.ipv6, wan.if_index, remote),
        ).await {
            Ok(Ok(stream)) => return Ok(stream),
            _ => continue,
        }
    }

    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Interface connect failed"))
}

/// Ultra-high performance lock-free zero-contention bidirectional stream splicer.
/// Uses 64KB stack buffers, TCP_NODELAY, atomic bandwidth counters, and clean half-close termination.
async fn splice_bidirectional(
    mut client: TcpStream,
    mut remote: TcpStream,
    active_pid: u8,
) {
    let _ = client.set_nodelay(true);
    let _ = remote.set_nodelay(true);

    let (mut cr, mut cw) = client.split();
    let (mut rr, mut rw) = remote.split();

    let pid = (active_pid as usize).min(15);

    let client_to_remote = async {
        let mut buf = [0u8; 65536];
        loop {
            match cr.read(&mut buf).await {
                Ok(0) => {
                    let _ = rw.shutdown().await;
                    break;
                }
                Ok(n) => {
                    if rw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    PROXY_PATH_TX_BYTES[pid].fetch_add(n as u64, Ordering::Relaxed);
                    PROXY_PATH_TX_PACKETS[pid].fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => break,
            }
        }
    };

    let remote_to_client = async {
        let mut buf = [0u8; 65536];
        loop {
            match rr.read(&mut buf).await {
                Ok(0) => {
                    let _ = cw.shutdown().await;
                    break;
                }
                Ok(n) => {
                    if cw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    PROXY_PATH_RX_BYTES[pid].fetch_add(n as u64, Ordering::Relaxed);
                    PROXY_PATH_RX_PACKETS[pid].fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => break,
            }
        }
    };

    tokio::select! {
        _ = client_to_remote => {},
        _ = remote_to_client => {},
    }
}

/// Asynchronously bind a TCP socket strictly to a physical network adapter's local IP and interface index
/// using Winsock IP_UNICAST_IF / IPV6_UNICAST_IF with TCP_NODELAY.
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

    // 2. High throughput buffer tuning
    let _ = socket.set_recv_buffer_size(1024 * 1024);
    let _ = socket.set_send_buffer_size(1024 * 1024);

    // 3. Force outbound interface selection via IP_UNICAST_IF / IPV6_UNICAST_IF on Windows
    // MUST BE CALLED BEFORE BIND SO WINSOCK ALIGNS ROUTING AND EPHEMERAL PORT SELECTION!
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

    // 4. Bind to source interface address
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
            let std_stream: std::net::TcpStream =
                unsafe { std::net::TcpStream::from_raw_socket(socket.as_raw_socket() as _) };
            std::mem::forget(socket);
            let stream = TcpStream::from_std(std_stream)?;
            let _ = stream.set_nodelay(true);
            let connect_timeout = if remote_addr.is_ipv6() {
                std::time::Duration::from_millis(350)
            } else {
                std::time::Duration::from_millis(1500)
            };
            match tokio::time::timeout(connect_timeout, stream.writable()).await {
                Ok(Ok(())) => {
                    if let Some(e) = stream.take_error()? {
                        return Err(e);
                    }
                    Ok(stream)
                }
                Ok(Err(e)) => Err(e),
                Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "TCP connect timeout")),
            }
        }
        Err(err) => Err(err),
    }
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
