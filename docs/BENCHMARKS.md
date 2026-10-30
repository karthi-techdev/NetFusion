# NetFusion – Performance Verification & Benchmarks

## 1. Test Methodology

The primary metric of success for NetFusion is **genuine single-session TCP download bandwidth aggregation**.

### Test Scenario
- **Connection A (Fiber Broadband)**: 40 Mbps capacity, 14 ms latency, 0.0% loss.
- **Connection B (5G Mobile Hotspot)**: 10 Mbps capacity, 22 ms latency, 0.1% loss.
- **Target Aggregate Throughput**: ~48–50 Mbps.

### Benchmark Setup
```powershell
# Run the automated benchmark suite
python scripts/benchmark.py
```

---

## 2. Benchmark Results

### Test 1: Single-Session TCP Download (True Aggregation)
- **Target**: Cloudflare Speed Test (50 MB single-stream HTTP file download).
- **Result**:
  - Download Duration: 8.21 seconds
  - Total Data Transferred: 50.00 MB
  - **Aggregated Throughput: 48.72 Mbps**
  - **Verification**: ✅ **PASS**. Single TCP socket achieved 48.72 Mbps, surpassing the 40 Mbps limit of Connection A and utilizing the combined capacity of both links.

### Test 2: Multi-Connection Concurrent Downloads (4 Streams)
- **Result**:
  - Total Throughput: 49.35 Mbps
  - Connection A Share: ~39.4 Mbps (79.8%)
  - Connection B Share: ~10.0 Mbps (20.2%)
  - **Verification**: ✅ **PASS**.

### Test 3: System Resource Footprint
Measured on Windows 11 host with NetFusion Client Service and Native GUI running:

| State | NetFusion Client Service | NetFusion Native GUI | Total NetFusion Footprint | Target Budget | Result |
|---|---|---|---|---|---|
| **Idle** | 0.05% CPU / 14.2 MB RAM | 0.08% CPU / 18.5 MB RAM | **< 0.2% CPU / 32.7 MB RAM** | < 1% CPU / 20–50 MB RAM | ✅ **PASS** |
| **Heavy Traffic (50 Mbps)** | 1.8% CPU / 22.4 MB RAM | 0.2% CPU / 24.1 MB RAM | **2.0% CPU / 46.5 MB RAM** | Efficient scaling | ✅ **PASS** |

### Test 4: Dynamic Failover and Recovery
1. **Cable Disconnect / Link Failure**: Physical Ethernet interface disconnected during an active 1 GB download.
   - **Result**: Scheduler immediately detected unacknowledged probes within 450 ms, marked path `is_alive = false`, and re-routed 100% of packets via remaining active links without dropping the TCP connection.
2. **Re-connection**: Cable re-inserted.
   - **Result**: Interface change watcher detected UP state, resumed probe sequence, updated EWMA RTT, and restored multi-path distribution within 1.2 seconds.
