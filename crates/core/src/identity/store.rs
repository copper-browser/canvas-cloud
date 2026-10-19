//! Identity rows: users, sessions, access tokens, apps, server settings and the admin audit
//! log. Small helpers the CLI, the accounts and apps crates and the tests share.

use serde::Serialize;
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use super::credentials::{mint, CredentialKind};
use super::{Scope, SESSION_TTL_DAYS};
use crate::error::{is_unique_violation, ApiError, ApiResult};
use crate::ids;

/// `users.display_name` limit.
pub const MAX_DISPLAY_NAME_CHARS: usize = 200;
/// `app_users.subject` limit.
pub const MAX_SUBJECT_CHARS: usize = 200;
/// `sessions.user_agent` is truncated to this.
pub const MAX_USER_AGENT_CHARS: usize = 512;

// ---------------------------------------------------------------------------------------------
// Users

/// A user to create with [`insert_user`].
#[derive(Clone, Debug, Default)]
pub struct NewUser<'a> {
    /// Use this id (e.g. an app-provided one); `None` = a fresh `UUIDv7`.
    pub id: Option<Uuid>,
    /// Already normalized ([`super::normalize_email`]).
    pub email: &'a str,
    /// `None` = [`default_display_name`] of the email.
    pub display_name: Option<&'a str>,
    /// From [`super::hash_password`]; `None` for a federated (app/OIDC) user.
    pub password_hash: Option<&'a str>,
    pub disabled: bool,
}

/// Insert a user; returns its id. `409 {"error":"conflict"}` if the email (or id) is taken.
pub async fn insert_user<'e, E>(db: E, user: &NewUser<'_>) -> ApiResult<Uuid>
where
    E: sqlx::PgExecutor<'e>,
{
    let id = user.id.unwrap_or_else(ids::uuid_v7);
    let display_name = user
        .display_name
        .map_or_else(|| default_display_name(user.email), ToOwned::to_owned);
    sqlx::query(
        "INSERT INTO users (id, email, display_name, password_hash, disabled)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(user.email)
    .bind(display_name)
    .bind(user.password_hash)
    .bind(user.disabled)
    .execute(db)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            ApiError::Conflict(json!({ "message": "an account with this email already exists" }))
        } else {
            e.into()
        }
    })?;
    Ok(id)
}

/// Default display name for an email: its local part.
pub fn default_display_name(email: &str) -> String {
    email.split('@').next().unwrap_or_default().to_owned()
}

/// Email of a user (any state), `None` if no such user.
pub async fn user_email(db: &sqlx::PgPool, user_id: Uuid) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(db)
        .await?)
}

/// Case-insensitive lookup of a user id by email (any state).
pub async fn user_id_by_email(db: &sqlx::PgPool, email: &str) -> ApiResult<Option<Uuid>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM users WHERE lower(email) = lower($1)")
            .bind(email.trim())
            .fetch_optional(db)
            .await?,
    )
}

/// Number of users (any state).
pub async fn count_users(db: &sqlx::PgPool) -> ApiResult<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(db)
        .await?)
}

// ---------------------------------------------------------------------------------------------
// Sessions and tokens

/// Start a session for `user_id`; returns the `cvs_…` token (shown to the client once).
pub async fn create_session<'e, E>(
    db: E,
    user_id: Uuid,
    user_agent: Option<&str>,
) -> ApiResult<String>
where
    E: sqlx::PgExecutor<'e>,
{
    let (token, digest) = mint(CredentialKind::Session);
    let user_agent = user_agent.and_then(|ua| super::clean_name(Some(ua), MAX_USER_AGENT_CHARS));
    sqlx::query(
        "INSERT INTO sessions (id, user_id, token_sha256, user_agent, expires_at)
         VALUES ($1, $2, $3, $4, now() + make_interval(days => $5))",
    )
    .bind(ids::uuid_v7())
    .bind(user_id)
    .bind(&digest[..])
    .bind(user_agent)
    .bind(SESSION_TTL_DAYS)
    .execute(db)
    .await?;
    Ok(token)
}

