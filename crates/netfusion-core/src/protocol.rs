use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const NFTP_MAGIC: [u8; 2] = [0x4E, 0x46]; // "NF"
pub const NFTP_VERSION: u8 = 1;
pub const HEADER_LEN: usize = 48;
pub const MAX_PACKET_SIZE: usize = 1600;
pub const DEFAULT_MTU: u16 = 1420;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    HandshakeInit = 0x01,
    HandshakeResp = 0x02,
    Data          = 0x03,
    Ack           = 0x04,
    Probe         = 0x05,
    ProbeReply    = 0x06,
    PathUpdate    = 0x07,
    KeepAlive     = 0x08,
    Disconnect    = 0x09,
}

impl TryFrom<u8> for MessageType {
    type Error = ProtocolError;

    fn try_from(val: u8) -> Result<Self, Self::Error> {
        match val {
            0x01 => Ok(MessageType::HandshakeInit),
            0x02 => Ok(MessageType::HandshakeResp),
            0x03 => Ok(MessageType::Data),
            0x04 => Ok(MessageType::Ack),
            0x05 => Ok(MessageType::Probe),
            0x06 => Ok(MessageType::ProbeReply),
            0x07 => Ok(MessageType::PathUpdate),
            0x08 => Ok(MessageType::KeepAlive),
            0x09 => Ok(MessageType::Disconnect),
            _ => Err(ProtocolError::InvalidMessageType(val)),
        }
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct PacketFlags: u8 {
        const IS_RETRANSMIT = 0b0000_0001;
        const PROBE_REQUEST = 0b0000_0010;
        const URGENT        = 0b0000_0100;
        const ENCRYPTED     = 0b0000_1000;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("Packet too short: got {0} bytes, minimum is {HEADER_LEN}")]
    PacketTooShort(usize),
    #[error("Invalid magic: expected [0x4E, 0x46], got {0:?}")]
    InvalidMagic([u8; 2]),
    #[error("Unsupported protocol version: {0}")]
    UnsupportedVersion(u8),
    #[error("Invalid message type: {0:#04x}")]
    InvalidMessageType(u8),
    #[error("Payload length mismatch: header specifies {expected}, buffer has {actual}")]
    LengthMismatch { expected: usize, actual: usize },
    #[error("Encryption / Decryption error: {0}")]
    CryptoError(String),
}

/// Header for NetFusion Tunnel Protocol (48 bytes fixed size)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketHeader {
    pub magic: [u8; 2],
    pub version: u8,
    pub msg_type: MessageType,
    pub session_id: u32,
    pub path_id: u8,
    pub flags: PacketFlags,
    pub seq_num: u64,
    pub path_seq_num: u32,
    pub ack_seq_num: u64,
    pub sack_mask: u64,
    pub timestamp_us: u32,
    pub timestamp_echo_us: u32,
    pub payload_len: u16,
}

impl Default for PacketHeader {
    fn default() -> Self {
        Self {
            magic: NFTP_MAGIC,
            version: NFTP_VERSION,
            msg_type: MessageType::Data,
            session_id: 0,
            path_id: 0,
            flags: PacketFlags::ENCRYPTED,
            seq_num: 0,
            path_seq_num: 0,
            ack_seq_num: 0,
            sack_mask: 0,
            timestamp_us: 0,
            timestamp_echo_us: 0,
            payload_len: 0,
        }
    }
}

impl PacketHeader {
    pub fn new(msg_type: MessageType, session_id: u32, path_id: u8, seq_num: u64) -> Self {
        Self {
            magic: NFTP_MAGIC,
            version: NFTP_VERSION,
            msg_type,
            session_id,
            path_id,
            flags: PacketFlags::ENCRYPTED,
            seq_num,
            path_seq_num: 0,
            ack_seq_num: 0,
            sack_mask: 0,
            timestamp_us: 0,
            timestamp_echo_us: 0,
            payload_len: 0,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.magic);
        dst.put_u8(self.version);
        dst.put_u8(self.msg_type as u8);
        dst.put_u32(self.session_id);
        dst.put_u8(self.path_id);
        dst.put_u8(self.flags.bits());
        dst.put_u64(self.seq_num);
        dst.put_u32(self.path_seq_num);
        dst.put_u64(self.ack_seq_num);
        dst.put_u64(self.sack_mask);
        dst.put_u32(self.timestamp_us);
        dst.put_u32(self.timestamp_echo_us);
        dst.put_u16(self.payload_len);
    }

