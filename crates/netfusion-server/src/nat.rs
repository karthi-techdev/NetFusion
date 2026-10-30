use log::{info, warn};
use std::process::Command;

pub struct NatManager;

impl NatManager {
    /// Enable Linux IPv4 kernel forwarding and iptables MASQUERADE
    pub fn enable_forwarding(tun_subnet: &str, egress_iface: &str) -> Result<(), String> {
        info!("Enabling Linux IPv4 kernel forwarding (sysctl net.ipv4.ip_forward=1)");
        let _ = Command::new("sysctl").args(["-w", "net.ipv4.ip_forward=1"]).output();

        info!(
            "Configuring iptables NAT MASQUERADE for subnet {} on egress interface {}",
            tun_subnet, egress_iface
        );

        let iptables_cmd = format!(
            "iptables -t nat -C POSTROUTING -s {tun_subnet} -o {egress_iface} -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -s {tun_subnet} -o {egress_iface} -j MASQUERADE",
            tun_subnet = tun_subnet,
            egress_iface = egress_iface
        );

        let output = Command::new("sh").args(["-c", &iptables_cmd]).output();
        if let Ok(out) = output {
            if !out.status.success() {
                warn!("iptables configuration message: {}", String::from_utf8_lossy(&out.stderr));
            }
        }

        // Enable FORWARD chain for tun traffic
        let forward_cmd = format!(
            "iptables -C FORWARD -s {tun_subnet} -j ACCEPT 2>/dev/null || iptables -A FORWARD -s {tun_subnet} -j ACCEPT; iptables -C FORWARD -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || iptables -A FORWARD -m state --state RELATED,ESTABLISHED -j ACCEPT",
            tun_subnet = tun_subnet
        );
        let _ = Command::new("sh").args(["-c", &forward_cmd]).output();

        Ok(())
    }
}