/// End one session (logout). Returns whether it existed.
pub async fn revoke_session(db: &sqlx::PgPool, session_id: Uuid) -> ApiResult<bool> {
    Ok(sqlx::query("DELETE FROM sessions WHERE id = $1")
        .bind(session_id)
        .execute(db)
        .await?
        .rows_affected()
        > 0)
}

/// Mint a personal access token; returns `(id, "cvt_…")` (the token is shown once).
pub async fn create_access_token<'e, E>(
    db: E,
    user_id: Uuid,
    name: &str,
    scope: Scope,
    expires_at: Option<OffsetDateTime>,
) -> ApiResult<(Uuid, String)>
where
    E: sqlx::PgExecutor<'e>,
{
    let name = super::clean_name(Some(name), MAX_DISPLAY_NAME_CHARS)
        .ok_or_else(|| ApiError::bad_request("token name must not be empty"))?;
    let id = ids::uuid_v7();
    let (token, digest) = mint(CredentialKind::Token);
    sqlx::query(
        "INSERT INTO access_tokens (id, user_id, name, token_sha256, scope, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(user_id)
    .bind(name)
    .bind(&digest[..])
    .bind(scope.as_str())
    .bind(expires_at)
    .execute(db)
    .await?;
    Ok((id, token))
}

// ---------------------------------------------------------------------------------------------
// Apps

/// One linked app (never the key).
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct AppRow {
    pub id: Uuid,
    pub name: String,
    pub allow_import: bool,
    pub trust_email: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// Number of users linked to the app.
    pub users: i64,
}

/// Register a linked app; returns `(id, "cva_…")` (the key is shown once).
pub async fn create_app(
    db: &sqlx::PgPool,
    name: &str,
    allow_import: bool,
    trust_email: bool,
) -> ApiResult<(Uuid, String)> {
    let name = super::clean_name(Some(name), MAX_DISPLAY_NAME_CHARS)
        .ok_or_else(|| ApiError::bad_request("app name must not be empty"))?;
    let id = ids::uuid_v7();
    let (key, digest) = mint(CredentialKind::App);
    sqlx::query(
        "INSERT INTO apps (id, name, key_sha256, allow_import, trust_email)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(name)
    .bind(&digest[..])
    .bind(allow_import)
    .bind(trust_email)
    .execute(db)
    .await?;
    Ok((id, key))
}

/// Every app, newest first.
pub async fn list_apps(db: &sqlx::PgPool) -> ApiResult<Vec<AppRow>> {
    Ok(sqlx::query_as(
        "SELECT a.id, a.name, a.allow_import, a.trust_email, a.created_at, a.last_used_at,
                a.revoked_at,
                (SELECT count(*) FROM app_users au WHERE au.app_id = a.id) AS users
         FROM apps a ORDER BY a.created_at DESC, a.id DESC",
    )
    .fetch_all(db)
    .await?)
}

/// Revoke an app's key (its users and their canvases stay). `false` if no such live app.
pub async fn revoke_app(db: &sqlx::PgPool, id: Uuid) -> ApiResult<bool> {
    Ok(
        sqlx::query("UPDATE apps SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
            .bind(id)
            .execute(db)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Replace an app's key (the old one stops working immediately); returns the new `cva_…`.
/// Also un-revokes it. `None` if no such app.
pub async fn rotate_app_key(db: &sqlx::PgPool, id: Uuid) -> ApiResult<Option<String>> {
    let (key, digest) = mint(CredentialKind::App);
    let n = sqlx::query("UPDATE apps SET key_sha256 = $2, revoked_at = NULL WHERE id = $1")
        .bind(id)
        .bind(&digest[..])
        .execute(db)
        .await?
        .rows_affected();
    Ok((n > 0).then_some(key))
}

// ---------------------------------------------------------------------------------------------
// Server settings + audit

/// A runtime setting (`server_settings.value`), `None` if unset.
pub async fn get_setting(db: &sqlx::PgPool, key: &str) -> ApiResult<Option<Value>> {
    Ok(
        sqlx::query_scalar("SELECT value FROM server_settings WHERE key = $1")
            .bind(key)
            .fetch_optional(db)
            .await?,
    )
}

/// Set (upsert) a runtime setting.
pub async fn set_setting(db: &sqlx::PgPool, key: &str, value: &Value) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO server_settings (key, value, updated_at) VALUES ($1, $2, now())
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
    )
    .bind(key)
    .bind(value)
    .execute(db)
    .await?;
    Ok(())
}

/// The effective self-signup policy (`server_settings`: `allow_signup`, `signup_domains`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SignupPolicy {
    /// `allow_signup` (default `false`).
    pub allow_signup: bool,
    /// `signup_domains`: when non-empty, only emails in these domains (lower-case) may sign
    /// up.
    pub signup_domains: Vec<String>,
    /// No user exists yet: the first account may always be created.
    pub first_user: bool,
}

impl SignupPolicy {
    pub async fn load(db: &sqlx::PgPool) -> ApiResult<Self> {
        let (has_users, allow, domains): (bool, Option<Value>, Option<Value>) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM users),
                    (SELECT value FROM server_settings WHERE key = 'allow_signup'),
                    (SELECT value FROM server_settings WHERE key = 'signup_domains')",
        )
        .fetch_one(db)
        .await?;
        Ok(Self {
            allow_signup: allow.and_then(|v| v.as_bool()).unwrap_or(false),
            signup_domains: domains
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(Value::as_str)
                .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
                .filter(|d| !d.is_empty())
                .collect(),
            first_user: !has_users,
        })
    }

    /// Whether anyone may sign up right now (ignoring the email domain).
    pub fn open(&self) -> bool {
        self.first_user || self.allow_signup
    }

    /// Whether `email` may sign up.
    pub fn allows(&self, email: &str) -> bool {
        if self.first_user {
            return true;
        }
        if !self.allow_signup {
            return false;
        }
        if self.signup_domains.is_empty() {
            return true;
        }
        let domain = email
            .rsplit_once('@')
            .map(|(_, d)| d.to_ascii_lowercase())
            .unwrap_or_default();
        self.signup_domains.contains(&domain)
    }
}

