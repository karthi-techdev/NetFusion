use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Key, Nonce,
};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
pub const REPLAY_WINDOW_SIZE: u64 = 1024;

#[derive(thiserror::Error, Debug)]
pub enum CryptoError {
    #[error("Encryption failed: {0}")]
    EncryptionFailed(String),
    #[error("Decryption failed / authentication tag mismatch")]
    DecryptionFailed,
    #[error("Replay attack detected: sequence number {0} already seen or outside replay window")]
    ReplayDetected(u64),
    #[error("Invalid key length: expected 32 bytes, got {0}")]
    InvalidKeyLength(usize),
}

/// ChaCha20-Poly1305 Cipher Context for NetFusion Tunnel
pub struct TunnelCrypto {
    cipher: ChaCha20Poly1305,
    session_salt: [u8; 4],
}

impl TunnelCrypto {
    pub fn new(key_bytes: &[u8; KEY_LEN], session_salt: [u8; 4]) -> Self {
        let key = Key::from_slice(key_bytes);
        let cipher = ChaCha20Poly1305::new(key);
        Self {
            cipher,
            session_salt,
        }
    }

    /// Construct a deterministic 96-bit (12-byte) nonce from session_salt, path_id, and seq_num.
    /// This avoids transmitting nonces over the wire and guarantees nonce uniqueness per packet.
    #[inline]
    pub fn construct_nonce(&self, path_id: u8, seq_num: u64) -> [u8; NONCE_LEN] {
        let mut nonce = [0u8; NONCE_LEN];
        // Bytes 0..4: session salt
        nonce[0..4].copy_from_slice(&self.session_salt);
        // Byte 4: path id
        nonce[4] = path_id;
        // Byte 5: reserved/padding
        nonce[5] = 0;
        // Bytes 6..12: lower 48 bits of sequence number
        let seq_bytes = seq_num.to_be_bytes();
        nonce[6..12].copy_from_slice(&seq_bytes[2..8]);
        nonce
    }

    /// Encrypt plaintext with ChaCha20-Poly1305, with optional associated data (e.g. packet header)
    pub fn encrypt(
        &self,
        path_id: u8,
        seq_num: u64,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let nonce_bytes = self.construct_nonce(path_id, seq_num);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let payload = chacha20poly1305::aead::Payload {
            msg: plaintext,
            aad,
        };

        self.cipher
            .encrypt(nonce, payload)
            .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))
    }

    /// Decrypt ciphertext with ChaCha20-Poly1305 and verify Poly1305 MAC tag
    pub fn decrypt(
        &self,
        path_id: u8,
        seq_num: u64,
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let nonce_bytes = self.construct_nonce(path_id, seq_num);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let payload = chacha20poly1305::aead::Payload {
            msg: ciphertext,
            aad,
        };

        self.cipher
            .decrypt(nonce, payload)
            .map_err(|_| CryptoError::DecryptionFailed)
    }
}

/// Sliding window anti-replay protection filter (1024-packet window)
pub struct ReplayFilter {
    max_seq: u64,
    bitmap: [u64; 16], // 16 * 64 = 1024 bits
}

impl Default for ReplayFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayFilter {
    pub fn new() -> Self {
        Self {
            max_seq: 0,
            bitmap: [0; 16],
        }
    }

    /// Check if sequence number is accepted and mark it as seen.
    /// Returns Ok(()) if valid, or Err(CryptoError::ReplayDetected) if duplicate or too old.
    pub fn check_and_update(&mut self, seq: u64) -> Result<(), CryptoError> {
        if seq == 0 {
            return Err(CryptoError::ReplayDetected(seq));
        }

        if seq > self.max_seq {
            let diff = seq - self.max_seq;
            if diff >= REPLAY_WINDOW_SIZE {
                // Large jump: reset entire bitmap
                self.bitmap = [0; 16];
            } else {
                // Shift bitmap by diff positions
                self.shift_bitmap(diff as usize);
            }
            self.max_seq = seq;
            self.set_bit(0);
            Ok(())
        } else {
            let diff = self.max_seq - seq;
            if diff >= REPLAY_WINDOW_SIZE {
                // Packet is older than the window
                return Err(CryptoError::ReplayDetected(seq));
            }
            if self.is_bit_set(diff as usize) {
                // Packet already seen
                return Err(CryptoError::ReplayDetected(seq));
            }
            self.set_bit(diff as usize);
            Ok(())
        }
    }

    #[inline]
    fn shift_bitmap(&mut self, shift: usize) {
        if shift >= REPLAY_WINDOW_SIZE as usize {
            self.bitmap = [0; 16];
            return;
        }

        let word_shift = shift / 64;
        let bit_shift = shift % 64;

        if word_shift > 0 {
            for i in (word_shift..16).rev() {
                self.bitmap[i] = self.bitmap[i - word_shift];
            }
            for i in 0..word_shift {
                self.bitmap[i] = 0;
            }
        }

        if bit_shift > 0 {
            let mut carry = 0u64;
            for i in 0..16 {
                let next_carry = self.bitmap[i] >> (64 - bit_shift);
                self.bitmap[i] = (self.bitmap[i] << bit_shift) | carry;
                carry = next_carry;
            }
        }
    }

    #[inline]
    fn set_bit(&mut self, pos: usize) {
        let word_idx = pos / 64;
        let bit_idx = pos % 64;
        if word_idx < 16 {
            self.bitmap[word_idx] |= 1u64 << bit_idx;
        }
    }

    #[inline]
    fn is_bit_set(&self, pos: usize) -> bool {
        let word_idx = pos / 64;
        let bit_idx = pos % 64;
        if word_idx < 16 {
            (self.bitmap[word_idx] & (1u64 << bit_idx)) != 0
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crypto_roundtrip() {
        let key = [0x5au8; KEY_LEN];
        let salt = [1, 2, 3, 4];
        let crypto = TunnelCrypto::new(&key, salt);

        let plaintext = b"Hello, NetFusion bonded connection!";
        let aad = b"header_data";
        let seq = 42;
        let path = 0;

        let ciphertext = crypto.encrypt(path, seq, plaintext, aad).expect("encryption");
        assert_ne!(&ciphertext[..plaintext.len()], plaintext);

        let decrypted = crypto.decrypt(path, seq, &ciphertext, aad).expect("decryption");
        assert_eq!(&decrypted, plaintext);

        // Verify tampering causes authentication error
        let mut tampered = ciphertext.clone();
        tampered[0] ^= 0x01;
        assert!(crypto.decrypt(path, seq, &tampered, aad).is_err());
    }

    #[test]
    fn test_replay_protection() {
        let mut filter = ReplayFilter::new();

        assert!(filter.check_and_update(100).is_ok());
        // Duplicate is rejected
        assert!(filter.check_and_update(100).is_err());

        // In-order advancement
        assert!(filter.check_and_update(101).is_ok());
        assert!(filter.check_and_update(105).is_ok());

        // Out-of-order within window is accepted
        assert!(filter.check_and_update(102).is_ok());
        assert!(filter.check_and_update(103).is_ok());

        // Replaying out-of-order packet is rejected
        assert!(filter.check_and_update(102).is_err());

        // Packet outside window is rejected
        assert!(filter.check_and_update(2000).is_ok());
        assert!(filter.check_and_update(100).is_err()); // 2000 - 100 > 1024
    }
}
