#!/usr/bin/env bash
# ==============================================================================
# NetFusion Bonding Server - Automated Production Deployment Script
# Supports: Ubuntu 20.04+, Debian 11+, Debian 12+, Rocky Linux / RHEL 9
# ==============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${CYAN}======================================================${NC}"
echo -e "${CYAN}   NetFusion Bonding Server - Automated Installer     ${NC}"
echo -e "${CYAN}======================================================${NC}"

if [[ $EUID -ne 0 ]]; then
   echo -e "${RED}Error: This installer must be run as root.${NC}" 
   exit 1
fi

INSTALL_DIR="/opt/netfusion"
CONFIG_DIR="/etc/netfusion"
SERVICE_FILE="/etc/systemd/system/netfusion-server.service"

echo -e "${YELLOW}[1/6] Installing system prerequisites...${NC}"
apt-get update -y || yum check-update || true
apt-get install -y curl iptables iproute2 build-essential git || yum install -y curl iptables iproute git gcc make

# 2. Enable kernel IPv4 forwarding
echo -e "${YELLOW}[2/6] Configuring Linux Kernel IP forwarding...${NC}"
cat > /etc/sysctl.d/99-netfusion.conf <<EOF
net.ipv4.ip_forward = 1
net.ipv4.conf.all.forwarding = 1
net.ipv4.conf.default.forwarding = 1
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
net.ipv4.tcp_rmem = 4096 87380 16777216
net.ipv4.tcp_wmem = 4096 65536 16777216
EOF
sysctl --system

# 3. Detect primary WAN egress interface (e.g. eth0, ens3, enp1s0)
PRIMARY_IFACE=$(ip route get 8.8.8.8 | awk '{print $5; exit}')
echo -e "${GREEN}Detected primary internet egress interface: ${PRIMARY_IFACE}${NC}"

# 4. Configure iptables NAT masquerading
echo -e "${YELLOW}[3/6] Setting up iptables NAT Masquerade...${NC}"
iptables -t nat -A POSTROUTING -s 10.8.0.0/24 -o "${PRIMARY_IFACE}" -j MASQUERADE
iptables -A FORWARD -s 10.8.0.0/24 -j ACCEPT
iptables -A FORWARD -m state --state RELATED,ESTABLISHED -j ACCEPT

# Persist iptables rules
if command -v netfilter-persistent &> /dev/null; then
    netfilter-persistent save
elif [ -d /etc/iptables ]; then
    iptables-save > /etc/iptables/rules.v4
fi

# 5. Create directories & configuration
echo -e "${YELLOW}[4/6] Installing NetFusion configuration...${NC}"
mkdir -p "${INSTALL_DIR}"
mkdir -p "${CONFIG_DIR}"

cat > "${CONFIG_DIR}/server.json" <<EOF
{
  "listen_port": 51820,
  "auth_token": "netfusion-secure-pre-shared-token-2026",
  "encryption_key": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "tun_ip": "10.8.0.1",
  "tun_name": "nf-tun0",
  "mtu": 1420,
  "enable_nat": true,
  "reorder_timeout_ms": 15
}
EOF

# 6. Build or copy netfusion-server binary
echo -e "${YELLOW}[5/6] Building / Installing NetFusion server binary...${NC}"
if ! command -v cargo &> /dev/null; then
    echo "Installing Rust toolchain..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

if [ -f "./target/release/netfusion-server" ]; then
    cp "./target/release/netfusion-server" "${INSTALL_DIR}/netfusion-server"
else
    echo "Compiling netfusion-server from source..."
    cargo build --release -p netfusion-server
    cp "./target/release/netfusion-server" "${INSTALL_DIR}/netfusion-server"
fi
chmod +x "${INSTALL_DIR}/netfusion-server"

# 7. Create systemd unit
echo -e "${YELLOW}[6/6] Creating and starting systemd service...${NC}"
cat > "${SERVICE_FILE}" <<EOF
[Unit]
Description=NetFusion Multipath Internet Bonding Server
After=network.target

[Service]
Type=simple
User=root
WorkingDirectory=${INSTALL_DIR}
ExecStart=${INSTALL_DIR}/netfusion-server --config ${CONFIG_DIR}/server.json
Restart=always
RestartSec=3
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable netfusion-server
systemctl restart netfusion-server

echo -e "${GREEN}======================================================${NC}"
echo -e "${GREEN}NetFusion Bonding Server is INSTALLED and ACTIVE!     ${NC}"
echo -e "${GREEN}Listening UDP Port : 51820                            ${NC}"
echo -e "${GREEN}TUN Virtual Subnet : 10.8.0.0/24 (Server IP 10.8.0.1) ${NC}"
echo -e "${GREEN}======================================================${NC}"
systemctl status netfusion-server --no-pager
