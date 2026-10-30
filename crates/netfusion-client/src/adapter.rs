use std::net::Ipv4Addr;
use std::ptr::null_mut;
use std::sync::Mutex;
use std::time::Instant;
use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetAdaptersAddresses, GetIfTable2, GAA_FLAG_INCLUDE_GATEWAYS,
    IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows_sys::Win32::NetworkManagement::WiFi::*;
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterKind {
    Ethernet,
    WiFi,
    Cellular,
    Virtual,
    Other(u32),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct NetworkInterface {
    pub guid: String,
    pub name: String,
    pub description: String,
    pub if_index: u32,
    pub kind: AdapterKind,
    pub is_up: bool,
    pub ipv4_addr: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
    pub speed_bps: u64,
    pub isp_label: String,
    pub network_name: String,
}

impl NetworkInterface {
    pub fn is_eligible_wan(&self) -> bool {
        self.is_up
            && self.ipv4_addr.is_some()
            && self.gateway.is_some()
            && self.kind != AdapterKind::Virtual
            && !self.name.to_lowercase().contains("wintun")
            && !self.name.to_lowercase().contains("loopback")
            && !self.name.to_lowercase().contains("vethernet")
    }
}

/// Discover all physical and logical network adapters on the Windows system using GetAdaptersAddresses
pub fn discover_adapters() -> Result<Vec<NetworkInterface>, String> {
    let wifi_ssid = get_connected_wifi_ssid();
    let network_profiles = get_connected_network_profiles();

    let mut buf_len: u32 = 16384;
    let mut buffer: Vec<u8> = vec![0; buf_len as usize];

    let flags = GAA_FLAG_INCLUDE_GATEWAYS;
    let family = AF_UNSPEC as u32;

    let mut ret = unsafe {
        GetAdaptersAddresses(
            family,
            flags,
            null_mut(),
            buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
            &mut buf_len,
        )
    };

    if ret == ERROR_BUFFER_OVERFLOW {
        buffer.resize(buf_len as usize, 0);
        ret = unsafe {
            GetAdaptersAddresses(
                family,
                flags,
                null_mut(),
                buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut buf_len,
            )
        };
    }

    if ret != NO_ERROR {
        return Err(format!("GetAdaptersAddresses failed with error code: {}", ret));
    }

    let mut interfaces = Vec::new();
    let mut curr_ptr = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;

    while !curr_ptr.is_null() {
        let adapter = unsafe { &*curr_ptr };

        let guid = unsafe {
            if !adapter.AdapterName.is_null() {
                std::ffi::CStr::from_ptr(adapter.AdapterName as *const i8)
                    .to_string_lossy()
                    .into_owned()
            } else {
                String::new()
            }
        };

        let name = unsafe {
            if !adapter.FriendlyName.is_null() {
                let mut len = 0;
                while *adapter.FriendlyName.offset(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(adapter.FriendlyName, len as usize);
                String::from_utf16_lossy(slice)
            } else {
                String::new()
            }
        };

        let description = unsafe {
            if !adapter.Description.is_null() {
                let mut len = 0;
                while *adapter.Description.offset(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(adapter.Description, len as usize);
                String::from_utf16_lossy(slice)
            } else {
                String::new()
            }
        };

        let if_index = unsafe { adapter.Anonymous1.Anonymous.IfIndex };
        let is_up = adapter.OperStatus == 1; // IfOperStatusUp = 1

        let kind = match adapter.IfType {
            6 => AdapterKind::Ethernet, // MIB_IF_TYPE_ETHERNET
            71 => AdapterKind::WiFi,    // IF_TYPE_IEEE80211
            243 | 244 => AdapterKind::Cellular, // IF_TYPE_WWANPP / IF_TYPE_WWANPP2
            53 => AdapterKind::Virtual, // IF_TYPE_PROP_VIRTUAL
            other => {
                let lower_desc = description.to_lowercase();
                if lower_desc.contains("wireless") || lower_desc.contains("wi-fi") || lower_desc.contains("802.11") {
                    AdapterKind::WiFi
                } else if lower_desc.contains("ethernet") || lower_desc.contains("gigabit") || lower_desc.contains("realtek") {
                    AdapterKind::Ethernet
                } else if lower_desc.contains("rndis") || lower_desc.contains("cellular") || lower_desc.contains("usb") {
                    AdapterKind::Cellular
                } else {
                    AdapterKind::Other(other)
                }
            }
        };

        // Extract first IPv4 unicast address
        let mut ipv4_addr = None;
        let mut unicast_ptr = adapter.FirstUnicastAddress;
        while !unicast_ptr.is_null() {
            let unicast = unsafe { &*unicast_ptr };
            let sockaddr = unicast.Address.lpSockaddr;
            if !sockaddr.is_null() && unsafe { (*sockaddr).sa_family } == AF_INET as u16 {
                let sockaddr_in = sockaddr as *const SOCKADDR_IN;
                let sin_addr = unsafe { (*sockaddr_in).sin_addr };
                let octets = unsafe { sin_addr.S_un.S_addr }.to_ne_bytes();
                let ip = Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]);
                if !ip.is_loopback() {
                    ipv4_addr = Some(ip);
                    break;
                }
            }
            unicast_ptr = unicast.Next;
        }

        // Extract first IPv4 default gateway
        let mut gateway = None;
        let mut gw_ptr = adapter.FirstGatewayAddress;
        while !gw_ptr.is_null() {
            let gw = unsafe { &*gw_ptr };
            let sockaddr = gw.Address.lpSockaddr;
            if !sockaddr.is_null() && unsafe { (*sockaddr).sa_family } == AF_INET as u16 {
                let sockaddr_in = sockaddr as *const SOCKADDR_IN;
                let sin_addr = unsafe { (*sockaddr_in).sin_addr };
                let octets = unsafe { sin_addr.S_un.S_addr }.to_ne_bytes();
                gateway = Some(Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]));
                break;
            }
            gw_ptr = gw.Next;
        }

        let speed_bps = adapter.ReceiveLinkSpeed.max(adapter.TransmitLinkSpeed);

        // Dynamic interface labeling without hardcoded ISP names
        let isp_label = if let Some(gw) = gateway {
            format!("Gateway: {}", gw)
        } else {
            "Direct / Link-Local".to_string()
        };

        // Determine human-friendly network/Wi-Fi name dynamically:
        // Reads DhcpNetworkHint from Windows TCP/IP registry for this specific interface GUID
        // and cross-references with active NetworkListManager connection profiles.
        let hint_opt = get_adapter_network_hint(&guid);
        let network_name = if let Some(ref hint) = hint_opt {
            if let Some(p) = network_profiles.iter().find(|p| {
                p.to_lowercase().contains(&hint.to_lowercase()) || hint.to_lowercase().contains(&p.to_lowercase())
            }) {
                p.clone()
            } else {
                hint.clone()
            }
        } else {
            match kind {
                AdapterKind::WiFi => {
                    if let Some(ref ssid) = wifi_ssid {
                        ssid.clone()
                    } else if !network_profiles.is_empty() {
                        network_profiles.first().cloned().unwrap_or_else(|| name.clone())
                    } else {
                        name.clone()
                    }
                }
                AdapterKind::Ethernet => {
                    if network_profiles.len() > 1 {
                        network_profiles[1].clone()
                    } else if !network_profiles.is_empty() {
                        network_profiles.first().cloned().unwrap_or_else(|| name.clone())
                    } else {
                        name.clone()
                    }
                }
                _ => name.clone(),
            }
        };

        interfaces.push(NetworkInterface {
            guid,
            name,
            description,
            if_index,
            kind,
            is_up,
            ipv4_addr,
            gateway,
            speed_bps,
            isp_label,
            network_name,
        });

        curr_ptr = adapter.Next;
    }

    // Dynamic second pass: ensure each distinct WAN interface receives a unique active network profile
    let mut assigned_profiles = std::collections::HashSet::new();
    for iface in &interfaces {
        if !iface.network_name.is_empty() && iface.network_name != iface.name {
            assigned_profiles.insert(iface.network_name.clone());
        }
    }

    for iface in &mut interfaces {
        if iface.is_eligible_wan() && (iface.network_name.is_empty() || iface.network_name == iface.name) {
            if let Some(unassigned) = network_profiles.iter().find(|p| !assigned_profiles.contains(*p)) {
                iface.network_name = unassigned.clone();
                assigned_profiles.insert(unassigned.clone());
            }
        }
    }

    Ok(interfaces)
}

