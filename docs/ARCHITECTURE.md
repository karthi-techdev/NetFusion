# NetFusion – Architecture & System Design Document

## 1. Executive Summary

**NetFusion** is a lightweight, high-performance Windows internet bonding engine and Linux tunnel gateway designed for true **packet-level multipath bandwidth aggregation**.

Traditional multi-WAN solutions (such as dual-WAN routers or Windows connection-metric switching) only perform **session-based load balancing**: each individual TCP session or HTTP download is assigned to only one WAN interface. As a result, a single large download can never exceed the speed of the fastest single connection (e.g., capped at 40 Mbps on a 40 Mbps Fiber + 10 Mbps Mobile dual-connection setup).

NetFusion operates at **Layer 3 (IP Packet Level)**. Raw IP packets from any application (single TCP streams, UDP streams, torrents, browsers) are captured via a high-performance **Wintun** virtual network adapter, sequenced with 64-bit monotonic sequence numbers, encrypted with ChaCha20-Poly1305, and intelligently scheduled across all active physical internet adapters (Fiber Broadband, 5G Cellular Tethering, Wi-Fi, Ethernet).

On the remote Linux bonding server, packets arriving across different WAN paths are reordered in an adaptive jitter buffer, injected into a Linux TUN interface (`nf-tun0`), and masqueraded (NAT) to the public internet. Return packets from the internet are similarly scheduled across the client's WAN paths in proportion to real-time capacity and delay, recombined in the Windows client reorder buffer, and delivered into Wintun.

```
+---------------------------------------------------------------------------------------+
|                                    WINDOWS CLIENT                                     |
|                                                                                       |
|  +---------------------------------------------------------------------------------+  |
|  |                 Applications (Chrome, Steam, curl, Torrent, Games)              |  |
|  +----------------------------------------+----------------------------------------+  |
|                                           | (Single TCP stream @ 50 Mbps)             |
|                                           v                                           |
|  +---------------------------------------------------------------------------------+  |
|  |             Wintun Virtual Layer-3 Adapter (NetFusionBond0 - 10.8.0.2)          |  |
|  +----------------------------------------+----------------------------------------+  |
|                                           | Raw IP Packets                            |
|                                           v                                           |
|  +---------------------------------------------------------------------------------+  |
|  |                          NETFUSION CLIENT SERVICE ENGINE                         |  |
|  |                                                                                 |  |
|  |  +-----------------------+  +--------------------------+  +------------------+  |  |
|  |  |   Packet Sequencer    |  |  ChaCha20-Poly1305 AEAD  |  |  Sliding Window  |  |  |
|  |  |  (64-bit Monotonic)   |  |   Hardware Encryption    |  |  Anti-Replay     |  |  |
|  |  +-----------+-----------+  +------------+-------------+  +--------+---------+  |  |
|  |              |                           |                         |            |  |
|  |              +---------------------------+-------------------------+            |  |
|  |                                          v                                      |  |
|  |  +---------------------------------------------------------------------------+  |  |
|  |  |         Intelligent Adaptive Scheduler (ECTF & Deficit Round Robin)       |  |  |
|  |  |         - Tracks EWMA RTT, Jitter, Loss Rate & Throughput per ISP         |  |  |
|  |  +---------------------+-------------------------------+---------------------+  |  |
|  +------------------------|-------------------------------|-----------------------+  |
|                           |                               |                          |
|         Path 0 (80% Credit|                               | Path 1 (20% Credit)      |
|                           v                               v                          |
|  +---------------------------------+             +---------------------------------+ |
|  |  Physical Adapter: WiFi         |             |  Physical Adapter: Ethernet     | |
|  |  Local IP: 192.0.2.10           |             |  Local IP: 198.51.100.10        | |
|  |  WAN 1 (Fiber 40 Mbps capacity) |             |  WAN 2 (5G 10 Mbps capacity)    | |
|  +----------------+----------------+             +----------------+----------------+ |
+-------------------|-----------------------------------------------|-------------------+
                    |                                               |
                    | (Encrypted UDP Tunnel Datagrams)              | (Encrypted UDP Tunnel Datagrams)
                    v                                               v
        [ ISP 1 Infrastructure ]                        [ ISP 2 Infrastructure ]
                    \                                               /
                     \                                             /
                      v                                           v
+---------------------------------------------------------------------------------------+
|                                LINUX BONDING SERVER                                   |
|                                                                                       |
|  +---------------------------------------------------------------------------------+  |
|  |           UDP Multi-Path Ingress/Egress Socket (Port 51820)                     |  |
|  +----------------------------------------+----------------------------------------+  |
|                                           |                                           |
|                                           v                                           |
|  +---------------------------------------------------------------------------------+  |
|  |  Decryption -> Sliding Window Check -> Adaptive Reorder Buffer (Jitter Buffer)  |  |
|  |  - Eliminates out-of-order delivery caused by latency differences               |  |
|  |  - Prevents TCP DupAcks and Fast Retransmits                                    |  |
|  +----------------------------------------+----------------------------------------+  |
|                                           | Contiguous In-Order IP Packets            |
|                                           v                                           |
|  +---------------------------------------------------------------------------------+  |
|  |                   Linux TUN Device (nf-tun0 - 10.8.0.1/24)                      |  |
|  +----------------------------------------+----------------------------------------+  |
|                                           |                                           |
|                                           v                                           |
|  +---------------------------------------------------------------------------------+  |
|  |                   Linux Kernel IPv4 Forwarding & iptables NAT                   |  |
|  |           iptables -t nat -A POSTROUTING -s 10.8.0.0/24 -j MASQUERADE           |  |
|  +----------------------------------------+----------------------------------------+  |
|                                           |                                           |
|                                           v                                           |
|                                   [ PUBLIC INTERNET ]                                 |
+---------------------------------------------------------------------------------------+
```

