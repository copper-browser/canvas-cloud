//! Envelope encryption at rest.
//!
//! * `master_key` (config, 32 bytes) → HKDF-SHA256 → key-encryption key (KEK).
//! * Every canvas has a random 32-byte `doc_key` wrapped by the KEK
//!   (`canvases.doc_key_wrapped`).
//! * Blobs are AES-256-GCM, laid out as `nonce(12) || ciphertext+tag`, with an AAD that binds
//!   the ciphertext to its owner/location (e.g. the canvas id), so a row copied to another
//!   canvas fails to decrypt.
//!
//! Key rotation is out of scope for v1.

use std::sync::Arc;

use aes_gcm::aead::{Aead as _, KeyInit as _, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroize as _;

use crate::error::ApiError;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEK_INFO: &[u8] = b"canvas-cloud/v1/key-encryption-key";
const WRAP_AAD: &[u8] = b"canvas-cloud/v1/wrapped-key";

#[derive(Clone)]
pub struct Crypto {
    kek: Arc<Aes256Gcm>,
}

impl std::fmt::Debug for Crypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Crypto { kek: <redacted> }")
    }
}

impl Crypto {
    /// Derive the key-encryption key from the configured master key.
    pub fn from_master_key(key: &[u8; 32]) -> Self {
        let hk = Hkdf::<Sha256>::new(Some(b"canvas-cloud"), key);
        let mut kek = [0u8; 32];
        hk.expand(KEK_INFO, &mut kek)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&kek));
        kek.zeroize();
        Self {
            kek: Arc::new(cipher),
        }
    }

    /// Fresh random 32-byte key (OS CSPRNG).
    pub fn new_key() -> [u8; 32] {
        crate::ids::random_bytes::<32>()
    }

    /// Wrap a data/doc key with the KEK. Output: `nonce(12) || ct(32) || tag(16)` = 60 bytes.
    pub fn wrap_key(&self, key: &[u8; 32]) -> Vec<u8> {
        seal_with(&self.kek, WRAP_AAD, key)
    }

    pub fn unwrap_key(&self, wrapped: &[u8]) -> Result<[u8; 32], ApiError> {
        let mut plain = open_with(&self.kek, WRAP_AAD, wrapped)?;
        let out: [u8; 32] = plain
            .as_slice()
            .try_into()
            .map_err(|_| ApiError::internal("wrapped key has wrong length"))?;
        plain.zeroize();
        Ok(out)
    }

    /// AES-256-GCM encrypt under `key` with `aad`. Output: `nonce(12) || ciphertext || tag(16)`.
    pub fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
        seal_with(&cipher, aad, plain)
    }

    /// Decrypt a blob produced by [`Crypto::seal`]. Fails on any tampering or wrong AAD/key.
    pub fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, ApiError> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
        open_with(&cipher, aad, sealed)
    }
}

fn seal_with(cipher: &Aes256Gcm, aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let nonce_bytes = crate::ids::random_bytes::<NONCE_LEN>();
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), Payload { msg: plain, aad })
        .expect("AES-GCM encryption cannot fail for in-memory buffers");
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    out
}

fn open_with(cipher: &Aes256Gcm, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, ApiError> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(ApiError::internal("sealed blob too short"));
    }
    let (nonce, ct) = sealed.split_at(NONCE_LEN);
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| ApiError::internal("blob failed authentication (wrong key/aad or tampered)"))
}

/// Size of a sealed blob for a plaintext of `n` bytes.
pub const fn sealed_len(n: usize) -> usize {
    NONCE_LEN + n + TAG_LEN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let key = Crypto::new_key();
        let sealed = Crypto::seal(&key, b"u:spaces", b"hello world");
        assert_eq!(sealed.len(), sealed_len(11));
        assert_eq!(
            Crypto::open(&key, b"u:spaces", &sealed).unwrap(),
            b"hello world"
        );
        // Nonces are random: same input encrypts differently.
        assert_ne!(sealed, Crypto::seal(&key, b"u:spaces", b"hello world"));
    }

    #[test]
    fn empty_plaintext_roundtrip() {
        let key = Crypto::new_key();
        let sealed = Crypto::seal(&key, b"", b"");
        assert_eq!(Crypto::open(&key, b"", &sealed).unwrap().len(), 0);
    }

    #[test]
    fn tamper_detected() {
        let key = Crypto::new_key();
        let mut sealed = Crypto::seal(&key, b"aad", b"secret payload");
        for i in [0, NONCE_LEN, sealed.len() - 1] {
            sealed[i] ^= 0x01;
            assert!(Crypto::open(&key, b"aad", &sealed).is_err(), "byte {i}");
            sealed[i] ^= 0x01;
        }
        assert!(Crypto::open(&key, b"aad", &sealed).is_ok());
        assert!(Crypto::open(&key, b"other-aad", &sealed).is_err());
        assert!(Crypto::open(&Crypto::new_key(), b"aad", &sealed).is_err());
        assert!(Crypto::open(&key, b"aad", &sealed[..10]).is_err());
    }

    #[test]
    fn wrap_unwrap() {
        let master = Crypto::new_key();
        let c = Crypto::from_master_key(&master);
        let dk = Crypto::new_key();
        let wrapped = c.wrap_key(&dk);
        assert_eq!(wrapped.len(), 60);
        assert_eq!(c.unwrap_key(&wrapped).unwrap(), dk);
        // Same master → same KEK; different master → cannot unwrap.
        assert_eq!(
            Crypto::from_master_key(&master)
                .unwrap_key(&wrapped)
                .unwrap(),
            dk
        );
        assert!(Crypto::from_master_key(&Crypto::new_key())
            .unwrap_key(&wrapped)
            .is_err());
        // The KEK is derived, not the raw master key.
        assert!(Crypto::open(&master, WRAP_AAD, &wrapped).is_err());
    }

    #[test]
    fn debug_redacts() {
        let c = Crypto::from_master_key(&[7u8; 32]);
        assert!(!format!("{c:?}").contains('7'));
    }
}
