//! Password hashing and input validation.
//!
//! Argon2id (m = 64 MiB, t = 3, p = 1), hashed on the blocking pool with bounded concurrency
//! so a login burst cannot exhaust memory.

use std::sync::OnceLock;

use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use tokio::sync::Semaphore;

use crate::error::{ApiError, ApiResult};

pub const MIN_PASSWORD_CHARS: usize = 10;
pub const MAX_PASSWORD_BYTES: usize = 1024;

fn argon2() -> Argon2<'static> {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(64 * 1024, 3, 1, None).expect("valid argon2 params"),
    )
}

/// Bounds concurrent Argon2 computations (each holds 64 MiB).
fn hash_slots() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| {
        let cpus = std::thread::available_parallelism().map_or(2, std::num::NonZero::get);
        Semaphore::new(cpus.clamp(2, 8))
    })
}

/// Argon2id PHC string for `password` (runs on the blocking pool).
pub async fn hash_password(password: String) -> ApiResult<String> {
    let _permit = hash_slots().acquire().await.map_err(ApiError::internal)?;
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut rand::rngs::OsRng);
        argon2()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| ApiError::internal(format!("argon2: {e}")))
    })
    .await?
}

/// Constant-work verify. `hash = None` (unknown user, or an app user without a password)
/// still burns one Argon2 computation so response timing does not reveal which emails exist.
pub async fn verify_password(password: String, hash: Option<String>) -> ApiResult<bool> {
    let _permit = hash_slots().acquire().await.map_err(ApiError::internal)?;
    tokio::task::spawn_blocking(move || {
        static DUMMY: OnceLock<String> = OnceLock::new();
        let real = hash.is_some();
        let hash = hash.unwrap_or_else(|| {
            DUMMY
                .get_or_init(|| {
                    let salt = SaltString::generate(&mut rand::rngs::OsRng);
                    argon2()
                        .hash_password(b"canvas-cloud-dummy-password", &salt)
                        .map(|h| h.to_string())
                        .unwrap_or_default()
                })
                .clone()
        });
        let Ok(parsed) = PasswordHash::new(&hash) else {
            return Ok(false);
        };
        let ok = argon2()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        Ok(ok && real)
    })
    .await?
}

/// Trimmed, plausibly-shaped email (`local@domain.tld`-ish, ≤ 254 bytes, no whitespace).
pub fn normalize_email(raw: &str) -> ApiResult<String> {
    let email = raw.trim();
    let valid = email.len() >= 3
        && email.len() <= 254
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !domain.contains('@')
        });
    if valid {
        Ok(email.to_owned())
    } else {
        Err(ApiError::bad_request("invalid email address"))
    }
}

/// At least [`MIN_PASSWORD_CHARS`] characters, at most [`MAX_PASSWORD_BYTES`] bytes.
pub fn validate_password(pw: &str) -> ApiResult<()> {
    if pw.chars().count() < MIN_PASSWORD_CHARS {
        return Err(ApiError::bad_request(format!(
            "password must be at least {MIN_PASSWORD_CHARS} characters"
        )));
    }
    if pw.len() > MAX_PASSWORD_BYTES {
        return Err(ApiError::bad_request("password is too long"));
    }
    Ok(())
}

/// Trimmed, control characters removed, at most `max_chars`; `None` when nothing is left.
pub fn clean_name(raw: Option<&str>, max_chars: usize) -> Option<String> {
    let s: String = raw?
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_validation() {
        assert_eq!(normalize_email("  A@B.co ").unwrap(), "A@B.co");
        for bad in [
            "", "a", "a@b", "@b.co", "a@.co", "a@b.co.", "a b@c.de", "a@b@c.de",
        ] {
            assert!(normalize_email(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn password_rules() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("0123456789").is_ok());
        assert!(validate_password("ññññññññññ").is_ok());
        assert!(validate_password(&"x".repeat(MAX_PASSWORD_BYTES + 1)).is_err());
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name(Some("  a\u{7}b  "), 10).as_deref(), Some("ab"));
        assert_eq!(clean_name(Some("abcdef"), 3).as_deref(), Some("abc"));
        assert_eq!(clean_name(Some("   "), 3), None);
        assert_eq!(clean_name(None, 3), None);
    }

    #[tokio::test]
    async fn hash_and_verify() {
        let h = hash_password("correct horse battery".into()).await.unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=65536,t=3,p=1$"));
        assert!(
            verify_password("correct horse battery".into(), Some(h.clone()))
                .await
                .unwrap()
        );
        assert!(!verify_password("wrong horse battery".into(), Some(h))
            .await
            .unwrap());
        assert!(!verify_password("anything at all".into(), None)
            .await
            .unwrap());
    }
}
