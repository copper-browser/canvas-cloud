//! `POST /v1/apps/self/users/{subject}/sessions`: a linked app that an operator allowed to
//! (`allow_sessions`, plus `trust_email`: it authenticates its people itself) signs one of its
//! users in to this server — a `cvs_…` session like a password or OIDC sign-in gives them, but
//! tagged with the app (`sessions.created_by_app`) so revoking or rotating the app ends it, and
//! capped at `limits.app_session_ttl_hours`. This is what lets another site act as a sign-in
//! provider (`[[delegated_sign_in]]`, see `docs/auth.md`).

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, USER_AGENT};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use canvas_cloud_core::config::Config;
use canvas_cloud_core::error::ApiError;
use canvas_cloud_core::extract::OptionalJsonBody;
use canvas_cloud_core::identity::{self, AppAuth};
use canvas_cloud_core::state::SharedState;
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use serde::Deserialize;
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::common::{Error, Result};
use crate::users::actor;

/// The (optional) request body: `{}` — nothing to say yet.
#[derive(Debug, Default, Deserialize)]
pub struct SessionBody {}

/// Per-app GCRA bucket: `limits.app_sessions_per_minute` mints per app per minute (one
/// instance per composed router, so tests with different configs do not share buckets).
#[derive(Default)]
pub struct SessionLimiter {
    limiter: OnceLock<DefaultKeyedRateLimiter<Uuid>>,
    calls: AtomicU64,
}

impl SessionLimiter {
    /// `Ok` if `app_id` may mint a session now, else `429 rate_limited`.
    fn check(&self, cfg: &Config, app_id: Uuid) -> Result<(), ApiError> {
        let limiter = self.limiter.get_or_init(|| {
            let n = NonZeroU32::new(cfg.limits.app_sessions_per_minute.max(1)).expect("non-zero");
            RateLimiter::keyed(Quota::per_minute(n))
        });
        if self.calls.fetch_add(1, Ordering::Relaxed) % 4096 == 4095 {
            limiter.retain_recent();
            limiter.shrink_to_fit();
        }
        if limiter.check_key(&app_id).is_ok() {
            Ok(())
        } else {
            metrics::counter!("app_sessions_rate_limited_total").increment(1);
            tracing::warn!(app_id = %app_id, "per-app session rate limit exceeded");
            Err(ApiError::RateLimited)
        }
    }
}

pub type SharedSessionLimiter = Arc<SessionLimiter>;

/// `POST /v1/apps/self/users/{subject}/sessions [{}]` →
/// `201 {token: "cvs_…", user_id, expires_at}` (`Cache-Control: no-store`).
///
/// * the app must have `allow_sessions` (`403 sessions_not_allowed`; off unless an operator
///   turns it on) and `trust_email` (`403 trust_email_required`): it vouches for who the
///   person is;
/// * `subject` must be one of this app's users (`404 unknown_subject`), not disabled
///   (`403 account_disabled`);
/// * at most `limits.app_sessions_per_minute` per app (`429 rate_limited`).
///
/// The session ends 30 days after its last use like any other, and at the latest
/// `limits.app_session_ttl_hours` after it was minted. It is tagged with the app: `apps revoke`
/// and `apps rotate` delete it and revoke the access tokens created with it. The token is never
/// logged; an `app.user.session` audit row records app, user and subject.
pub async fn create(
    State(state): State<SharedState>,
    Extension(limiter): Extension<SharedSessionLimiter>,
    app: AppAuth,
    Path(subject): Path<String>,
    headers: HeaderMap,
    OptionalJsonBody(_body): OptionalJsonBody<SessionBody, 1024>,
) -> Result<Response> {
    check_policy(app.allow_sessions, app.trust_email)?;
    limiter.check(&state.cfg, app.app_id)?;
    let row: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT u.id, u.disabled FROM app_users au JOIN users u ON u.id = au.user_id
         WHERE au.app_id = $1 AND au.subject = $2",
    )
    .bind(app.app_id)
    .bind(&subject)
    .fetch_optional(&state.db)
    .await?;
    let Some((user_id, disabled)) = row else {
        return Err(Error::coded(
            StatusCode::NOT_FOUND,
            "unknown_subject",
            "this app has not provisioned that user (PUT /v1/apps/self/users/{subject})",
        ));
    };
    if disabled {
        return Err(Error::coded(
            StatusCode::FORBIDDEN,
            "account_disabled",
            "this account is disabled",
        ));
    }
    let user_agent = headers.get(USER_AGENT).and_then(|v| v.to_str().ok());
    // Re-check the app under a share lock on its row, with the key this request used: a
    // concurrent revoke, rotate or policy change either waits for this session to commit (and
    // then ends it) or has committed already (and the mint is refused).
    let mut tx = state.db.begin().await?;
    let policy: Option<(bool, bool)> = sqlx::query_as(
        "SELECT allow_sessions, trust_email FROM apps
         WHERE id = $1 AND key_sha256 = $2 AND revoked_at IS NULL FOR SHARE",
    )
    .bind(app.app_id)
    .bind(&app.key_digest()[..])
    .fetch_optional(&mut *tx)
    .await?;
    let (allow_sessions, trust_email) = policy.ok_or(ApiError::Unauthorized("unauthorized"))?;
    check_policy(allow_sessions, trust_email)?;
    let (token, expires_at) = identity::create_app_session(
        &mut *tx,
        user_id,
        user_agent,
        app.app_id,
        state.cfg.limits.app_session_max_hours(),
    )
    .await?;
    tx.commit().await?;
    metrics::counter!("app_sessions_created_total").increment(1);
    tracing::Span::current().record("user_id", tracing::field::display(user_id));
    tracing::info!(app_id = %app.app_id, user_id = %user_id, "app session created");
    identity::audit(
        &state.db,
        &actor(&app),
        "app.user.session",
        &user_id.to_string(),
        json!({ "subject": subject }),
    )
    .await;
    let expires_at = expires_at
        .format(&Rfc3339)
        .map_err(|e| ApiError::internal(format!("formatting expires_at: {e}")))?;
    let mut resp = (
        StatusCode::CREATED,
        Json(json!({ "token": token, "user_id": user_id, "expires_at": expires_at })),
    )
        .into_response();
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

/// Minting needs both opt-ins: `allow_sessions` (`403 sessions_not_allowed`) and
/// `trust_email` (`403 trust_email_required`).
fn check_policy(allow_sessions: bool, trust_email: bool) -> Result<()> {
    if !allow_sessions {
        return Err(Error::coded(
            StatusCode::FORBIDDEN,
            "sessions_not_allowed",
            "this app may not sign its users in; an operator allows it with `canvas-cloud apps set <app> --allow-sessions true`",
        ));
    }
    if !trust_email {
        return Err(Error::coded(
            StatusCode::FORBIDDEN,
            "trust_email_required",
            "only an app with trust_email may sign its users in (`canvas-cloud apps set <app> --trust-email true`)",
        ));
    }
    Ok(())
}
