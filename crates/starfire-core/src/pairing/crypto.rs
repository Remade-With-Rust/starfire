// SPDX-License-Identifier: Apache-2.0
//! Pairing crypto primitives — docs/protocol/02-pairing-and-crypto.md.
//! Derived from the public GameStream pairing protocol; validated by successful
//! live pairing against Sunshine (success is the proof). Clean-room — no Moonlight
//! source consulted.
//!
//! Three primitives the pairing ladder is built from:
//!   - `pin_key`: the AES key derived from the PIN + salt.
//!   - `aes_ecb_{encrypt,decrypt}`: the challenge cipher (ECB — match it exactly,
//!     do not "improve" to CBC/GCM; it is what the host expects).
//!   - `sha256`: the hash used throughout the challenge chain.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use sha2::{Digest, Sha256};

/// Derive the pairing AES-128 key: `SHA-256(salt ‖ pin)` truncated to 16 bytes.
/// The PIN is the ASCII digit string the user/agent provides; the salt is the
/// 16 random bytes sent in the `getservercert` phase. [CAPTURE-LOCKED] — the
/// truncation + concatenation order are confirmed by live pairing succeeding.
pub fn pin_key(salt: &[u8], pin: &str) -> [u8; 16] {
    let digest = sha256(&[salt, pin.as_bytes()]);
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[..16]);
    key
}

/// Fill an array with OS randomness (salt, challenges, secrets).
pub fn random_bytes<const N: usize>() -> crate::Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).map_err(|e| crate::Error::Protocol(format!("OS RNG: {e}")))?;
    Ok(b)
}

/// SHA-256 over the concatenation of `parts`.
pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for p in parts {
        hasher.update(p);
    }
    hasher.finalize().into()
}

/// AES-128 **ECB** encrypt. Input is zero-padded up to a block boundary; the
/// GameStream blobs we encrypt are already block multiples (16 or 32 bytes).
pub fn aes_ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = Vec::with_capacity(data.len().div_ceil(16) * 16);
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        let mut ga = GenericArray::clone_from_slice(&block);
        cipher.encrypt_block(&mut ga);
        out.extend_from_slice(&ga);
    }
    out
}

/// AES-128 **ECB** decrypt. Any trailing partial block is ignored.
pub fn aes_ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        if chunk.len() != 16 {
            break;
        }
        let mut ga = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut ga);
        out.extend_from_slice(&ga);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS-197 AES-128 known-answer vector — proves our ECB block op is correct
    /// without any Sunshine dependency.
    #[test]
    fn aes128_fips197_vector() {
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let plain: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let expected: [u8; 16] = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a,
        ];
        let ct = aes_ecb_encrypt(&key, &plain);
        assert_eq!(ct, expected);
        assert_eq!(aes_ecb_decrypt(&key, &ct), plain);
    }

    #[test]
    fn ecb_roundtrip_multiblock() {
        let key = [0x42u8; 16];
        let data: Vec<u8> = (0u8..32).collect(); // two blocks
        assert_eq!(aes_ecb_decrypt(&key, &aes_ecb_encrypt(&key, &data)), data);
    }

    #[test]
    fn pin_key_is_deterministic_and_sensitive() {
        let salt = [7u8; 16];
        let a = pin_key(&salt, "1234");
        assert_eq!(a, pin_key(&salt, "1234"));
        assert_ne!(a, pin_key(&salt, "1235"));
        assert_ne!(a, pin_key(&[8u8; 16], "1234"));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn sha256_concatenates() {
        // sha256(["ab","c"]) == sha256(["abc"])
        assert_eq!(sha256(&[b"ab", b"c"]), sha256(&[b"abc"]));
    }
}
