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
            Err(res)
        }
    }
}

/// Add an IPv6 route entry via Windows IP Helper API
#[allow(dead_code)]
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
            Err(res)
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
#[allow(dead_code)]
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
/// Guarantees that both Ethernet and Wi-Fi have equal metric default routes and weak host model active
/// so that sockets bound via IP_UNICAST_IF / IPV6_UNICAST_IF route directly through each physical adapter concurrently.
pub fn setup_multipath_routing(wans: &[crate::adapter::NetworkInterface]) {
    purge_conflicting_kernel_routes();

    let eligible: Vec<_> = wans.iter().filter(|w| w.is_eligible_wan()).collect();
    if eligible.is_empty() {
        return;
    }

    let mut configured_indices = Vec::new();
    let eligible_indices: std::collections::HashSet<u32> = eligible.iter().map(|w| w.if_index).collect();

    // 1. Enable Weak Host Model & equalize interface metrics on all WAN adapters
    for wan in &eligible {
        configured_indices.push(wan.if_index);
        enable_weak_host_model(wan.if_index);

        // Equalize interface metrics so Windows treats both links with equal priority
        let _ = Command::new("netsh")
            .args(["interface", "ipv4", "set", "interface", &wan.if_index.to_string(), "metric=15"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let _ = Command::new("netsh")
            .args(["interface", "ipv6", "set", "interface", &wan.if_index.to_string(), "metric=15"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }

    // 2. Equalize default route metrics on both IPv4 and IPv6 to 15
    unsafe {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            FreeMibTable, GetIpForwardTable2, SetIpForwardEntry2, MIB_IPFORWARD_ROW2, MIB_IPFORWARD_TABLE2,
        };
        use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

        // IPv4 Default Routes
        let mut table_ptr: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
        if GetIpForwardTable2(AF_INET, &mut table_ptr) == 0 && !table_ptr.is_null() {
            let num_entries = (*table_ptr).NumEntries as usize;
            let rows_slice = std::slice::from_raw_parts((*table_ptr).Table.as_ptr(), num_entries);
            for row in rows_slice {
                if row.DestinationPrefix.PrefixLength == 0 && eligible_indices.contains(&row.InterfaceIndex) {
                    let mut updated: MIB_IPFORWARD_ROW2 = *row;
                    updated.Metric = 15;
                    let _ = SetIpForwardEntry2(&updated);
                }
            }
            FreeMibTable(table_ptr as *const _);
        }

        // IPv6 Default Routes
        let mut table_v6_ptr: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
        if GetIpForwardTable2(AF_INET6, &mut table_v6_ptr) == 0 && !table_v6_ptr.is_null() {
            let num_entries = (*table_v6_ptr).NumEntries as usize;
            let rows_slice = std::slice::from_raw_parts((*table_v6_ptr).Table.as_ptr(), num_entries);
            for row in rows_slice {
                if row.DestinationPrefix.PrefixLength == 0 && eligible_indices.contains(&row.InterfaceIndex) {
                    let mut updated: MIB_IPFORWARD_ROW2 = *row;
                    updated.Metric = 15;
                    let _ = SetIpForwardEntry2(&updated);
                }
            }
            FreeMibTable(table_v6_ptr as *const _);
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

    let indices = {
        CONFIGURED_IF_INDICES.lock().map(|l| l.clone()).unwrap_or_default()
    };

    for idx in indices {
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

/// Purge any lingering /8, /1, /4, /32, or /128 static routes that conflict with Multi-WAN socket binding
pub fn purge_conflicting_kernel_routes() {
    unsafe {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            DeleteIpForwardEntry2, FreeMibTable, GetIpForwardTable2, MIB_IPFORWARD_TABLE2,
        };
        use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

        // IPv4 purge
        let mut table_ptr: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
        if GetIpForwardTable2(AF_INET, &mut table_ptr) == 0 && !table_ptr.is_null() {
            let num_entries = (*table_ptr).NumEntries as usize;
            let rows_slice = std::slice::from_raw_parts((*table_ptr).Table.as_ptr(), num_entries);
            let mut purged_count = 0;
            for row in rows_slice {
                let plen = row.DestinationPrefix.PrefixLength;
                let first_octet = row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_un_b.s_b1;
                let has_gw = row.NextHop.Ipv4.sin_addr.S_un.S_addr != 0;

                // Delete /8, /1, or dynamically added /32 routes that have a next-hop gateway
                let is_removable = (plen == 8 && (1..=223).contains(&first_octet) && first_octet != 10 && first_octet != 127)
                    || (plen == 1)
                    || (plen == 32 && has_gw && first_octet != 127 && first_octet != 255);

                if is_removable {
                    if DeleteIpForwardEntry2(row as *const _) == 0 {
                        purged_count += 1;
                    }
                }
            }
            FreeMibTable(table_ptr as *const _);
            if purged_count > 0 {
                info!("Purged {} conflicting IPv4 routes via IP Helper API", purged_count);
            }
        }

        // IPv6 purge
        let mut table_v6_ptr: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
        if GetIpForwardTable2(AF_INET6, &mut table_v6_ptr) == 0 && !table_v6_ptr.is_null() {
            let num_entries = (*table_v6_ptr).NumEntries as usize;
            let rows_slice = std::slice::from_raw_parts((*table_v6_ptr).Table.as_ptr(), num_entries);
            let mut purged_v6 = 0;
            for row in rows_slice {
                let plen = row.DestinationPrefix.PrefixLength;
                // Delete /4 half-spaces or dynamically added /128 routes with gateways
                if plen == 4 || plen == 128 {
                    let b0 = row.DestinationPrefix.Prefix.Ipv6.sin6_addr.u.Byte[0];
                    let b1 = row.DestinationPrefix.Prefix.Ipv6.sin6_addr.u.Byte[1];
                    let is_link_local = b0 == 0xfe && (b1 & 0xc0) == 0x80;
                    let is_loopback = row.DestinationPrefix.Prefix.Ipv6.sin6_addr.u.Byte[15] == 1
                        && row.DestinationPrefix.Prefix.Ipv6.sin6_addr.u.Byte[..15].iter().all(|&b| b == 0);

                    if !is_link_local && !is_loopback {
                        if DeleteIpForwardEntry2(row as *const _) == 0 {
                            purged_v6 += 1;
                        }
                    }
                }
            }
            FreeMibTable(table_v6_ptr as *const _);
            if purged_v6 > 0 {
                info!("Purged {} conflicting IPv6 routes via IP Helper API", purged_v6);
            }
        }
    }
}

impl Drop for RouteManager {
    fn drop(&mut self) {
        self.cleanup();
    }
}
