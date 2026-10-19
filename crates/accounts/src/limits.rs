//! Per-email GCRA bucket for the password routes (login, signup, password change), on top of
//! core's per-IP `/v1/auth/*` limiter: a credential-stuffing run spread over many IPs still
//! gets `limits.auth_per_minute` tries per email address per minute.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use canvas_cloud_core::config::Config;
use canvas_cloud_core::error::ApiError;
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};

#[derive(Default)]
pub(crate) struct EmailLimiter {
    limiter: OnceLock<DefaultKeyedRateLimiter<String>>,
    calls: AtomicU64,
}

impl EmailLimiter {
    /// `Ok` if `email` (any case) may try again now, else `429 rate_limited`.
    pub(crate) fn check(&self, cfg: &Config, email: &str) -> Result<(), ApiError> {
        let limiter = self.limiter.get_or_init(|| {
            let n = NonZeroU32::new(cfg.limits.auth_per_minute.max(1)).expect("non-zero");
            RateLimiter::keyed(Quota::per_minute(n))
        });
        if self.calls.fetch_add(1, Ordering::Relaxed) % 4096 == 4095 {
            limiter.retain_recent();
            limiter.shrink_to_fit();
        }
        let key = email.trim().to_lowercase();
        if limiter.check_key(&key).is_ok() {
            Ok(())
        } else {
            metrics::counter!("auth_email_rate_limited_total").increment(1);
            tracing::warn!("per-email auth rate limit exceeded");
            Err(ApiError::RateLimited)
        }
    }
}