---

## 2. Overcoming Fundamental Challenges in Packet-Level Multipath

### A. Head-of-Line (HoL) Blocking and Latency Disparities
When combining connections with differing latencies (e.g. Fiber with 15ms latency and 5G with 35ms latency), naive round-robin scheduling causes packets to arrive out of order at the destination.
- Standard TCP interprets out-of-order packets as network packet loss.
- Three duplicate ACKs (DupAcks) trigger **Fast Retransmit** and cut the TCP congestion window (`cwnd`) by 50%, resulting in degraded performance rather than aggregate speed.

**NetFusion Solution:**
1. **Earliest Completion Time First (ECTF) Scheduler**:
   Each WAN path maintains an exponential weighted moving average of round trip time ($\text{SRTT}$) and RTT variance ($\text{RTTVAR}$). Outbound packets are credited such that packet departure times are modulated by path latency, smoothing packet arrival order.
2. **Dynamic Jitter / Reordering Buffer**:
   Incoming packets are held in a bounded sparse queue. An adaptive timeout ($\Delta T = \min(40\text{ms}, \max(8\text{ms}, \beta \times |\text{SRTT}_1 - \text{SRTT}_2|))$) allows delayed packets to slot into their correct sequence. If a packet is genuinely lost on the wire, the timeout flushes the queue immediately, avoiding stalled connections.

### B. Routing Loop Prevention
When the client system directs default internet traffic (`0.0.0.0/0`) through Wintun, the NetFusion engine's own UDP tunnel traffic must NOT route into Wintun.
**NetFusion Solution:**
1. **Explicit Socket Binding with `IP_UNICASTIF`**: Sockets are bound not just to `INADDR_ANY`, but specifically to the local IPv4 address of each physical interface, and tagged with Win32 `IP_UNICASTIF` to pin the outgoing interface index.
2. **Targeted `/32` Host Routes**: The client route manager installs active host routes to the remote server IP via each physical interface gateway (`netsh interface ipv4 add route <SERVER_IP>/32 <IF_INDEX> <GATEWAY_IP> metric=1`).

---

## 3. Resource Footprint & Efficiency

NetFusion is written entirely in **Rust** with zero-cost abstractions:
- **Idle CPU**: `< 0.2%` (worker threads block asynchronously on OS I/O completion ports / epoll).
- **RAM**: `20–35 MB` working set.
- **Zero Heavyweight Frameworks**: No Electron, No Node.js runtime, No Docker Desktop required on the Windows client.
- **Hardware Acceleration**: ChaCha20-Poly1305 utilizes AVX2 / SSE4.1 SIMD instructions for line-rate encryption with negligible CPU consumption.
