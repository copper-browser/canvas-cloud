//! Personal access tokens: `GET/POST /v1/tokens`, `DELETE /v1/tokens/{id}`. Only a signed-in
//! session manages them — a token cannot mint, list or revoke tokens.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use canvas_cloud_core::error::ApiError;
use canvas_cloud_core::extract::JsonBody;
use canvas_cloud_core::identity::{self, AuthUser, Scope, MAX_DISPLAY_NAME_CHARS};
use canvas_cloud_core::state::SharedState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{session_required, Result};

/// Live (not revoked, not expired) tokens a user may hold.
pub const MAX_ACTIVE_TOKENS: i64 = 50;
/// Longest `expires_in_days` (omit it for a token that never expires).
pub const MAX_EXPIRES_IN_DAYS: u32 = 3650;

/// One token as listed (never the secret).
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub(crate) struct TokenView {
    id: Uuid,
    name: String,
    scope: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    last_used_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
}

const LIVE: &str = "revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())";

#[derive(Deserialize)]
pub(crate) struct CreateToken {
    name: Option<String>,
    scope: Option<String>,
    expires_in_days: Option<u32>,
}

fn require_session(user: &AuthUser) -> Result<()> {
    user.session_id().map(|_| ()).ok_or_else(|| session_required().into())
}

/// `GET /v1/tokens` → `{tokens: [{id, name, scope, created_at, last_used_at, expires_at}],
/// limit}` — live tokens, newest first.
pub(crate) async fn list(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>> {
    require_session(&user)?;
    let tokens: Vec<TokenView> = sqlx::query_as(&format!(
        "SELECT id, name, scope, created_at, last_used_at, expires_at FROM access_tokens
         WHERE user_id = $1 AND {LIVE} ORDER BY created_at DESC, id DESC"
    ))
    .bind(user.user_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        json!({ "tokens": tokens, "limit": MAX_ACTIVE_TOKENS }),
    ))
}

/// `POST /v1/tokens {name, scope: "read"|"write", expires_in_days?}` → `201 {token, id, name,
/// scope, created_at, last_used_at, expires_at}`. The `cvt_…` secret is in this response only.
pub(crate) async fn create(
    State(state): State<SharedState>,
    user: AuthUser,
    JsonBody(req): JsonBody<CreateToken, 4096>,
) -> Result<(StatusCode, Json<Value>)> {
    require_session(&user)?;
    let raw_name = req.name.unwrap_or_default();
    if raw_name.trim().chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(ApiError::bad_request(format!(
            "name must be at most {MAX_DISPLAY_NAME_CHARS} characters"
        ))
        .into());
    }
    let name = identity::clean_name(Some(&raw_name), MAX_DISPLAY_NAME_CHARS)
        .ok_or_else(|| ApiError::bad_request("name must not be empty"))?;
    let scope = req
        .scope
        .as_deref()
        .and_then(Scope::parse)
        .ok_or_else(|| ApiError::bad_request("scope must be \"read\" or \"write\""))?;
    let expires_at = match req.expires_in_days {
        None => None,
        Some(d) if (1..=MAX_EXPIRES_IN_DAYS).contains(&d) => {
            Some(OffsetDateTime::now_utc() + time::Duration::days(i64::from(d)))
        }
        Some(_) => {
            return Err(ApiError::bad_request(format!(
                "expires_in_days must be between 1 and {MAX_EXPIRES_IN_DAYS}"
            ))
            .into())
        }
    };

    let mut tx = state.db.begin().await?;
    // Serialize this user's token creation so the limit holds under concurrency.
    sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
        .bind(user.user_id)
        .execute(&mut *tx)
        .await?;
    let live: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM access_tokens WHERE user_id = $1 AND {LIVE}"
    ))
    .bind(user.user_id)
    .fetch_one(&mut *tx)
    .await?;
    if live >= MAX_ACTIVE_TOKENS {
        return Err(ApiError::Denied {
            code: "token_limit",
            message: "you already have the maximum number of access tokens; revoke one first",
        }
        .into());
    }
    let (id, token) =
        identity::create_access_token(&mut *tx, user.user_id, &name, scope, expires_at).await?;
    let view: TokenView = sqlx::query_as(
        "SELECT id, name, scope, created_at, last_used_at, expires_at FROM access_tokens
         WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(user_id = %user.user_id, token_id = %id, scope = %scope, "access token created");
    let mut body = serde_json::to_value(view).map_err(ApiError::internal)?;
    body["token"] = Value::String(token);
    Ok((StatusCode::CREATED, Json(body)))
}

/// `DELETE /v1/tokens/{id}` → `{ok: true}`; `404` if it is not one of the caller's live
/// tokens.
pub(crate) async fn revoke(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Value>> {
    require_session(&user)?;
    let id = Uuid::parse_str(&raw).map_err(|_| ApiError::NotFound)?;
    let n = sqlx::query(
        "UPDATE access_tokens SET revoked_at = now()
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user.user_id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(ApiError::NotFound.into());
    }
    tracing::info!(user_id = %user.user_id, token_id = %id, "access token revoked");
    Ok(Json(json!({ "ok": true })))
}
