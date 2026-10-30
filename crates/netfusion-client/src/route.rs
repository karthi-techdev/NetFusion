use log::{info, warn};
use std::net::Ipv4Addr;
use std::os::windows::process::CommandExt;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x08000000;

#[allow(dead_code)]
pub struct ActiveRoute {
    pub destination: Ipv4Addr,
    pub mask: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub if_index: u32,
    pub metric: u32,
}

#[allow(dead_code)]
pub struct RouteManager {
    added_routes: Vec<ActiveRoute>,
}

#[allow(dead_code)]
impl RouteManager {
    pub fn new() -> Self {
        Self {
            added_routes: Vec::new(),
        }
    }

    /// Add a direct /32 host route for the bonding server via a physical gateway
    /// to guarantee tunnel UDP traffic never enters the virtual TUN adapter.
    pub fn add_server_bypass_route(
        &mut self,
        server_ip: Ipv4Addr,
        physical_gw: Ipv4Addr,
        if_index: u32,
    ) -> Result<(), String> {
        if server_ip.is_loopback() || server_ip.is_unspecified() || server_ip.is_link_local() {
            info!("Skipping bypass route for local address: {}", server_ip);
            return Ok(());
        }

        info!(
            "Adding server bypass route: {}/32 via gateway {} (if_index: {})",
            server_ip, physical_gw, if_index
        );

        let output = Command::new("netsh")
            .args([
                "interface",
                "ipv4",
                "add",
                "route",
                &format!("{}/32", server_ip),
                &if_index.to_string(),
                &physical_gw.to_string(),
                "metric=1",
                "store=active",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("Failed to execute netsh add route: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Ignore if route already exists
            if !stdout.contains("already exists") && !stderr.contains("already exists") {
                warn!("netsh route add notice: stdout={}, stderr={}", stdout.trim(), stderr.trim());
            }
        }

        self.added_routes.push(ActiveRoute {
            destination: server_ip,
            mask: Ipv4Addr::new(255, 255, 255, 255),
            gateway: physical_gw,
            if_index,
            metric: 1,
        });

        Ok(())
    }

    /// Configure the Wintun adapter default gateway (0.0.0.0/0) with low metric
    pub fn set_wintun_default_route(wintun_if_index: u32, wintun_gw: Ipv4Addr) -> Result<(), String> {
        info!("Setting default route 0.0.0.0/0 via Wintun {} (if_index: {})", wintun_gw, wintun_if_index);

        let output = Command::new("netsh")
            .args([
                "interface",
                "ipv4",
                "add",
                "route",
                "0.0.0.0/0",
                &wintun_if_index.to_string(),
                &wintun_gw.to_string(),
                "metric=5",
                "store=active",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("Failed to set wintun default route: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("netsh default route status: {}", stderr.trim());
        }

        Ok(())
    }

    /// Remove default route through Wintun
    pub fn remove_wintun_default_route(wintun_if_index: u32, wintun_gw: Ipv4Addr) {
        let _ = Command::new("netsh")
            .args([
                "interface",
                "ipv4",
                "delete",
                "route",
                "0.0.0.0/0",
                &wintun_if_index.to_string(),
                &wintun_gw.to_string(),
                "store=active",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }

    /// Clean up all bypass routes on shutdown
    pub fn cleanup(&mut self) {
        purge_conflicting_kernel_routes();
        for route in self.added_routes.drain(..) {
            info!(
                "Removing bypass route {} via if_index {}",
                route.destination, route.if_index
            );
            let _ = Command::new("netsh")
                .args([
                    "interface",
                    "ipv4",
                    "delete",
                    "route",
                    &format!("{}/32", route.destination),
                    &route.if_index.to_string(),
                    &route.gateway.to_string(),
                    "store=active",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
    }
}

/// Enable Weak Host Model on interface to allow sending & receiving through multiple WAN adapters
pub fn enable_weak_host_model(if_index: u32) {
    let _ = Command::new("netsh")
        .args([
            "interface",
            "ipv4",
            "set",
            "interface",
            &if_index.to_string(),
            "weakhostsend=enabled",
            "weakhostreceive=enabled",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

/// Purge any lingering /8 static routes that conflict with Multi-WAN socket binding
pub fn purge_conflicting_kernel_routes() {
    unsafe {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            DeleteIpForwardEntry2, FreeMibTable, GetIpForwardTable2, MIB_IPFORWARD_TABLE2,
        };
        use windows_sys::Win32::Networking::WinSock::AF_INET;

        let mut table_ptr: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
        if GetIpForwardTable2(AF_INET, &mut table_ptr) == 0 && !table_ptr.is_null() {
            let num_entries = (*table_ptr).NumEntries as usize;
            let rows_slice = std::slice::from_raw_parts((*table_ptr).Table.as_ptr(), num_entries);
            for row in rows_slice {
                if row.DestinationPrefix.PrefixLength == 8 {
                    let first_octet = row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_un_b.s_b1;
                    if (1..=223).contains(&first_octet)
                        && first_octet != 10
                        && first_octet != 127
                    {
                        let _ = DeleteIpForwardEntry2(row as *const _);
                    }
                }
            }
            FreeMibTable(table_ptr as *const _);
        }
    }
}

impl Drop for RouteManager {
    fn drop(&mut self) {
        self.cleanup();
    }
}
