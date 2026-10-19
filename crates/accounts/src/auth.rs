//! Password accounts: `POST /v1/auth/signup`, `/login`, `/logout`, `GET/PATCH /v1/auth/me`,
//! `POST /v1/auth/password`.

use axum::extract::State;
use axum::http::header::USER_AGENT;
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use canvas_cloud_core::error::{ApiError, ApiResult};
use canvas_cloud_core::extract::{JsonBody, OptionalJsonBody};
use canvas_cloud_core::identity::{
    self, AuthUser, NewUser, SignupPolicy, Via, MAX_DISPLAY_NAME_CHARS, MAX_PASSWORD_BYTES,
};
use canvas_cloud_core::state::SharedState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{invalid_credentials, session_required, Error, Result};
use crate::SharedAccounts;

/// Largest accepted body on the auth routes.
pub(crate) const AUTH_BODY_LIMIT: usize = 16 * 1024;
/// New passwords (signup, change) are 10–256 characters. Sign-in still accepts anything an
/// administrator set with the CLI (up to core's byte limit).
pub const MAX_NEW_PASSWORD_CHARS: usize = 256;
/// `pg_advisory_xact_lock` key serializing signups, so the "first password account" rule
/// admits exactly one account even when two arrive at once.
const SIGNUP_LOCK: i64 = 0x6376_632d_7369_676e; // "cvc-sign"

/// The `user` object of every auth response.
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct UserView {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

const USER_SQL: &str = "SELECT id, email, display_name, created_at FROM users WHERE id = $1";

pub(crate) async fn user_view<'e, E>(db: E, id: Uuid) -> ApiResult<UserView>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_as(USER_SQL).bind(id).fetch_one(db).await?)
}

#[derive(Serialize)]
pub(crate) struct SessionResponse {
    token: String,
    user: UserView,
}