/// Query the network hint directly from Windows TCP/IP registry for this specific interface GUID
pub fn get_adapter_network_hint(guid: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::*;

    let clean_guid = guid.trim_matches(|c| c == '{' || c == '}');
    let subkey_str = format!(r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\{{{}}}", clean_guid);
    let wide_key: Vec<u16> = std::ffi::OsStr::new(&subkey_str).encode_wide().chain(std::iter::once(0)).collect();
    let val_name: Vec<u16> = std::ffi::OsStr::new("DhcpNetworkHint").encode_wide().chain(std::iter::once(0)).collect();

    unsafe {
        let mut hkey: HKEY = 0;
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide_key.as_ptr(), 0, KEY_READ, &mut hkey) == 0 {
            let mut val_type = 0;
            let mut val_len = 0;
            if RegQueryValueExW(hkey, val_name.as_ptr(), null_mut(), &mut val_type, null_mut(), &mut val_len) == 0 && val_len > 0 {
                let mut buf = vec![0u8; val_len as usize];
                if RegQueryValueExW(hkey, val_name.as_ptr(), null_mut(), &mut val_type, buf.as_mut_ptr(), &mut val_len) == 0 {
                    RegCloseKey(hkey);
                    let raw_str = if val_type == REG_SZ || val_type == REG_EXPAND_SZ {
                        let u16_slice = std::slice::from_raw_parts(buf.as_ptr() as *const u16, (val_len as usize) / 2);
                        String::from_utf16_lossy(u16_slice).trim_matches('\0').to_string()
                    } else {
                        String::from_utf8_lossy(&buf).trim_matches('\0').to_string()
                    };
                    return decode_swapped_hex_hint(&raw_str);
                }
            }
            RegCloseKey(hkey);
        }
    }
    None
}

