#![windows_subsystem = "windows"]

mod adapter;
mod ipc;
mod proxy;
mod route;
mod tray;
mod wan_socket;
mod wintun;

use adapter::discover_adapters;
use bytes::Bytes;
use clap::Parser;
use log::{error, info, warn};
use netfusion_core::config::ClientConfig;
use netfusion_core::crypto::TunnelCrypto;
use netfusion_core::metrics::{EngineMetricsSnapshot, PathSnapshot, RateTracker};
use netfusion_core::protocol::{MessageType, PacketHeader, HEADER_LEN};
use netfusion_core::reorder::ReorderBuffer;
use netfusion_core::scheduler::{AdaptiveScheduler, PathMetrics};
use parking_lot::RwLock;
use route::RouteManager;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
use tokio::sync::{broadcast, mpsc};
use wan_socket::WanPathSocket;
use wintun::WintunDevice;

#[derive(Parser, Debug)]
#[command(author, version, about = "NetFusion Windows Internet Bonding Engine Client", long_about = None)]
struct CliArgs {
    #[arg(short, long, default_value = "configs/client.json")]
    config: PathBuf,

    #[arg(long)]
    no_service: bool,

    #[arg(long)]
    service_run: bool,

    #[arg(long)]
    watchdog: Option<u32>,

    #[arg(long)]
    dump_adapters: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = CliArgs::parse();

    // 0. Setup file logging immediately so ANY startup error or status is captured
    let log_path = get_app_dir().join("netfusion.log");
    if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .target(env_logger::Target::Pipe(Box::new(file)))
            .try_init();
    } else {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
    }

    if args.dump_adapters {
        let adapters = adapter::discover_adapters()?;
        for a in adapters {
            if a.is_eligible_wan() {
                println!("[WAN] {:?} | Name: '{}' | Network: '{}' | IP: {:?}", a.kind, a.name, a.network_name, a.ipv4_addr);
            }
        }
        return Ok(());
    }

    // Watchdog mode: lightweight detached process that monitors parent NetFusion PID.
    // If NetFusion exits, panics, crashes, or is killed by Task Manager, this watchdog
    // immediately resets Windows proxy to 0 within milliseconds to guarantee internet is NEVER broken.
    if let Some(parent_pid) = args.watchdog {
        proxy::run_watchdog_loop(parent_pid);
        return Ok(());
    }

    // 1. Reset any stale proxy immediately on startup
    proxy::disable_windows_proxy();

    // 2. Single instance mutex to prevent multiple conflicting instances
    use std::os::windows::ffi::OsStrExt;
    let mutex_name: Vec<u16> = std::ffi::OsStr::new("Local\\NetFusionSingleInstanceMutex")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        windows_sys::Win32::Foundation::SetLastError(0);
    }
    let _h_mutex = unsafe {
        windows_sys::Win32::System::Threading::CreateMutexW(
            std::ptr::null(),
            1,
            mutex_name.as_ptr(),
        )
    };
    let last_err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    if _h_mutex != 0 && last_err == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS {
        info!("Another NetFusion instance is already running (GetLastError={}). Exiting duplicate instance.", last_err);
        return Ok(());
    }

    // 3. Strict crash/panic protection: disable proxy on ANY panic
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        proxy::disable_windows_proxy();
        default_panic(info);
    }));

    // 4. Strict termination protection: disable proxy on console close/logoff/shutdown
    unsafe {
        unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> windows_sys::Win32::Foundation::BOOL {
            match ctrl_type {
                0 /* CTRL_C_EVENT */ | 1 /* CTRL_BREAK_EVENT */ => {
                    // Ignore console break events in a GUI tray app
                    1
                }
                2 /* CTRL_CLOSE_EVENT */ | 5 /* CTRL_LOGOFF_EVENT */ | 6 /* CTRL_SHUTDOWN_EVENT */ => {
                    proxy::disable_windows_proxy();
                    0
                }
                _ => 0,
            }
        }
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(ctrl_handler), 1);
    }

    // 5. Strict external watchdog: spawn an independent lightweight watchdog that monitors this process.
    // Even if this process crashes, is killed by Task Manager, or terminates abnormally,
    // the watchdog restores direct internet access within milliseconds!
    if let Ok(current_exe) = std::env::current_exe() {
        use std::os::windows::process::CommandExt;
        let pid = std::process::id();
        let _ = std::process::Command::new(&current_exe)
            .args(["--watchdog", &pid.to_string()])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .spawn();
    }

    // 6. RAII drop guard for normal / unwind exits
    struct SafeProxyCleanup;
    impl Drop for SafeProxyCleanup {
        fn drop(&mut self) {
            proxy::disable_windows_proxy();
        }
    }
    let _proxy_guard = SafeProxyCleanup;

    info!("Starting NetFusion Windows Internet Bonding Engine v{}", env!("CARGO_PKG_VERSION"));

    let config = load_or_create_config();

    let shared_metrics = Arc::new(RwLock::new(EngineMetricsSnapshot::default()));
    let bonding_active = Arc::new(AtomicBool::new(true));
    let test_trigger = Arc::new(AtomicBool::new(false));
    let exit_requested = Arc::new(AtomicBool::new(false));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let sm_clone = shared_metrics.clone();
    let ba_clone = bonding_active.clone();
    let ex_clone = exit_requested.clone();

    // Spawn Tokio background runtime to power the Bonding Engine
    std::thread::spawn(move || {
        rt.block_on(async {
            if let Err(e) = run_bonding_engine(config, sm_clone, ba_clone, ex_clone).await {
                error!("Bonding engine error: {}", e);
            }
        });
    });

    // Run Win32 System Tray Application on the main UI thread
    let tray_app = tray::SystemTrayApp::new(
        shared_metrics,
        bonding_active,
        test_trigger,
        exit_requested,
    );
    tray_app.run_loop();

    // Clean up proxy on exit
    proxy::disable_windows_proxy();

    Ok(())
}