    pub fn decode(src: &mut &[u8]) -> Result<Self, ProtocolError> {
        if src.len() < HEADER_LEN {
            return Err(ProtocolError::PacketTooShort(src.len()));
        }

        let magic = [src[0], src[1]];
        if magic != NFTP_MAGIC {
            return Err(ProtocolError::InvalidMagic(magic));
        }

        let version = src[2];
        if version != NFTP_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }

        let msg_type = MessageType::try_from(src[3])?;
        let session_id = (&src[4..8]).get_u32();
        let path_id = src[8];
        let flags = PacketFlags::from_bits_truncate(src[9]);
        let seq_num = (&src[10..18]).get_u64();
        let path_seq_num = (&src[18..22]).get_u32();
        let ack_seq_num = (&src[22..30]).get_u64();
        let sack_mask = (&src[30..38]).get_u64();
        let timestamp_us = (&src[38..42]).get_u32();
        let timestamp_echo_us = (&src[42..46]).get_u32();
        let payload_len = (&src[46..48]).get_u16();

        *src = &src[HEADER_LEN..];

        Ok(Self {
            magic,
            version,
            msg_type,
            session_id,
            path_id,
            flags,
            seq_num,
            path_seq_num,
            ack_seq_num,
            sack_mask,
            timestamp_us,
            timestamp_echo_us,
            payload_len,
        })
    }
}

/// A parsed NetFusion packet consisting of a header and payload
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub header: PacketHeader,
    pub payload: Bytes,
}

impl Packet {
    pub fn new(header: PacketHeader, payload: Bytes) -> Self {
        let mut header = header;
        header.payload_len = payload.len() as u16;
        Self { header, payload }
    }

    pub fn serialize(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(HEADER_LEN + self.payload.len());
        self.header.encode(&mut buf);
        buf.put_slice(&self.payload);
        buf.freeze()
    }

    pub fn deserialize(mut data: &[u8]) -> Result<Self, ProtocolError> {
        let header = PacketHeader::decode(&mut data)?;
        let expected = header.payload_len as usize;
        if data.len() < expected {
            return Err(ProtocolError::LengthMismatch {
                expected,
                actual: data.len(),
            });
        }
        let payload = Bytes::copy_from_slice(&data[..expected]);
        Ok(Self { header, payload })
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct HandshakePayload {
    pub client_id: String,
    pub timestamp: u64,
    pub auth_token: String,
    pub requested_mtu: u16,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct HandshakeResponsePayload {
    pub session_id: u32,
    pub assigned_ip: String,
    pub subnet_mask: String,
    pub server_virtual_ip: String,
    pub mtu: u16,
    pub heartbeat_interval_ms: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_encode_decode() {
        let header = PacketHeader {
            magic: NFTP_MAGIC,
            version: NFTP_VERSION,
            msg_type: MessageType::Data,
            session_id: 0x12345678,
            path_id: 1,
            flags: PacketFlags::ENCRYPTED | PacketFlags::URGENT,
            seq_num: 1000500,
            path_seq_num: 200,
            ack_seq_num: 999990,
            sack_mask: 0b10101,
            timestamp_us: 1234567,
            timestamp_echo_us: 7654321,
            payload_len: 256,
        };

        let mut buf = BytesMut::new();
        header.encode(&mut buf);
        assert_eq!(buf.len(), HEADER_LEN);

        let mut slice = &buf[..];
        let decoded = PacketHeader::decode(&mut slice).expect("decode failed");
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_packet_serialize_deserialize() {
        let payload = Bytes::from_static(b"Sample IP payload for NetFusion bonding tunnel");
        let header = PacketHeader::new(MessageType::Data, 42, 0, 777);
        let packet = Packet::new(header, payload.clone());

        let bytes = packet.serialize();
        let parsed = Packet::deserialize(&bytes).expect("deserialize failed");

        assert_eq!(parsed.header.seq_num, 777);
        assert_eq!(parsed.header.session_id, 42);
        assert_eq!(parsed.header.path_id, 0);
        assert_eq!(parsed.payload, payload);
    }
}
