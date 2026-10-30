use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BondingMode {
    AdaptiveWeighted,
    DeficitRoundRobin,
    Redundant,
    ActiveBackup,
}

impl Default for BondingMode {
    fn default() -> Self {
        BondingMode::AdaptiveWeighted
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceOverride {
    pub name: String,
    pub weight: Option<f32>,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub server_endpoint: String,
    pub auth_token: String,
    pub encryption_key: String,
    #[serde(default)]
    pub bonding_mode: BondingMode,
    #[serde(default = "default_true")]
    pub auto_discover_interfaces: bool,
    #[serde(default)]
    pub interface_overrides: Vec<InterfaceOverride>,
    #[serde(default = "default_mtu")]
    pub mtu: u16,
    #[serde(default = "default_virtual_ip")]
    pub virtual_ip: String,
    #[serde(default = "default_dns")]
    pub dns_servers: Vec<String>,
    #[serde(default = "default_reorder_timeout_ms")]
    pub reorder_timeout_ms: u64,
    #[serde(default = "default_ipc_port")]
    pub ipc_port: u16,
}

fn default_true() -> bool {
    true
}

fn default_mtu() -> u16 {
    1420
}

fn default_virtual_ip() -> String {
    "10.8.0.2".to_string()
}

fn default_dns() -> Vec<String> {
    vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()]
}

fn default_reorder_timeout_ms() -> u64 {
    15
}

fn default_ipc_port() -> u16 {
    53921
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server_endpoint: "127.0.0.1:51820".to_string(),
            auth_token: "netfusion-secure-pre-shared-token-2026".to_string(),
            encryption_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            bonding_mode: BondingMode::AdaptiveWeighted,
            auto_discover_interfaces: true,
            interface_overrides: vec![],
            mtu: 1420,
            virtual_ip: "10.8.0.2".to_string(),
            dns_servers: default_dns(),
            reorder_timeout_ms: 15,
            ipc_port: 53921,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub listen_port: u16,
    pub auth_token: String,
    pub encryption_key: String,
    #[serde(default = "default_server_tun_ip")]
    pub tun_ip: String,
    #[serde(default = "default_server_tun_name")]
    pub tun_name: String,
    #[serde(default = "default_mtu")]
    pub mtu: u16,
    #[serde(default = "default_true")]
    pub enable_nat: bool,
    #[serde(default = "default_reorder_timeout_ms")]
    pub reorder_timeout_ms: u64,
}

fn default_server_tun_ip() -> String {
    "10.8.0.1".to_string()
}

fn default_server_tun_name() -> String {
    "nf-tun0".to_string()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_port: 51820,
            auth_token: "netfusion-secure-pre-shared-token-2026".to_string(),
            encryption_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            tun_ip: "10.8.0.1".to_string(),
            tun_name: "nf-tun0".to_string(),
            mtu: 1420,
            enable_nat: true,
            reorder_timeout_ms: 15,
        }
    }
}