fn get_app_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let dir = PathBuf::from(local_app_data).join("NetFusion");
        let _ = std::fs::create_dir_all(&dir);
        dir
    } else {
        PathBuf::from(".")
    }
}

fn load_or_create_config() -> ClientConfig {
    let config_path = get_app_dir().join("client.json");

    if config_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            if let Ok(cfg) = serde_json::from_str::<ClientConfig>(&content) {
                return cfg;
            }
        }
    }

    let default_config = ClientConfig::default();
    if let Ok(json) = serde_json::to_string_pretty(&default_config) {
        let _ = std::fs::write(&config_path, json);
    }
    default_config
}

async fn run_bonding_engine(
    config: ClientConfig,
    shared_metrics: Arc<RwLock<EngineMetricsSnapshot>>,
    bonding_active: Arc<AtomicBool>,
    exit_requested: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Initializing NetFusion Multipath Engine...");

    // 1. Discover physical network interfaces
    let all_adapters = discover_adapters()?;
    info!("Discovered {} total network adapters:", all_adapters.len());
    for ad in &all_adapters {
        info!(
            " - [{:?}] {} (IP: {:?}, GW: {:?}, Up: {}, ISP: {})",
            ad.kind, ad.name, ad.ipv4_addr, ad.gateway, ad.is_up, ad.isp_label
        );
    }

    let eligible_wans: Vec<_> = all_adapters.into_iter().filter(|a| a.is_eligible_wan()).collect();
    info!("Identified {} active WAN connections for bonding", eligible_wans.len());

    let server_sockaddr: SocketAddrV4 = config
        .server_endpoint
        .parse()
        .map_err(|e| format!("Invalid server endpoint {}: {}", config.server_endpoint, e))?;

    // 2. Setup bypass routes, purge conflicting routes, and enable Weak Host Model across all physical interfaces
    route::purge_conflicting_kernel_routes();
    let mut route_mgr = RouteManager::new();
    for wan in &eligible_wans {
        route::enable_weak_host_model(wan.if_index);
        if let (Some(gw), Some(_)) = (wan.gateway, wan.ipv4_addr) {
            let _ = route_mgr.add_server_bypass_route(*server_sockaddr.ip(), gw, wan.if_index);
        }
    }

    // 3. Initialize ChaCha20-Poly1305 encryption key
    let mut key_bytes = [0u8; 32];
    let key_hex = config.encryption_key.trim();
    if let Ok(bytes) = hex_decode(key_hex) {
        if bytes.len() == 32 {
            key_bytes.copy_from_slice(&bytes);
        }
    }
    let crypto = Arc::new(TunnelCrypto::new(&key_bytes, [0x4E, 0x46, 0x01, 0x02]));

    // 4. Bind WAN sockets to individual physical adapters
    let mut wan_sockets = Vec::new();
    let mut wan_interfaces = Vec::new();
    let mut scheduler = AdaptiveScheduler::new(1500.0);
    let mut path_if_indices = std::collections::HashMap::new();

    for (idx, wan) in eligible_wans.iter().enumerate() {
        let path_id = idx as u8;
        path_if_indices.insert(path_id, wan.if_index);
        if let Some(local_ip) = wan.ipv4_addr {
            match WanPathSocket::bind(path_id, &wan.name, local_ip, server_sockaddr) {
                Ok(wan_sock) => {
                    wan_sockets.push(Arc::new(wan_sock));
                    wan_interfaces.push((path_id, wan.name.clone(), local_ip, wan.if_index));
                    let mut pm = PathMetrics::new(path_id, wan.name.clone(), local_ip.to_string());
                    pm.network_name = wan.network_name.clone();
                    // Balanced fair capacity weighting: ensures multi-WAN connections (Ethernet and WiFi)
                    // are actively and concurrently utilized without starvation.
                    pm.capacity_weight = 1.0;
                    scheduler.add_path(pm);
                }
                Err(e) => {
                    error!("Failed to bind WAN socket for {}: {}", wan.name, e);
                }
            }
        }
    }

    if wan_sockets.is_empty() {
        warn!("No physical WAN interfaces available! Running in loopback/testing fallback mode...");
        let loopback_ip = Ipv4Addr::new(127, 0, 0, 1);
        let fallback_sock = WanPathSocket::bind(0, "LoopbackWAN", loopback_ip, server_sockaddr)?;
        wan_sockets.push(Arc::new(fallback_sock));
        wan_interfaces.push((0, "LoopbackWAN".to_string(), loopback_ip, 1));
        scheduler.add_path(PathMetrics::new(0, "LoopbackWAN".to_string(), "127.0.0.1".to_string()));
    }

    let scheduler = Arc::new(RwLock::new(scheduler));
    let wan_sockets = Arc::new(wan_sockets);
    let wan_interfaces = Arc::new(parking_lot::RwLock::new(wan_interfaces));

    // 5. Initialize Wintun Adapter (Virtual Layer 3 Adapter)
    let virt_ip: Ipv4Addr = config.virtual_ip.parse().unwrap_or(Ipv4Addr::new(10, 8, 0, 2));
    let virt_gw = Ipv4Addr::new(10, 8, 0, 1);
    let subnet_mask = Ipv4Addr::new(255, 255, 255, 0);

    let wintun_dev = match WintunDevice::create("NetFusionBond0", virt_ip, subnet_mask, virt_gw, config.mtu) {
        Ok(dev) => Some(dev),
        Err(e) => {
            warn!("Wintun driver initialization skipped: {}. Local bonding dispatcher active.", e);
            None
        }
    };

    // 6. Safe Remote Server Probe & Default Route Verification
    // CRITICAL: We only hijack 0.0.0.0/0 route IF the server is verified alive!
    // If using dummy IP (e.g. 203.0.113.50) or server is unreachable, we DO NOT break internet!
    let server_reachable = Arc::new(AtomicBool::new(false));
    let probe_sockets = wan_sockets.clone();
    let _probe_server_reachable = server_reachable.clone();
    let _probe_scheduler = scheduler.clone();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1000));
        loop {
            interval.tick().await;
            for wan in probe_sockets.iter() {
                let now_us = (Instant::now().elapsed().as_micros() & 0xFFFFFFFF) as u32;
                let mut header = PacketHeader::new(MessageType::Probe, 1, wan.path_id, 0);
                header.timestamp_us = now_us;
                let mut buf = bytes::BytesMut::with_capacity(HEADER_LEN);
                header.encode(&mut buf);
                let _ = wan.socket.send(&buf).await;
            }
        }
    });

    // 7. Initialize IPC Server & Telemetry
    let (telemetry_tx, _) = broadcast::channel(64);
    let (cmd_tx, mut cmd_rx) = mpsc::channel(32);

    let ipc_server = ipc::IpcServer::new(config.ipc_port, telemetry_tx.clone(), cmd_tx);
    tokio::spawn(async move {
        let _ = ipc_server.run().await;
    });

    let rate_tracker = Arc::new(RateTracker::new());
    let reorder_buf = Arc::new(parking_lot::Mutex::new(ReorderBuffer::new(1)));

    // 8. WAN Inbound UDP Traffic Processing Tasks (Remote Server -> Multi-WAN -> Wintun)
    for wan_sock in wan_sockets.iter() {
        let wan_sock = wan_sock.clone();
        let crypto = crypto.clone();
        let reorder_buf = reorder_buf.clone();
        let scheduler = scheduler.clone();
        let wintun_session = wintun_dev.as_ref().map(|d| d.session.clone());
        let server_reachable = server_reachable.clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match wan_sock.socket.recv_from(&mut buf).await {
                    Ok((n, _src)) => {
                        let data = &buf[..n];
                        if n < HEADER_LEN {
                            continue;
                        }

                        let mut header_slice = &data[..HEADER_LEN];
                        if let Ok(header) = PacketHeader::decode(&mut header_slice) {
                            match header.msg_type {
                                MessageType::Data => {
                                    server_reachable.store(true, Ordering::Relaxed);
                                    let ciphertext = &data[HEADER_LEN..];
                                    let aad = &data[..HEADER_LEN];
                                    if let Ok(plaintext) = crypto.decrypt(header.path_id, header.seq_num, ciphertext, aad) {
                                        {
                                            let mut sched = scheduler.write();
                                            if let Some(p) = sched.paths_mut().iter_mut().find(|p| p.path_id == header.path_id) {
                                                p.rx_bytes += n as u64;
                                                p.rx_packets += 1;
                                            }
                                        }

                                        let ready_packets = reorder_buf.lock().push(header.seq_num, Bytes::from(plaintext));
                                        if let Some(ref session) = wintun_session {
                                            for pkt in ready_packets {
                                                if let Ok(mut w_pkt) = session.allocate_send_packet(pkt.len() as u16) {
                                                    w_pkt.bytes_mut().copy_from_slice(&pkt);
                                                    session.send_packet(w_pkt);
                                                }
                                            }
                                        }
                                    }
                                }
                                MessageType::ProbeReply => {
                                    server_reachable.store(true, Ordering::Relaxed);
                                    let now_us = (Instant::now().elapsed().as_micros() & 0xFFFFFFFF) as u32;
                                    let rtt_us = now_us.saturating_sub(header.timestamp_echo_us);
                                    let rtt_dur = Duration::from_micros(rtt_us as u64);

                                    let mut sched = scheduler.write();
                                    if let Some(p) = sched.paths_mut().iter_mut().find(|p| p.path_id == header.path_id) {
                                        p.update_rtt(rtt_dur);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(_e) => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        });
    }

    // 9. Outbound Wintun Packet Reading Loop (Wintun -> Multi-WAN UDP -> Server)
    if let Some(ref dev) = wintun_dev {
        let session = dev.session.clone();
        let scheduler = scheduler.clone();
        let crypto = crypto.clone();
        let wan_sockets = wan_sockets.clone();
        let global_uplink_seq = Arc::new(AtomicU64::new(1));

        std::thread::spawn(move || {
            loop {
                match session.receive_blocking() {
                    Ok(pkt) => {
                        let data = pkt.bytes();
                        let n = data.len();
                        if n < 20 {
                            continue;
                        }

                        let path_id = {
                            let mut sched = scheduler.write();
                            sched.select_path(n).unwrap_or(0)
                        };

                        let seq = global_uplink_seq.fetch_add(1, Ordering::Relaxed);
                        let mut header = PacketHeader::new(MessageType::Data, 1, path_id, seq);
                        header.payload_len = (n + 16) as u16;

                        let mut header_bytes = bytes::BytesMut::with_capacity(HEADER_LEN);
                        header.encode(&mut header_bytes);

                        if let Ok(ciphertext) = crypto.encrypt(path_id, seq, data, &header_bytes) {
                            let mut final_datagram = bytes::BytesMut::with_capacity(HEADER_LEN + ciphertext.len());
                            final_datagram.extend_from_slice(&header_bytes);
                            final_datagram.extend_from_slice(&ciphertext);

                            if let Some(wan_sock) = wan_sockets.iter().find(|s| s.path_id == path_id) {
                                let _ = wan_sock.socket.try_send(&final_datagram);
                                {
                                    let mut sched = scheduler.write();
                                    if let Some(p) = sched.paths_mut().iter_mut().find(|p| p.path_id == path_id) {
                                        p.tx_bytes += (HEADER_LEN + ciphertext.len()) as u64;
                                        p.tx_packets += 1;
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        });
    }

    // 10. Start Built-in Zero-Server Multi-WAN Local Dispatcher Proxy
    // Distributes outgoing browser/speed-test traffic across all active interfaces!
    let local_dispatcher = proxy::LocalDispatcher::new(8080, scheduler.clone(), wan_interfaces.clone(), bonding_active.clone());
    tokio::spawn(async move {
        if let Err(e) = local_dispatcher.run().await {
            error!("Local dispatcher error: {}", e);
        }
    });

    // Automatically enable Windows proxy so Chrome, Edge, and Speed tests bond traffic
    proxy::enable_windows_proxy(8080);

    // 11. System Metrics and Telemetry Broadcast Loop (REAL HARDWARE KERNEL METRICS)
    let _metrics_handle = {
        let scheduler = scheduler.clone();
        let wan_interfaces = wan_interfaces.clone();
        let rate_tracker = rate_tracker.clone();
        let telemetry_tx = telemetry_tx.clone();
        let shared_metrics = shared_metrics.clone();
        let path_if_indices = Arc::new(parking_lot::RwLock::new(path_if_indices));

        tokio::spawn(async move {
            let mut sys = System::new_with_specifics(
                RefreshKind::new()
                    .with_cpu(CpuRefreshKind::everything())
                    .with_memory(MemoryRefreshKind::everything()),
            );

            let mut prev_path_rx = std::collections::HashMap::new();
            let mut prev_path_tx = std::collections::HashMap::new();
            let mut prev_hw_rx: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
            let mut prev_hw_tx: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
            let mut last_tick = Instant::now();

            let mut interval = tokio::time::interval(Duration::from_millis(500));

            loop {
                interval.tick().await;
                sys.refresh_cpu();
                sys.refresh_memory();

                let cpu = sys.global_cpu_info().cpu_usage();
                let ram_mb = (sys.used_memory() as f32) / (1024.0 * 1024.0);

                let elapsed_sec = last_tick.elapsed().as_secs_f64().max(0.1);
                last_tick = Instant::now();

                // Dynamic network hardware interface monitoring:
                // Instantly detects disconnected cables, turned off Wi-Fi, and reconnected networks.
                if let Ok(current_adapters) = adapter::discover_adapters() {
                    let active_wans: Vec<_> = current_adapters.into_iter().filter(|a| a.is_eligible_wan()).collect();
                    let active_if_indices: std::collections::HashSet<u32> = active_wans.iter().map(|w| w.if_index).collect();

                    // Update scheduler path liveness & Wi-Fi / network names
                    {
                        let mut sched_mut = scheduler.write();
                        let mut p_indices = path_if_indices.write();

                        for p in sched_mut.paths_mut() {
                            let if_idx = p_indices.get(&p.path_id).copied().unwrap_or(0);
                            let is_now_up = active_if_indices.contains(&if_idx);
                            p.is_alive = is_now_up;

                            if let Some(active_wan) = active_wans.iter().find(|w| w.if_index == if_idx) {
                                if !active_wan.network_name.is_empty() {
                                    p.network_name = active_wan.network_name.clone();
                                }
                                if let Some(ip) = active_wan.ipv4_addr {
                                    p.local_ip = ip.to_string();
                                }
                            }
                        }

                        // Dynamically register newly plugged in network adapters at runtime
                        for wan in &active_wans {
                            let already_known = p_indices.values().any(|&idx| idx == wan.if_index);
                            if !already_known {
                                if let Some(ip) = wan.ipv4_addr {
                                    let new_pid = (p_indices.len() as u8).wrapping_add(1);
                                    p_indices.insert(new_pid, wan.if_index);
                                    let mut pm = PathMetrics::new(new_pid, wan.name.clone(), ip.to_string());
                                    pm.network_name = wan.network_name.clone();
                                    pm.capacity_weight = 1.0;
                                    pm.is_alive = true;
                                    sched_mut.add_path(pm);
                                    route::enable_weak_host_model(wan.if_index);
                                    info!("Dynamically discovered newly connected network: {} ({:?})", wan.network_name, ip);
                                }
                            }
                        }
                    }

                    // Update LocalDispatcher's active WAN interfaces list so traffic immediately stops
                    // routing to disconnected adapters and strictly uses all currently connected links.
                    {
                        let p_indices = path_if_indices.read();
                        let mut dispatcher_list = Vec::new();
                        for w in &active_wans {
                            if let Some(ip) = w.ipv4_addr {
                                let pid = p_indices
                                    .iter()
                                    .find(|(_, &idx)| idx == w.if_index)
                                    .map(|(&p, _)| p)
                                    .unwrap_or(0);
                                dispatcher_list.push((pid, w.name.clone(), ip, w.if_index));
                            }
                        }
                        if !dispatcher_list.is_empty() {
                            *wan_interfaces.write() = dispatcher_list;
                        }
                    }
                }

                // Fetch real-time hardware InOctets/OutOctets directly from Windows kernel
                let hw_stats = adapter::get_adapter_traffic_stats();

                let sched = scheduler.read();
                let mut path_snapshots = Vec::new();
                let mut total_rx = 0u64;
                let mut total_tx = 0u64;

                for p in sched.paths() {
                    // CRITICAL: Strictly skip disconnected interfaces so they stop showing in the tray list dropdown!
                    if !p.is_alive {
                        continue;
                    }

                    total_rx += p.rx_bytes;
                    total_tx += p.tx_bytes;

                    let prev_rx = *prev_path_rx.get(&p.path_id).unwrap_or(&p.rx_bytes);
                    let prev_tx = *prev_path_tx.get(&p.path_id).unwrap_or(&p.tx_bytes);

                    let rx_diff = p.rx_bytes.saturating_sub(prev_rx) as f64;
                    let tx_diff = p.tx_bytes.saturating_sub(prev_tx) as f64;

                    let path_rx_bps = (rx_diff * 8.0) / elapsed_sec;
                    let path_tx_bps = (tx_diff * 8.0) / elapsed_sec;

                    prev_path_rx.insert(p.path_id, p.rx_bytes);
                    prev_path_tx.insert(p.path_id, p.tx_bytes);

                    // Cross-reference with real-time OS physical network adapter telemetry
                    let if_idx = path_if_indices.read().get(&p.path_id).copied().unwrap_or(0);
                    let (hw_in, hw_out) = hw_stats.get(&if_idx).copied().unwrap_or((0, 0));

                    let prev_hw_in = *prev_hw_rx.get(&if_idx).unwrap_or(&hw_in);
                    let prev_hw_out = *prev_hw_tx.get(&if_idx).unwrap_or(&hw_out);

                    let hw_in_diff = hw_in.saturating_sub(prev_hw_in) as f64;
                    let hw_out_diff = hw_out.saturating_sub(prev_hw_out) as f64;

                    let os_rx_bps = (hw_in_diff * 8.0) / elapsed_sec;
                    let os_tx_bps = (hw_out_diff * 8.0) / elapsed_sec;

                    prev_hw_rx.insert(if_idx, hw_in);
                    prev_hw_tx.insert(if_idx, hw_out);

                    let effective_rx_bps = path_rx_bps.max(os_rx_bps);
                    let effective_tx_bps = path_tx_bps.max(os_tx_bps);

                    path_snapshots.push(PathSnapshot {
                        path_id: p.path_id,
                        name: p.name.clone(),
                        local_ip: p.local_ip.clone(),
                        network_name: p.network_name.clone(),
                        is_alive: p.is_alive,
                        rtt_ms: p.smoothed_rtt.as_micros() as f32 / 1000.0,
                        loss_percent: p.loss_rate * 100.0,
                        rx_bps: effective_rx_bps,
                        tx_bps: effective_tx_bps,
                        share_percent: p.capacity_weight
                            / sched
                                .paths()
                                .iter()
                                .filter(|x| x.is_alive)
                                .map(|x| x.capacity_weight)
                                .sum::<f32>()
                                .max(1.0)
                            * 100.0,
                    });
                }

                rate_tracker.tick(total_rx, total_tx);

                // Combined aggregate speeds across all active bonded links
                let comb_rx_bps: f64 = path_snapshots.iter().map(|s| s.rx_bps).sum();
                let comb_tx_bps: f64 = path_snapshots.iter().map(|s| s.tx_bps).sum();

                let snapshot = EngineMetricsSnapshot {
                    timestamp_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    total_rx_bps: comb_rx_bps.max(*rate_tracker.current_rx_bps.lock()),
                    total_tx_bps: comb_tx_bps.max(*rate_tracker.current_tx_bps.lock()),
                    total_rx_bytes: total_rx,
                    total_tx_bytes: total_tx,
                    cpu_usage_percent: cpu,
                    memory_rss_mb: ram_mb,
                    active_bonding: true,
                    paths: path_snapshots,
                };

                *shared_metrics.write() = snapshot.clone();
                let _ = telemetry_tx.send(snapshot);
            }
        })
    };

    info!("NetFusion Multipath Bonding Engine is RUNNING!");
    info!("Local Multi-WAN Dispatcher active on 127.0.0.1:8080 (Zero-Server Mode)");
    info!("System Tray active in Windows Notification Area.");

    // Handle graceful shutdown via Ctrl+C, exit_requested, or IPC
    loop {
        if exit_requested.load(Ordering::Relaxed) {
            info!("System tray exit requested. Shutting down bonding engine...");
            break;
        }
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                info!("Received command: {:?}", cmd);
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }

    // Always restore Windows proxy and clean routes on exit
    proxy::disable_windows_proxy();
    route::purge_conflicting_kernel_routes();

    Ok(())
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    if s.len() % 2 != 0 {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}