#[derive(Deserialize)]
pub(crate) struct SignupRequest {
    email: String,
    password: String,
    display_name: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct LoginRequest {
    email: String,
    password: String,
}

#[derive(Default, Deserialize)]
pub(crate) struct LogoutRequest {
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
pub(crate) struct PasswordRequest {
    #[serde(alias = "old")]
    current: String,
    new: String,
}

#[derive(Deserialize)]
pub(crate) struct UpdateMeRequest {
    display_name: Option<String>,
}

/// Trimmed, lower-cased, plausibly-shaped email (the same rule as canvas invites).
pub(crate) fn account_email(raw: &str) -> ApiResult<String> {
    Ok(identity::normalize_email(raw)?.to_lowercase())
}

/// 10–256 characters (and core's byte cap).
pub(crate) fn validate_new_password(pw: &str) -> ApiResult<()> {
    identity::validate_password(pw)?;
    if pw.chars().count() > MAX_NEW_PASSWORD_CHARS {
        return Err(ApiError::bad_request(format!(
            "password must be at most {MAX_NEW_PASSWORD_CHARS} characters"
        )));
    }
    Ok(())
}

pub(crate) fn user_agent(headers: &HeaderMap) -> Option<&str> {
    headers.get(USER_AGENT).and_then(|v| v.to_str().ok())
}

/// `Ok` if the policy admits a password signup for `email`; else `403 signup_closed` or
/// `403 signup_domain_not_allowed`.
fn check_signup_policy(policy: &SignupPolicy, email: &str) -> ApiResult<()> {
    if policy.allows(email) {
        Ok(())
    } else if policy.open() {
        Err(ApiError::Denied {
            code: "signup_domain_not_allowed",
            message: "sign-up on this server is limited to certain email domains",
        })
    } else {
        Err(ApiError::Denied {
            code: "signup_closed",
            message: "sign-up is closed on this server; ask an administrator for an account",
        })
    }
}

/// `409 email_taken` — the same code apps get from `PUT /v1/apps/self/users/{subject}`.
fn email_taken() -> Error {
    Error::coded(
        StatusCode::CONFLICT,
        "email_taken",
        "an account with this email already exists",
    )
}

/// `POST /v1/auth/signup {email, password, display_name?}` → `201 {token, user}`.
pub(crate) async fn signup(
    State(state): State<SharedState>,
    Extension(shared): Extension<SharedAccounts>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<SignupRequest, AUTH_BODY_LIMIT>,
) -> Result<(StatusCode, Json<SessionResponse>)> {
    let email = account_email(&req.email)?;
    validate_new_password(&req.password)?;
    let display_name = identity::clean_name(req.display_name.as_deref(), MAX_DISPLAY_NAME_CHARS);
    shared.limits.check(&state.cfg, &email)?;
    check_signup_policy(&SignupPolicy::load(&state.db).await?, &email)?;
    // An existing user — including a password-less one a linked app provisioned — is never
    // claimed by a signup (that would hand their canvases to whoever typed the email).
    if identity::user_id_by_email(&state.db, &email).await?.is_some() {
        return Err(email_taken());
    }
    let hash = identity::hash_password(req.password).await?;

    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SIGNUP_LOCK)
        .execute(&mut *tx)
        .await?;
    check_signup_policy(&SignupPolicy::load(&mut *tx).await?, &email)?;
    let id = identity::insert_user(
        &mut *tx,
        &NewUser {
            email: &email,
            display_name: display_name.as_deref(),
            password_hash: Some(&hash),
            ..NewUser::default()
        },
    )
    .await
    .map_err(|e| match e {
        ApiError::Conflict(_) => email_taken(),
        e => e.into(),
    })?;
    let token = identity::create_session(&mut *tx, id, user_agent(&headers)).await?;
    let user = user_view(&mut *tx, id).await?;
    tx.commit().await?;
    tracing::Span::current().record("user_id", tracing::field::display(id));
    tracing::info!(user_id = %id, "signup");
    Ok((StatusCode::CREATED, Json(SessionResponse { token, user })))
}

/// `POST /v1/auth/login {email, password}` → `{token, user}`.
pub(crate) async fn login(
    State(state): State<SharedState>,
    Extension(shared): Extension<SharedAccounts>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<LoginRequest, AUTH_BODY_LIMIT>,
) -> Result<Json<SessionResponse>> {
    let email = req.email.trim().to_lowercase();
    shared.limits.check(&state.cfg, &email)?;
    if req.password.len() > MAX_PASSWORD_BYTES {
        return Err(invalid_credentials().into());
    }
    let row: Option<(Uuid, Option<String>, bool)> = sqlx::query_as(
        "SELECT id, password_hash, disabled FROM users WHERE lower(email) = lower($1)",
    )
    .bind(&email)
    .fetch_optional(&state.db)
    .await?;
    // Unknown email and password-less (app / OIDC) users verify against a dummy hash, so the
    // answer and its timing are the same as for a wrong password.
    let hash = row.as_ref().and_then(|r| r.1.clone());
    let ok = identity::verify_password(req.password, hash).await?;
    let Some((id, _, disabled)) = row.filter(|_| ok) else {
        tracing::info!("login failed");
        return Err(invalid_credentials().into());
    };
    if disabled {
        return Err(ApiError::UnauthorizedWith {
            code: "account_disabled",
            message: "this account is disabled",
        }
        .into());
    }
    let mut tx = state.db.begin().await?;
    let token = identity::create_session(&mut *tx, id, user_agent(&headers)).await?;
    let user = user_view(&mut *tx, id).await?;
    tx.commit().await?;
    tracing::Span::current().record("user_id", tracing::field::display(id));
    tracing::info!(user_id = %id, "login");
    Ok(Json(SessionResponse { token, user }))
}

/// `POST /v1/auth/logout [{all}]` → `{ok, revoked}` (session only).
pub(crate) async fn logout(
    State(state): State<SharedState>,
    user: AuthUser,
    OptionalJsonBody(req): OptionalJsonBody<LogoutRequest, 1024>,
) -> Result<Json<Value>> {
    let session_id = user.session_id().ok_or_else(session_required)?;
    let revoked = if req.all {
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(user.user_id)
            .execute(&state.db)
            .await?
    } else {
        sqlx::query("DELETE FROM sessions WHERE id = $1 AND user_id = $2")
            .bind(session_id)
            .bind(user.user_id)
            .execute(&state.db)
            .await?
    }
    .rows_affected();
    tracing::info!(user_id = %user.user_id, revoked, "logout");
    Ok(Json(json!({ "ok": true, "revoked": revoked })))
}

/// `via` of `GET /v1/auth/me`: `{"kind":"session"}`, `{"kind":"token","scope":…}` or
/// `{"kind":"app","app_id":…}`.
pub(crate) fn via_json(via: &Via) -> Value {
    match via {
        Via::Session { .. } => json!({ "kind": "session" }),
        Via::Token { scope, .. } => json!({ "kind": "token", "scope": scope }),
        Via::App { app_id, .. } => json!({ "kind": "app", "app_id": app_id }),
    }
}

/// `GET /v1/auth/me` → `{user, via}`.
pub(crate) async fn me(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>> {
    let view = user_view(&state.db, user.user_id).await?;
    Ok(Json(json!({ "user": view, "via": via_json(&user.via) })))
}

/// `PATCH /v1/auth/me {display_name}` → `{user, via}`.
pub(crate) async fn update_me(
    State(state): State<SharedState>,
    user: AuthUser,
    JsonBody(req): JsonBody<UpdateMeRequest, AUTH_BODY_LIMIT>,
) -> Result<Json<Value>> {
    let raw = req
        .display_name
        .ok_or_else(|| ApiError::bad_request("display_name is required"))?;
    if raw.trim().chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(ApiError::bad_request(format!(
            "display_name must be at most {MAX_DISPLAY_NAME_CHARS} characters"
        ))
        .into());
    }
    let name = identity::clean_name(Some(&raw), MAX_DISPLAY_NAME_CHARS)
        .ok_or_else(|| ApiError::bad_request("display_name must not be empty"))?;
    let view: UserView = sqlx::query_as(
        "UPDATE users SET display_name = $2, updated_at = now() WHERE id = $1
         RETURNING id, email, display_name, created_at",
    )
    .bind(user.user_id)
    .bind(name)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({ "user": view, "via": via_json(&user.via) })))
}

