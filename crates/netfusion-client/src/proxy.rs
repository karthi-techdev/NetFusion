use log::{info, warn};
use netfusion_core::scheduler::AdaptiveScheduler;
use parking_lot::RwLock;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// High-performance local Multi-WAN dispatcher proxy
/// Automatically distributes parallel web connections (Google speed test, Chrome, Edge)
/// across independent physical network interfaces with zero remote VPS required.
pub struct LocalDispatcher {
    listen_port: u16,
    scheduler: Arc<RwLock<AdaptiveScheduler>>,
    wan_interfaces: Arc<RwLock<Vec<(u8, String, Ipv4Addr, u32)>>>,
    bonding_active: Arc<AtomicBool>,
}

impl LocalDispatcher {
    pub fn new(
        listen_port: u16,
        scheduler: Arc<RwLock<AdaptiveScheduler>>,
        wan_interfaces: Arc<RwLock<Vec<(u8, String, Ipv4Addr, u32)>>>,
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
                    let scheduler = self.scheduler.clone();
                    let wan_interfaces = self.wan_interfaces.clone();
                    let is_active = self.bonding_active.load(Ordering::Relaxed);

                    tokio::spawn(async move {
                        if !is_active {
                            return;
                        }
                        let _ = handle_client(client_stream, scheduler, wan_interfaces).await;
                    });
                }
                Err(e) => {
                    warn!("Local dispatcher accept error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }
}

async fn handle_client(
    mut client: TcpStream,
    scheduler: Arc<RwLock<AdaptiveScheduler>>,
    wan_interfaces: Arc<RwLock<Vec<(u8, String, Ipv4Addr, u32)>>>,
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
        let target_host = if let Some(idx) = target.find(':') {
            &target[..idx]
        } else {
            target
        };
        let target_port: u16 = if let Some(idx) = target.find(':') {
            target[idx + 1..].parse().unwrap_or(443)
        } else {
            443
        };

        // Select WAN path using fair round-robin alternation to strictly utilize ALL connections simultaneously
        let path_id = {
            let mut sched = scheduler.write();
            sched.select_next_path().unwrap_or(0)
        };

        let target_wan = {
            let wans = wan_interfaces.read();
            wans.iter()
                .find(|(id, _, _, _)| *id == path_id)
                .cloned()
                .or_else(|| wans.first().cloned())
        };

        if let Some((pid, _name, local_ip, if_index)) = target_wan {
            let target_addr_opt = match tokio::time::timeout(
                std::time::Duration::from_millis(3000),
                tokio::net::lookup_host(format!("{}:{}", target_host, target_port)),
            ).await {
                Ok(Ok(mut it)) => it.find(|a| a.is_ipv4()),
                _ => None,
            };

            if let Some(target_sockaddr) = target_addr_opt {
                // Attempt connection via chosen interface with 2.5s timeout
                let mut connect_res = match tokio::time::timeout(
                    std::time::Duration::from_millis(2500),
                    bind_and_connect(local_ip, if_index, target_sockaddr),
                ).await {
                    Ok(res) => res,
                    Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Interface connect timeout")),
                };
                let mut active_pid = pid;

                // Failover: if connection on this path fails, try other available interfaces
                if connect_res.is_err() {
                    let wans = wan_interfaces.read().clone();
                    for (alt_pid, _, alt_ip, alt_idx) in &wans {
                        if *alt_pid != pid {
                            let alt_res = match tokio::time::timeout(
                                std::time::Duration::from_millis(2500),
                                bind_and_connect(*alt_ip, *alt_idx, target_sockaddr),
                            ).await {
                                Ok(res) => res,
                                Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Interface connect timeout")),
                            };
                            if let Ok(stream) = alt_res {
                                connect_res = Ok(stream);
                                active_pid = *alt_pid;
                                break;
                            }
                        }
                    }
                }

                match connect_res {
                    Ok(remote_stream) => {
                        // Acknowledge connection established to browser
                        client
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .await?;

                        // Splice bidirectionally with real-time byte tracking per interface
                        let (mut cr, mut cw) = client.into_split();
                        let (mut rr, mut rw) = remote_stream.into_split();

                        let sched1 = scheduler.clone();
                        let t1 = tokio::spawn(async move {
                            let mut temp_buf = vec![0u8; 32768];
                            while let Ok(read_bytes) = cr.read(&mut temp_buf).await {
                                if read_bytes == 0 {
                                    break;
                                }
                                if rw.write_all(&temp_buf[..read_bytes]).await.is_err() {
                                    break;
                                }
                                let mut s = sched1.write();
                                if let Some(p) = s.paths_mut().iter_mut().find(|p| p.path_id == active_pid) {
                                    p.tx_bytes += read_bytes as u64;
                                    p.tx_packets += 1;
                                }
                            }
                        });

                        let sched2 = scheduler.clone();
                        let t2 = tokio::spawn(async move {
                            let mut temp_buf = vec![0u8; 32768];
                            while let Ok(read_bytes) = rr.read(&mut temp_buf).await {
                                if read_bytes == 0 {
                                    break;
                                }
                                if cw.write_all(&temp_buf[..read_bytes]).await.is_err() {
                                    break;
                                }
                                let mut s = sched2.write();
                                if let Some(p) = s.paths_mut().iter_mut().find(|p| p.path_id == active_pid) {
                                    p.rx_bytes += read_bytes as u64;
                                    p.rx_packets += 1;
                                }
                            }
                        });

                        let _ = tokio::join!(t1, t2);
                    }
                    Err(_e) => {
                        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    }
                }
            } else {
                let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
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
            let target_host = if let Some(idx) = host.find(':') {
                host[..idx].to_string()
            } else {
                host.clone()
            };
            let target_port: u16 = if let Some(idx) = host.find(':') {
                host[idx + 1..].parse().unwrap_or(80)
            } else {
                80
            };

            let path_id = {
                let mut sched = scheduler.write();
                sched.select_next_path().unwrap_or(0)
            };

            let target_wan = {
                let wans = wan_interfaces.read();
                wans.iter()
                    .find(|(id, _, _, _)| *id == path_id)
                    .cloned()
                    .or_else(|| wans.first().cloned())
            };

            if let Some((pid, _name, local_ip, if_index)) = target_wan {
                let target_addr_opt = match tokio::time::timeout(
                    std::time::Duration::from_millis(3000),
                    tokio::net::lookup_host(format!("{}:{}", target_host, target_port)),
                ).await {
                    Ok(Ok(mut it)) => it.find(|a| a.is_ipv4()),
                    _ => None,
                };

                if let Some(target_sockaddr) = target_addr_opt {
                    let mut connect_res = match tokio::time::timeout(
                        std::time::Duration::from_millis(2500),
                        bind_and_connect(local_ip, if_index, target_sockaddr),
                    ).await {
                        Ok(res) => res,
                        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Interface connect timeout")),
                    };
                    let mut active_pid = pid;

                    if connect_res.is_err() {
                        let wans = wan_interfaces.read().clone();
                        for (alt_pid, _, alt_ip, alt_idx) in &wans {
                            if *alt_pid != pid {
                                let alt_res = match tokio::time::timeout(
                                    std::time::Duration::from_millis(2500),
                                    bind_and_connect(*alt_ip, *alt_idx, target_sockaddr),
                                ).await {
                                    Ok(res) => res,
                                    Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Interface connect timeout")),
                                };
                                if let Ok(stream) = alt_res {
                                    connect_res = Ok(stream);
                                    active_pid = *alt_pid;
                                    break;
                                }
                            }
                        }
                    }

                    if let Ok(mut remote_stream) = connect_res {
                        // Forward original request
                        remote_stream.write_all(&buf[..n]).await?;
                        {
                            let mut s = scheduler.write();
                            if let Some(p) = s.paths_mut().iter_mut().find(|p| p.path_id == active_pid) {
                                p.tx_bytes += n as u64;
                                p.tx_packets += 1;
                            }
                        }

                        let (mut cr, mut cw) = client.into_split();
                        let (mut rr, mut rw) = remote_stream.into_split();

                        let sched1 = scheduler.clone();
                        let t1 = tokio::spawn(async move {
                            let mut temp_buf = vec![0u8; 32768];
                            while let Ok(read_bytes) = cr.read(&mut temp_buf).await {
                                if read_bytes == 0 {
                                    break;
                                }
                                if rw.write_all(&temp_buf[..read_bytes]).await.is_err() {
                                    break;
                                }
                                let mut s = sched1.write();
                                if let Some(p) = s.paths_mut().iter_mut().find(|p| p.path_id == active_pid) {
                                    p.tx_bytes += read_bytes as u64;
                                    p.tx_packets += 1;
                                }
                            }
                        });

                        let sched2 = scheduler.clone();
                        let t2 = tokio::spawn(async move {
                            let mut temp_buf = vec![0u8; 32768];
                            while let Ok(read_bytes) = rr.read(&mut temp_buf).await {
                                if read_bytes == 0 {
                                    break;
                                }
                                if cw.write_all(&temp_buf[..read_bytes]).await.is_err() {
                                    break;
                                }
                                let mut s = sched2.write();
                                if let Some(p) = s.paths_mut().iter_mut().find(|p| p.path_id == active_pid) {
                                    p.rx_bytes += read_bytes as u64;
                                    p.rx_packets += 1;
                                }
                            }
                        });

                        let _ = tokio::join!(t1, t2);
                    } else {
                        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    }
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

/// Asynchronously bind a TCP socket strictly to a physical network adapter's local IP and interface index
/// using Winsock IP_UNICAST_IF to guarantee traffic routes directly through the chosen physical adapter.
async fn bind_and_connect(
    local_ip: Ipv4Addr,
    if_index: u32,
    remote_addr: SocketAddr,
) -> std::io::Result<TcpStream> {
    let domain = if remote_addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;

    // 1. Force outbound interface selection via IP_UNICAST_IF on Windows
    // On Windows, if_index must be passed in network byte order (big-endian).
    if if_index > 0 {
        let net_if_index = if_index.to_be();
        unsafe {
            windows_sys::Win32::Networking::WinSock::setsockopt(
                socket.as_raw_socket() as _,
                0, // IPPROTO_IP
                31, // IP_UNICAST_IF
                &net_if_index as *const _ as *const u8,
                std::mem::size_of::<u32>() as i32,
            );
        }
    }

    // 2. Bind to source interface address
    let bind_addr = SocketAddrV4::new(local_ip, 0);
    if socket.bind(&bind_addr.into()).is_err() {
        let any_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
        let _ = socket.bind(&any_addr.into());
    }

    match socket.connect(&remote_addr.into()) {
        Ok(()) => {
            let std_stream: std::net::TcpStream =
                unsafe { std::net::TcpStream::from_raw_socket(socket.as_raw_socket() as _) };
            std::mem::forget(socket);
            TcpStream::from_std(std_stream)
        }
        Err(err)
            if err.raw_os_error() == Some(10035) // WSAEWOULDBLOCK
                || err.kind() == std::io::ErrorKind::WouldBlock =>
        {
            let std_stream: std::net::TcpStream =
                unsafe { std::net::TcpStream::from_raw_socket(socket.as_raw_socket() as _) };
            std::mem::forget(socket);
            let stream = TcpStream::from_std(std_stream)?;
            match tokio::time::timeout(std::time::Duration::from_millis(2000), stream.writable()).await {
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