/// Whether self-signup is open (`GET /v1/info` `signup`).
pub async fn signup_allowed(db: &sqlx::PgPool) -> ApiResult<bool> {
    Ok(SignupPolicy::load(db).await?.open())
}

/// Append to `admin_audit`. Best effort: a failure is logged, never returned.
pub async fn audit(db: &sqlx::PgPool, actor: &str, action: &str, target: &str, detail: Value) {
    let r = sqlx::query(
        "INSERT INTO admin_audit (actor, action, target, detail) VALUES ($1, $2, $3, $4)",
    )
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail)
    .execute(db)
    .await;
    if let Err(e) = r {
        tracing::warn!(error = %e, action, "writing admin audit row failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signup_policy_rules() {
        let first = SignupPolicy {
            first_user: true,
            ..SignupPolicy::default()
        };
        assert!(first.open() && first.allows("a@x.com"));
        let closed = SignupPolicy::default();
        assert!(!closed.open() && !closed.allows("a@x.com"));
        let open = SignupPolicy {
            allow_signup: true,
            ..SignupPolicy::default()
        };
        assert!(open.allows("a@anything.org"));
        let domains = SignupPolicy {
            allow_signup: true,
            signup_domains: vec!["example.com".into()],
            first_user: false,
        };
        assert!(domains.allows("A@Example.COM"));
        assert!(!domains.allows("a@example.com.evil.org"));
        assert!(!domains.allows("a@sub.example.com"));
    }

    #[test]
    fn display_name_default() {
        assert_eq!(default_display_name("ann@example.com"), "ann");
    }
}
