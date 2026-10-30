mod nat;
mod session;
mod tun;

use bytes::Bytes;
use clap::Parser;
use log::{error, info, warn};
use nat::NatManager;
use netfusion_core::config::ServerConfig;
use netfusion_core::crypto::TunnelCrypto;
use netfusion_core::protocol::{
    MessageType, PacketHeader, HEADER_LEN,
};
use session::{ClientSession, SessionManager};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tun::TunDevice;

#[derive(Parser, Debug)]
#[command(author, version, about = "NetFusion Linux Multipath Bonding Server", long_about = None)]
struct CliArgs {
    #[arg(short, long, default_value = "configs/server.json")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = CliArgs::parse();

    info!("Starting NetFusion Bonding Server v{}", env!("CARGO_PKG_VERSION"));

    let config = if args.config.exists() {
        let content = std::fs::read_to_string(&args.config)?;
        serde_json::from_str::<ServerConfig>(&content)?
    } else {
        info!("Server config {:?} not found. Creating default...", args.config);
        let default_config = ServerConfig::default();
        if let Some(parent) = args.config.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&args.config, serde_json::to_string_pretty(&default_config)?)?;
        default_config
    };

    run_bonding_server(config).await?;

    Ok(())
}

async fn run_bonding_server(config: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    let tun_ip: Ipv4Addr = config.tun_ip.parse().unwrap_or(Ipv4Addr::new(10, 8, 0, 1));

    // 1. Initialize TUN device
    let tun_dev = Arc::new(TunDevice::create(&config.tun_name, tun_ip, config.mtu)?);

    // 2. Enable NAT and IPv4 forwarding if on Linux
    if config.enable_nat {
        let _ = NatManager::enable_forwarding("10.8.0.0/24", "eth0");
    }

    // 3. Initialize Shared UDP Listen Socket
    let bind_addr = format!("0.0.0.0:{}", config.listen_port);
    let socket = Arc::new(UdpSocket::bind(&bind_addr).await?);
    info!("NetFusion Server listening for multi-WAN UDP traffic on {}", bind_addr);

    // 4. Session Manager
    let session_mgr = Arc::new(SessionManager::new());

    // Pre-create standard client session (Session ID: 1, Assigned IP: 10.8.0.2)
    let mut key_bytes = [0u8; 32];
    if let Ok(bytes) = hex_decode(config.encryption_key.trim()) {
        if bytes.len() == 32 {
            key_bytes.copy_from_slice(&bytes);
        }
    }
    let crypto = Arc::new(TunnelCrypto::new(&key_bytes, [0x4E, 0x46, 0x01, 0x02]));
    let default_session = Arc::new(ClientSession::new(
        1,
        Ipv4Addr::new(10, 8, 0, 2),
        crypto.clone(),
        config.reorder_timeout_ms,
    ));
    session_mgr.insert(default_session.clone());

    let global_downlink_seq = Arc::new(AtomicU64::new(1));

    // 5. Spawn Inbound UDP Traffic Processing Loop (WAN -> TUN)
    {
        let socket = socket.clone();
        let session_mgr = session_mgr.clone();
        let tun_dev = tun_dev.clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((n, remote_addr)) => {
                        let data = &buf[..n];
                        if n < HEADER_LEN {
                            continue;
                        }

                        let mut header_slice = &data[..HEADER_LEN];
                        if let Ok(header) = PacketHeader::decode(&mut header_slice) {
                            if let Some(session) = session_mgr.get_by_id(header.session_id) {
                                // Dynamically record or update this client WAN uplink
                                session.update_client_path(header.path_id, remote_addr);

                                match header.msg_type {
                                    MessageType::Data => {
                                        let ciphertext = &data[HEADER_LEN..];
                                        let aad = &data[..HEADER_LEN];
                                        if let Ok(plaintext) = session.crypto.decrypt(header.path_id, header.seq_num, ciphertext, aad) {
                                            let ready = session.reorder_buf.lock().push(header.seq_num, Bytes::from(plaintext));
                                            #[cfg(target_os = "linux")]
                                            for pkt in ready {
                                                let _ = tun_dev.write_packet(&pkt).await;
                                            }
                                        }
                                    }
                                    MessageType::Probe => {
                                        // Send ProbeReply for RTT tracking
                                        let mut reply_header = PacketHeader::new(MessageType::ProbeReply, header.session_id, header.path_id, 0);
                                        reply_header.timestamp_echo_us = header.timestamp_us;
                                        let mut reply_buf = bytes::BytesMut::with_capacity(HEADER_LEN);
                                        reply_header.encode(&mut reply_buf);
                                        let _ = socket.send_to(&reply_buf, remote_addr).await;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        });
    }

    // 6. Spawn Outbound TUN Return Traffic Processing Loop (TUN -> Multi-WAN -> Client)
    #[cfg(target_os = "linux")]
    {
        let socket = socket.clone();
        let session_mgr = session_mgr.clone();
        let tun_dev = tun_dev.clone();
        let global_downlink_seq = global_downlink_seq.clone();

        tokio::spawn(async move {
            let mut tun_buf = vec![0u8; 2048];
            loop {
                match tun_dev.read_packet(&mut tun_buf).await {
                    Ok(n) if n >= 20 => {
                        let ip_pkt = &tun_buf[..n];
                        // Parse destination IP from IPv4 header (bytes 16..20)
                        let dest_ip = Ipv4Addr::new(ip_pkt[16], ip_pkt[17], ip_pkt[18], ip_pkt[19]);

                        if let Some(session) = session_mgr.get_by_ip(&dest_ip) {
                            if let Some((path_id, remote_addr)) = session.select_downlink_path(n) {
                                let seq = global_downlink_seq.fetch_add(1, Ordering::Relaxed);
                                let mut header = PacketHeader::new(MessageType::Data, session.session_id, path_id, seq);
                                header.payload_len = (n + 16) as u16; // plaintext + Poly1305 MAC tag

                                let mut header_bytes = bytes::BytesMut::with_capacity(HEADER_LEN);
                                header.encode(&mut header_bytes);

                                if let Ok(ciphertext) = session.crypto.encrypt(path_id, seq, ip_pkt, &header_bytes) {
                                    let mut final_datagram = bytes::BytesMut::with_capacity(HEADER_LEN + ciphertext.len());
                                    final_datagram.extend_from_slice(&header_bytes);
                                    final_datagram.extend_from_slice(&ciphertext);
                                    let _ = socket.send_to(&final_datagram, remote_addr).await;
                                }
                            }
                        }
                    }
                    _ => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        });
    }

    info!("NetFusion Bonding Server is fully operational on port {}.", config.listen_port);

    // Keep server running indefinitely
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
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
