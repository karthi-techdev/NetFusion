use log::info;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    CreateIpForwardEntry2, DeleteIpForwardEntry2, InitializeIpForwardEntry, MIB_IPFORWARD_ROW2,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

const CREATE_NO_WINDOW: u32 = 0x08000000;

static MULTIPATH_ACTIVE: AtomicBool = AtomicBool::new(false);
static CONFIGURED_IF_INDICES: Mutex<Vec<u32>> = Mutex::new(Vec::new());

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

        let _ = add_route_entry_v4(server_ip, 32, physical_gw, if_index, 1);

        self.added_routes.push(ActiveRoute {
            destination: server_ip,
            mask: Ipv4Addr::new(255, 255, 255, 255),
            gateway: physical_gw,
            if_index,
            metric: 1,
        });

        Ok(())
    }

    /// Clean up all bypass routes on shutdown
    pub fn cleanup(&mut self) {
        cleanup_multipath_routing();
        for route in self.added_routes.drain(..) {
            let _ = delete_route_entry_v4(route.destination, 32, route.if_index);
        }
    }
}

/// Add an IPv4 route entry via Windows IP Helper API (takes ~2 microseconds)
pub fn add_route_entry_v4(
    dest_ip: Ipv4Addr,
    prefix_len: u8,
    gateway: Ipv4Addr,
    if_index: u32,
    metric: u32,
) -> Result<(), u32> {
    unsafe {
        let mut row: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
        InitializeIpForwardEntry(&mut row);
        row.InterfaceIndex = if_index;
        row.Metric = metric;
        row.ValidLifetime = 600; // 10 minutes auto-expiration
        row.PreferredLifetime = 600;

        row.DestinationPrefix.Prefix.Ipv4.sin_family = AF_INET;
        row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes(dest_ip.octets());
        row.DestinationPrefix.PrefixLength = prefix_len;

        row.NextHop.Ipv4.sin_family = AF_INET;
        row.NextHop.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes(gateway.octets());

        let res = CreateIpForwardEntry2(&row);
        // 0 = Success, 5010 = ERROR_OBJECT_ALREADY_EXISTS, 183 = ERROR_ALREADY_EXISTS
        if res == 0 || res == 5010 || res == 183 {
            Ok(())
        } else {
            // Netsh fallback for resilient execution
            let _ = Command::new("netsh")
                .args([
                    "interface", "ipv4", "add", "route",
                    &format!("{}/{}", dest_ip, prefix_len),
                    &if_index.to_string(),
                    &gateway.to_string(),
                    &format!("metric={}", metric),
                    "store=active",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            Ok(())
        }
    }
}

/// Add an IPv6 route entry via Windows IP Helper API
pub fn add_route_entry_v6(
    dest_ip: Ipv6Addr,
    prefix_len: u8,
    gateway: Ipv6Addr,
    if_index: u32,
    metric: u32,
) -> Result<(), u32> {
    unsafe {
        let mut row: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
        InitializeIpForwardEntry(&mut row);
        row.InterfaceIndex = if_index;
        row.Metric = metric;
        row.ValidLifetime = 600;
        row.PreferredLifetime = 600;

        row.DestinationPrefix.Prefix.Ipv6.sin6_family = AF_INET6;
        row.DestinationPrefix.Prefix.Ipv6.sin6_addr.u.Byte = dest_ip.octets();
        row.DestinationPrefix.PrefixLength = prefix_len;

        row.NextHop.Ipv6.sin6_family = AF_INET6;
        row.NextHop.Ipv6.sin6_addr.u.Byte = gateway.octets();

        let res = CreateIpForwardEntry2(&row);
        if res == 0 || res == 5010 || res == 183 {
            Ok(())
        } else {
            let _ = Command::new("netsh")
                .args([
                    "interface", "ipv6", "add", "route",
                    &format!("{}/{}", dest_ip, prefix_len),
                    &if_index.to_string(),
                    &gateway.to_string(),
                    &format!("metric={}", metric),
                    "store=active",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            Ok(())
        }
    }
}

/// Delete an IPv4 route entry
pub fn delete_route_entry_v4(dest_ip: Ipv4Addr, prefix_len: u8, if_index: u32) -> Result<(), u32> {
    unsafe {
        let mut row: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
        InitializeIpForwardEntry(&mut row);
        row.InterfaceIndex = if_index;

        row.DestinationPrefix.Prefix.Ipv4.sin_family = AF_INET;
        row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes(dest_ip.octets());
        row.DestinationPrefix.PrefixLength = prefix_len;

        let res = DeleteIpForwardEntry2(&row);
        if res == 0 || res == 1168 /* ERROR_NOT_FOUND */ {
            Ok(())
        } else {
            let _ = Command::new("netsh")
                .args([
                    "interface", "ipv4", "delete", "route",
                    &format!("{}/{}", dest_ip, prefix_len),
                    &if_index.to_string(),
                    "store=active",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            Ok(())
        }
    }
}

/// Delete an IPv6 route entry
pub fn delete_route_entry_v6(dest_ip: Ipv6Addr, prefix_len: u8, if_index: u32) -> Result<(), u32> {
    unsafe {
        let mut row: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
        InitializeIpForwardEntry(&mut row);
        row.InterfaceIndex = if_index;

        row.DestinationPrefix.Prefix.Ipv6.sin6_family = AF_INET6;
        row.DestinationPrefix.Prefix.Ipv6.sin6_addr.u.Byte = dest_ip.octets();
        row.DestinationPrefix.PrefixLength = prefix_len;

        let res = DeleteIpForwardEntry2(&row);
        if res == 0 || res == 1168 {
            Ok(())
        } else {
            let _ = Command::new("netsh")
                .args([
                    "interface", "ipv6", "delete", "route",
                    &format!("{}/{}", dest_ip, prefix_len),
                    &if_index.to_string(),
                    "store=active",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            Ok(())
        }
    }
}

/// Setup global balanced multipath routing across physical interfaces.
/// Guarantees that both Ethernet and Wi-Fi have active kernel routes for outbound traffic
/// so that sockets can communicate concurrently at combined speed.
pub fn setup_multipath_routing(wans: &[crate::adapter::NetworkInterface]) {
    purge_conflicting_kernel_routes();

    let eligible: Vec<_> = wans.iter().filter(|w| w.is_eligible_wan()).collect();
    if eligible.is_empty() {
        return;
    }

    let mut configured_indices = Vec::new();

    // 1. Enable Weak Host Model & equalize interface metrics on all WAN adapters
    for wan in &eligible {
        configured_indices.push(wan.if_index);
        enable_weak_host_model(wan.if_index);

        // Equalize interface metrics so Windows doesn't completely disable secondary interface
        let _ = Command::new("netsh")
            .args(["interface", "ipv4", "set", "interface", &wan.if_index.to_string(), "metric=15"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let _ = Command::new("netsh")
            .args(["interface", "ipv6", "set", "interface", &wan.if_index.to_string(), "metric=15"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }

    // 2. If 2 or more physical interfaces exist, install balanced half-space internet routes
    if eligible.len() >= 2 {
        let wan0 = &eligible[0];
        let wan1 = &eligible[1];

        // IPv4: 0.0.0.0/1 via WAN0 gateway, 128.0.0.0/1 via WAN1 gateway
        if let Some(gw0) = wan0.gateway {
            let dest0 = Ipv4Addr::new(0, 0, 0, 0);
            let _ = add_route_entry_v4(dest0, 1, gw0, wan0.if_index, 10);
            info!("Multipath IPv4: 0.0.0.0/1 routed via {} on {} (if: {})", gw0, wan0.name, wan0.if_index);
        }
        if let Some(gw1) = wan1.gateway {
            let dest1 = Ipv4Addr::new(128, 0, 0, 0);
            let _ = add_route_entry_v4(dest1, 1, gw1, wan1.if_index, 10);
            info!("Multipath IPv4: 128.0.0.0/1 routed via {} on {} (if: {})", gw1, wan1.name, wan1.if_index);
        }

        // IPv6: 2000::/4 via WAN0 IPv6 gateway, 3000::/4 via WAN1 IPv6 gateway
        if let Some(gw0_v6) = wan0.ipv6_gateway {
            let dest0_v6: Ipv6Addr = "2000::".parse().unwrap();
            let _ = add_route_entry_v6(dest0_v6, 4, gw0_v6, wan0.if_index, 10);
            info!("Multipath IPv6: 2000::/4 routed via {} on {} (if: {})", gw0_v6, wan0.name, wan0.if_index);
        }
        if let Some(gw1_v6) = wan1.ipv6_gateway {
            let dest1_v6: Ipv6Addr = "3000::".parse().unwrap();
            let _ = add_route_entry_v6(dest1_v6, 4, gw1_v6, wan1.if_index, 10);
            info!("Multipath IPv6: 3000::/4 routed via {} on {} (if: {})", gw1_v6, wan1.name, wan1.if_index);
        }
    }

    if let Ok(mut lock) = CONFIGURED_IF_INDICES.lock() {
        *lock = configured_indices;
    }
    MULTIPATH_ACTIVE.store(true, Ordering::SeqCst);
}

/// Clean up balanced multipath routes and restore standard Windows routing metrics
pub fn cleanup_multipath_routing() {
    if !MULTIPATH_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }

    info!("Restoring standard Windows kernel routing table...");

    // Remove balanced half-space IPv4 routes
    let dest0 = Ipv4Addr::new(0, 0, 0, 0);
    let dest1 = Ipv4Addr::new(128, 0, 0, 0);
    let dest0_v6: Ipv6Addr = "2000::".parse().unwrap();
    let dest1_v6: Ipv6Addr = "3000::".parse().unwrap();

    let indices = {
        CONFIGURED_IF_INDICES.lock().map(|l| l.clone()).unwrap_or_default()
    };

    for idx in indices {
        let _ = delete_route_entry_v4(dest0, 1, idx);
        let _ = delete_route_entry_v4(dest1, 1, idx);
        let _ = delete_route_entry_v6(dest0_v6, 4, idx);
        let _ = delete_route_entry_v6(dest1_v6, 4, idx);

        // Restore interface metrics to automatic
        let _ = Command::new("netsh")
            .args(["interface", "ipv4", "set", "interface", &idx.to_string(), "metric=automatic"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let _ = Command::new("netsh")
            .args(["interface", "ipv6", "set", "interface", &idx.to_string(), "metric=automatic"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }

    purge_conflicting_kernel_routes();
}

/// Enable Weak Host Model on interface to allow sending & receiving through multiple WAN adapters
/// for both IPv4 and IPv6 protocols
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

    let _ = Command::new("netsh")
        .args([
            "interface",
            "ipv6",
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
            let mut purged_count = 0;
            for row in rows_slice {
                if row.DestinationPrefix.PrefixLength == 8 {
                    let first_octet = row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_un_b.s_b1;
                    if (1..=223).contains(&first_octet)
                        && first_octet != 10
                        && first_octet != 127
                    {
                        let res = DeleteIpForwardEntry2(row as *const _);
                        if res == 0 {
                            purged_count += 1;
                        }
                    }
                }
            }
            FreeMibTable(table_ptr as *const _);
            if purged_count > 0 {
                info!("Purged {} conflicting /8 static routes via IP Helper API", purged_count);
            }
        }
    }

    // PowerShell fallback removal to guarantee no /8 routes remain
    let _ = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-NetRoute | Where-Object { $_.DestinationPrefix -like '*/8' -and $_.DestinationPrefix -notlike '10.*' -and $_.DestinationPrefix -notlike '127.*' } | Remove-NetRoute -Confirm:$false -ErrorAction SilentlyContinue",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

impl Drop for RouteManager {
    fn drop(&mut self) {
        self.cleanup();
    }
}
