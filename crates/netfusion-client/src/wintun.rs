use log::{info, warn};
use std::net::Ipv4Addr;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

#[allow(dead_code)]
pub struct WintunDevice {
    pub adapter: Arc<wintun::Adapter>,
    pub session: Arc<wintun::Session>,
    pub if_index: u32,
}

impl WintunDevice {
    /// Locate wintun.dll in current directory, parent directories, or driver bundle
    pub fn find_wintun_dll() -> Result<PathBuf, String> {
        let candidates = [
            PathBuf::from("wintun.dll"),
            PathBuf::from("drivers/wintun/bin/amd64/wintun.dll"),
            PathBuf::from("../../drivers/wintun/bin/amd64/wintun.dll"),
            PathBuf::from("../drivers/wintun/bin/amd64/wintun.dll"),
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("wintun.dll")))
                .unwrap_or_else(|| PathBuf::from("wintun.dll")),
        ];

        for candidate in &candidates {
            if candidate.exists() {
                info!("Found wintun.dll at: {:?}", candidate);
                return Ok(candidate.clone());
            }
        }

        Err("wintun.dll not found in standard paths. Ensure drivers/wintun/bin/amd64/wintun.dll exists.".to_string())
    }

    /// Create and initialize the Wintun adapter
    pub fn create(
        adapter_name: &str,
        virtual_ip: Ipv4Addr,
        subnet_mask: Ipv4Addr,
        _gateway: Ipv4Addr,
        mtu: u16,
    ) -> Result<Self, String> {
        let dll_path = Self::find_wintun_dll()?;
        let wintun = unsafe {
            wintun::load_from_path(&dll_path)
                .map_err(|e| format!("Failed to load wintun.dll from {:?}: {}", dll_path, e))?
        };

        let pool_name = "NetFusion";
        let guid = None;

        info!("Creating or opening Wintun adapter: {} (pool: {})", adapter_name, pool_name);
        let adapter = match wintun::Adapter::open(&wintun, adapter_name) {
            Ok(a) => {
                info!("Opened existing Wintun adapter: {}", adapter_name);
                a
            }
            Err(_) => {
                info!("Creating new Wintun adapter: {}", adapter_name);
                wintun::Adapter::create(&wintun, pool_name, adapter_name, guid)
                    .map_err(|e| format!("Failed to create Wintun adapter: {}", e))?
            }
        };

        let if_index = adapter
            .get_adapter_index()
            .map_err(|e| format!("Failed to get adapter index: {}", e))?;

        info!("Wintun adapter index: {}", if_index);

        // Configure IP address and subnet mask using netsh
        let ip_str = virtual_ip.to_string();
        let mask_str = subnet_mask.to_string();

        // Assign IP address and mask to Wintun adapter WITHOUT setting any default gateway.
        // Crucial: setting gateway=none ensures Windows default route (0.0.0.0/0) is NEVER hijacked,
        // strictly protecting physical Ethernet and Wi-Fi adapters from being disconnected or disabled.
        let output = Command::new("netsh")
            .args([
                "interface",
                "ipv4",
                "set",
                "address",
                &format!("name={}", adapter_name),
                "source=static",
                &format!("address={}", ip_str),
                &format!("mask={}", mask_str),
                "gateway=none",
            ])
            .creation_flags(0x08000000)
            .output();

        if let Ok(out) = output {
            if !out.status.success() {
                warn!(
                    "netsh set address status: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }

        // Set MTU
        let _ = Command::new("netsh")
            .args([
                "interface",
                "ipv4",
                "set",
                "subinterface",
                &adapter_name,
                &format!("mtu={}", mtu),
                "store=active",
            ])
            .creation_flags(0x08000000)
            .output();

        info!("Starting Wintun ring session (capacity: {})", wintun::MAX_RING_CAPACITY);
        let session = adapter
            .start_session(wintun::MAX_RING_CAPACITY)
            .map_err(|e| format!("Failed to start Wintun session: {}", e))?;

        Ok(Self {
            adapter,
            session: Arc::new(session),
            if_index,
        })
    }
}
