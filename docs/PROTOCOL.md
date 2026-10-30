# NetFusion Tunnel Protocol (NFTP) Specification

**Version:** 1.0  
**Transport:** UDP  
**Standard Port:** 51820  
**Default MTU:** 1420 bytes  

---

## 1. Frame Header Format

All NetFusion datagrams begin with a fixed **48-byte header**, followed by the payload (encrypted for data frames) and the 16-byte Poly1305 authentication tag.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|       Magic (0x4E, 0x46)      |    Version    |   Msg Type    |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                          Session ID                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|    Path ID    |     Flags     |          (Reserved)           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
+                    Sequence Number (64-bit)                   +
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                      Path Sequence Number                     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
+                  Cumulative ACK Sequence (64-bit)             +
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
+                  Selective ACK Bitmask (64-bit)               +
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                         Timestamp (us)                        |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                      Timestamp Echo (us)                      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|          Payload Len          |   Payload + Poly1305 Tag...   |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

### Field Definitions

| Field | Size | Description |
|---|---|---|
| `Magic` | 2 bytes | Protocol identifier: `0x4E, 0x46` (ASCII "NF") |
| `Version` | 1 byte | Protocol version: `0x01` |
| `Msg Type` | 1 byte | Message type identifier (see table below) |
| `Session ID` | 4 bytes | Unique 32-bit client tunnel session handle |
| `Path ID` | 1 byte | Physical WAN identifier (`0x00` = Path 0, `0x01` = Path 1, etc.) |
| `Flags` | 1 byte | Bitmask: Bit 0 = Retransmit, Bit 1 = Probe, Bit 2 = Urgent, Bit 3 = Encrypted |
| `Sequence Number`| 8 bytes | Monotonically increasing 64-bit global sequence number |
| `Path Seq Num` | 4 bytes | Per-path sequence counter used for path loss measurement |
| `Cumulative ACK` | 8 bytes | Highest contiguous packet sequence received |
| `SACK Mask` | 8 bytes | Bitmap of 64 packets received out of order past cumulative ACK |
| `Timestamp (us)` | 4 bytes | Sender microsecond timestamp |
| `Timestamp Echo` | 4 bytes | Echoed peer timestamp for precise RTT/jitter calculation |
| `Payload Len` | 2 bytes | Length of following payload |
| `Payload + Tag` | Variable | Encrypted IP packet payload + 16-byte Poly1305 MAC tag |

---

## 2. Message Types

| Value | Identifier | Direction | Purpose |
|---|---|---|---|
| `0x01` | `HandshakeInit` | Client -> Server | Session initiation request |
| `0x02` | `HandshakeResp` | Server -> Client | Session approval & IP assignment |
| `0x03` | `Data` | Bidirectional | Encapsulated bonded IP packet |
| `0x04` | `Ack` | Bidirectional | Standalone acknowledgment frame |
| `0x05` | `Probe` | Bidirectional | RTT & path health probe |
| `0x06` | `ProbeReply` | Bidirectional | RTT probe response |
| `0x07` | `PathUpdate` | Bidirectional | Dynamic path capacity change |
| `0x08` | `KeepAlive` | Bidirectional | Maintain NAT state on routers |
| `0x09` | `Disconnect` | Bidirectional | Graceful tunnel teardown |

---

## 3. Cryptography & Security

### ChaCha20-Poly1305 Authenticated Encryption
- **Key**: 256-bit symmetric pre-shared key (PSK) or derived via authenticated handshake.
- **Nonce Construction (96-bit)**:
  - Bytes 0..3: Session Salt (4 bytes)
  - Byte 4: Path ID (1 byte)
  - Byte 5: Reserved (`0x00`)
  - Bytes 6..11: Lower 48 bits of `Sequence Number`
  Because `Sequence Number` strictly increments for every packet, nonce uniqueness is guaranteed with zero wire transmission overhead.
- **Associated Data (AAD)**: The entire 48-byte `PacketHeader` is passed as AAD into Poly1305. Any modification or tampering of headers in transit results in instant authentication rejection.
- **Anti-Replay Protection**: A 1024-packet sliding window bitmap rejects duplicate or replayed packets.