/// `POST /v1/auth/password {current, new}` → `{ok, revoked_sessions}` (session only). Every
/// other session of the user is signed out; personal access tokens keep working.
pub(crate) async fn change_password(
    State(state): State<SharedState>,
    Extension(shared): Extension<SharedAccounts>,
    user: AuthUser,
    JsonBody(req): JsonBody<PasswordRequest, AUTH_BODY_LIMIT>,
) -> Result<Json<Value>> {
    let session_id = user.session_id().ok_or_else(session_required)?;
    validate_new_password(&req.new)?;
    shared.limits.check(&state.cfg, &user.email)?;
    let hash: Option<String> = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
        .bind(user.user_id)
        .fetch_one(&state.db)
        .await?;
    let Some(hash) = hash else {
        return Err(ApiError::Denied {
            code: "password_not_set",
            message: "this account signs in without a password; an administrator can set one",
        }
        .into());
    };
    if req.current.len() > MAX_PASSWORD_BYTES
        || !identity::verify_password(req.current, Some(hash)).await?
    {
        // 403, not 401: the session is fine, the old password is not — clients must not
        // treat this as being signed out.
        return Err(ApiError::Denied {
            code: "invalid_credentials",
            message: "the current password is wrong",
        }
        .into());
    }
    let new_hash = identity::hash_password(req.new).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
        .bind(user.user_id)
        .bind(&new_hash)
        .execute(&mut *tx)
        .await?;
    let revoked = sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id <> $2")
        .bind(user.user_id)
        .bind(session_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    tracing::info!(user_id = %user.user_id, revoked, "password changed");
    Ok(Json(json!({ "ok": true, "revoked_sessions": revoked })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_lower_cased_and_validated() {
        assert_eq!(account_email(" Ann@Example.COM ").unwrap(), "ann@example.com");
        for bad in ["", "ann", "ann@", "@x.io", "a b@c.io", "a@b"] {
            assert!(account_email(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn new_password_bounds() {
        assert!(validate_new_password("123456789").is_err());
        assert!(validate_new_password("1234567890").is_ok());
        assert!(validate_new_password(&"é".repeat(256)).is_ok());
        assert!(validate_new_password(&"x".repeat(257)).is_err());
    }

    #[test]
    fn policy_errors() {
        let closed = SignupPolicy::default();
        assert_eq!(
            check_signup_policy(&closed, "a@b.co").unwrap_err().code(),
            "signup_closed"
        );
        let domains = SignupPolicy {
            allow_signup: true,
            signup_domains: vec!["example.com".into()],
            first_user: false,
        };
        assert!(check_signup_policy(&domains, "a@example.com").is_ok());
        assert_eq!(
            check_signup_policy(&domains, "a@other.org")
                .unwrap_err()
                .code(),
            "signup_domain_not_allowed"
        );
        let first = SignupPolicy {
            first_user: true,
            ..domains
        };
        assert!(check_signup_policy(&first, "a@other.org").is_ok());
    }

    #[test]
    fn via_shapes() {
        let id = Uuid::nil();
        assert_eq!(
            via_json(&Via::Session { session_id: id }),
            json!({"kind":"session"})
        );
        assert_eq!(
            via_json(&Via::Token {
                token_id: id,
                scope: identity::Scope::Read
            }),
            json!({"kind":"token","scope":"read"})
        );
        assert_eq!(
            via_json(&Via::App {
                app_id: id,
                subject: "s".into()
            }),
            json!({"kind":"app","app_id":id})
        );
    }
}