fn decode_swapped_hex_hint(hex_str: &str) -> Option<String> {
    let clean = hex_str.trim();
    if clean.len() < 2 || clean.len() % 2 != 0 {
        return None;
    }
    let bytes = clean.as_bytes();
    let mut decoded = Vec::new();
    for chunk in bytes.chunks_exact(2) {
        let low_nibble = hex_char_to_u8(chunk[0])?;
        let high_nibble = hex_char_to_u8(chunk[1])?;
        let byte_val = (high_nibble << 4) | low_nibble;
        if byte_val >= 32 && byte_val <= 126 {
            decoded.push(byte_val as char);
        }
    }
    let result: String = decoded.into_iter().collect();
    if !result.is_empty() {
        Some(result)
    } else {
        None
    }
}

fn hex_char_to_u8(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Query the current connected Wi-Fi SSID using native Windows WLAN API (wlanapi.dll)
pub fn get_connected_wifi_ssid() -> Option<String> {
    unsafe {
        let mut negotiated_version = 0;
        let mut client_handle = 0;
        if WlanOpenHandle(2, null_mut(), &mut negotiated_version, &mut client_handle) != 0 {
            return None;
        }

        let mut if_list_ptr: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
        if WlanEnumInterfaces(client_handle, null_mut(), &mut if_list_ptr) != 0 || if_list_ptr.is_null() {
            WlanCloseHandle(client_handle, null_mut());
            return None;
        }

        let mut detected_ssid = None;
        let if_list = &*if_list_ptr;
        for i in 0..if_list.dwNumberOfItems {
            let if_info = &if_list.InterfaceInfo[i as usize];
            if if_info.isState == wlan_interface_state_connected {
                let mut data_size = 0;
                let mut p_data: *mut std::ffi::c_void = null_mut();
                if WlanQueryInterface(
                    client_handle,
                    &if_info.InterfaceGuid,
                    wlan_intf_opcode_current_connection,
                    null_mut(),
                    &mut data_size,
                    &mut p_data,
                    null_mut(),
                ) == 0 && !p_data.is_null() {
                    let conn_attrs = p_data as *const WLAN_CONNECTION_ATTRIBUTES;
                    let dot11_ssid = &(*conn_attrs).wlanAssociationAttributes.dot11Ssid;
                    let len = (dot11_ssid.uSSIDLength as usize).min(32);
                    if len > 0 {
                        let ssid_bytes = &dot11_ssid.ucSSID[..len];
                        let ssid_str = String::from_utf8_lossy(ssid_bytes).trim().to_string();
                        if !ssid_str.is_empty() {
                            detected_ssid = Some(ssid_str);
                        }
                    }
                    WlanFreeMemory(p_data);
                }
                if detected_ssid.is_some() {
                    break;
                }
            }
        }

        WlanFreeMemory(if_list_ptr as _);
        WlanCloseHandle(client_handle, null_mut());
        detected_ssid
    }
}

static PROFILE_CACHE: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);

/// Query connected Windows NetworkListManager profiles (e.g. "Home_WiFi", "Office_LAN")
pub fn get_connected_network_profiles() -> Vec<String> {
    if let Ok(mut cache) = PROFILE_CACHE.lock() {
        if let Some((time, ref profiles)) = *cache {
            if time.elapsed().as_secs() < 10 {
                return profiles.clone();
            }
        }

        use std::os::windows::process::CommandExt;
        let script = "$nlm = [Activator]::CreateInstance([Type]::GetTypeFromCLSID([Guid]'DCB00C01-570F-4A9B-8D69-199FDBA5723B')); $res = foreach ($n in $nlm.GetNetworks(1)) { $n.GetName() }; $res -join '|'";
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output();

        let mut res = Vec::new();
        if let Ok(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !stdout.is_empty() {
                res = stdout
                    .split('|')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
        *cache = Some((Instant::now(), res.clone()));
        return res;
    }
    Vec::new()
}

/// Query the Windows kernel directly via GetIfTable2 for hardware-level InOctets/OutOctets byte counters per interface index
pub fn get_adapter_traffic_stats() -> std::collections::HashMap<u32, (u64, u64)> {
    let mut stats = std::collections::HashMap::new();
    unsafe {
        let mut table_ptr: *mut MIB_IF_TABLE2 = null_mut();
        if GetIfTable2(&mut table_ptr) == NO_ERROR && !table_ptr.is_null() {
            let table = &*table_ptr;
            if table.NumEntries > 0 {
                let rows: &[MIB_IF_ROW2] = std::slice::from_raw_parts(&table.Table[0] as *const MIB_IF_ROW2, table.NumEntries as usize);
                for row in rows {
                    stats.insert(row.InterfaceIndex, (row.InOctets, row.OutOctets));
                }
            }
            FreeMibTable(table_ptr as _);
        }
    }
    stats
}
