# NetFusion – Deployment & Operational Guide

## 1. Linux Bonding Server Setup

### Requirements
- Linux VPS / Server (Ubuntu 20.04+, Debian 11/12, or Alma/Rocky Linux)
- Public static IPv4 address
- Open UDP port: `51820`
- Root / sudo privileges

### Automated Deployment
On your Linux server, clone the repository and run the automated installer:
```bash
git clone https://github.com/your-org/netfusion.git /tmp/netfusion
cd /tmp/netfusion
sudo bash scripts/server-setup.sh
```

The script automatically:
1. Installs system dependencies and iptables.
2. Enables kernel IPv4 forwarding (`sysctl net.ipv4.ip_forward=1`).
3. Sets up iptables NAT Masquerade on the primary internet interface (`eth0`).
4. Compiles the high-performance `netfusion-server` binary.
5. Configures and enables the `netfusion-server` systemd service.

### Verify Server Operation
```bash
sudo systemctl status netfusion-server
ip addr show nf-tun0
```

---

## 2. Windows Client Setup

### Requirements
- Windows 10 / Windows 11 (64-bit)
- Two or more active internet connections:
  - e.g. Wi-Fi (Airtel Broadband) + Ethernet (Jio Fiber) or USB 5G Tethering.
- Administrator privileges (for virtual network adapter creation and routing setup).

### Configuration (`configs/client.json`)
```json
{
  "server_endpoint": "YOUR_SERVER_PUBLIC_IP:51820",
  "auth_token": "netfusion-secure-pre-shared-token-2026",
  "encryption_key": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "bonding_mode": "AdaptiveWeighted",
  "auto_discover_interfaces": true,
  "mtu": 1420,
  "virtual_ip": "10.8.0.2",
  "dns_servers": ["1.1.1.1", "8.8.8.8"],
  "ipc_port": 53921
}
```

### Running the Client

#### Option A: Interactive Standalone Mode (Recommended for testing)
```powershell
# In PowerShell as Administrator:
cargo run --release -p netfusion-client -- --config configs/client.json
```

#### Option B: Native Windows Background Service
```powershell
# In PowerShell as Administrator:
.\scripts\service-install.ps1 -Action install
```

### Launching the Native Windows GUI Dashboard
```powershell
cargo run --release -p netfusion-gui
```
The native dashboard displays:
- Real-time combined and per-ISP throughput gauges (Airtel, Jio).
- CPU utilization & RAM consumption (confirming <1% CPU and 20–50 MB RAM).
- Latency (RTT) and packet loss per path.
- Start / Stop bonding controls and mode switcher.

---

## 3. Running Diagnostics and Benchmarks

```powershell
# Discover local physical network interfaces and gateway assignments:
cargo run -p netfusion-cli -- interfaces

# Stream live telemetry from the background client service:
cargo run -p netfusion-cli -- status

# Run the single-TCP throughput aggregation benchmark:
python scripts/benchmark.py
```
