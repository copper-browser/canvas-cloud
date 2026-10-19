//! Identifiers, opaque tokens and hashing helpers.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};

/// Time-ordered UUID (v7) for new rows.
pub fn uuid_v7() -> uuid::Uuid {
    uuid::Uuid::now_v7()
}

/// 32 random bytes from the OS CSPRNG, base64url without padding (43 chars).
pub fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

/// `true` for the shape [`random_token`] produces: exactly 43 base64url characters.
pub fn is_token_shape(t: &str) -> bool {
    t.len() == 43
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `N` random bytes from the OS CSPRNG.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    rand::rngs::OsRng.fill_bytes(&mut out);
    out
}

/// SHA-256 digest.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Lower-case hex SHA-256 digest.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha256(bytes))
}

/// base64url (no padding) encode.
pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// base64url decode; accepts input with or without `=` padding.
pub fn b64url_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    URL_SAFE_NO_PAD.decode(s.trim().trim_end_matches('='))
}

/// Standard base64 (with padding) — used for JSON payloads.
pub fn b64_std(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode standard or url-safe base64, padded or not.
pub fn b64_decode_any(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    let s = s.trim();
    if s.contains(['-', '_']) {
        return URL_SAFE_NO_PAD.decode(s.trim_end_matches('='));
    }
    if s.ends_with('=') {
        STANDARD.decode(s)
    } else {
        STANDARD_NO_PAD.decode(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_shape() {
        let t = random_token();
        assert_eq!(t.len(), 43);
        assert!(t
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_ne!(t, random_token());
        assert_eq!(b64url_decode(&t).unwrap().len(), 32);
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn uuid_is_v7() {
        assert_eq!(uuid_v7().get_version_num(), 7);
    }

    #[test]
    fn base64_variants() {
        let raw = b"\xfb\xff\x00hello";
        assert_eq!(b64_decode_any(&b64_std(raw)).unwrap(), raw);
        assert_eq!(b64_decode_any(&b64url(raw)).unwrap(), raw);
    }
}
