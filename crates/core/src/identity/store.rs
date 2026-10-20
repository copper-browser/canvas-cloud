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
    Ok(create_session_with_expiry(db, user_id, user_agent).await?.0)
}

/// [`create_session`], also returning when the session expires unless it is used
/// ([`SESSION_TTL_DAYS`] from now; every use slides it).
pub async fn create_session_with_expiry<'e, E>(
    db: E,
    user_id: Uuid,
    user_agent: Option<&str>,
) -> ApiResult<(String, OffsetDateTime)>
where
    E: sqlx::PgExecutor<'e>,
{
    insert_session(db, user_id, user_agent, None, None).await
}

/// A session a linked app minted for one of its users
/// (`POST /v1/apps/self/users/{subject}/sessions`): tagged with `app_id`
/// (`sessions.created_by_app`), so revoking or rotating the app ends it. With
/// `max_lifetime_hours` it never slides past that many hours from now. Returns the `cvs_…`
/// token and when it expires unless it is used.
pub async fn create_app_session<'e, E>(
    db: E,
    user_id: Uuid,
    user_agent: Option<&str>,
    app_id: Uuid,
    max_lifetime_hours: Option<u32>,
) -> ApiResult<(String, OffsetDateTime)>
where
    E: sqlx::PgExecutor<'e>,
{
    insert_session(db, user_id, user_agent, Some(app_id), max_lifetime_hours).await
}

async fn insert_session<'e, E>(
    db: E,
    user_id: Uuid,
    user_agent: Option<&str>,
    created_by_app: Option<Uuid>,
    max_lifetime_hours: Option<u32>,
) -> ApiResult<(String, OffsetDateTime)>
where
    E: sqlx::PgExecutor<'e>,
{
    let (token, digest) = mint(CredentialKind::Session);
    let user_agent = user_agent.and_then(|ua| super::clean_name(Some(ua), MAX_USER_AGENT_CHARS));
    let max_hours = max_lifetime_hours.map(|h| i32::try_from(h).unwrap_or(i32::MAX));
    // LEAST ignores NULL: without a cap this is the ordinary sliding expiry.
    let expires_at: OffsetDateTime = sqlx::query_scalar(
        "INSERT INTO sessions (id, user_id, token_sha256, user_agent, expires_at, created_by_app,
                               max_expires_at)
         VALUES ($1, $2, $3, $4,
                 LEAST(now() + make_interval(days => $5), now() + make_interval(hours => $7)),
                 $6, now() + make_interval(hours => $7))
         RETURNING expires_at",
    )
    .bind(ids::uuid_v7())
    .bind(user_id)
    .bind(&digest[..])
    .bind(user_agent)
    .bind(SESSION_TTL_DAYS)
    .bind(created_by_app)
    .bind(max_hours)
    .fetch_one(db)
    .await?;
    Ok((token, expires_at))
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
    create_access_token_for(db, user_id, name, scope, expires_at, None).await
}

/// [`create_access_token`], tagged with the app whose minted session created it
/// (`access_tokens.created_by_app`), so revoking or rotating that app revokes it too.
pub async fn create_access_token_for<'e, E>(
    db: E,
    user_id: Uuid,
    name: &str,
    scope: Scope,
    expires_at: Option<OffsetDateTime>,
    created_by_app: Option<Uuid>,
) -> ApiResult<(Uuid, String)>
where
    E: sqlx::PgExecutor<'e>,
{
    let name = super::clean_name(Some(name), MAX_DISPLAY_NAME_CHARS)
        .ok_or_else(|| ApiError::bad_request("token name must not be empty"))?;
    let id = ids::uuid_v7();
    let (token, digest) = mint(CredentialKind::Token);
    sqlx::query(
        "INSERT INTO access_tokens (id, user_id, name, token_sha256, scope, expires_at,
                                    created_by_app)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(user_id)
    .bind(name)
    .bind(&digest[..])
    .bind(scope.as_str())
    .bind(expires_at)
    .bind(created_by_app)
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
    /// May mint user sessions (`POST /v1/apps/self/users/{subject}/sessions`; that also needs
    /// `trust_email`).
    pub allow_sessions: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// Number of users linked to the app.
    pub users: i64,
    /// Live sessions the app minted.
    pub live_sessions: i64,
    /// Live access tokens created with those sessions.
    pub live_tokens: i64,
}

impl AppRow {
    /// Whether the app may sign its users in right now: live, `allow_sessions` and
    /// `trust_email`.
    pub fn can_mint_sessions(&self) -> bool {
        self.revoked_at.is_none() && self.allow_sessions && self.trust_email
    }
}

/// A linked app's policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AppFlags {
    /// May create canvases with `POST /v1/apps/self/import`.
    pub allow_import: bool,
    /// Provisioning links an existing user with the same email (else `409 email_taken`).
    pub trust_email: bool,
    /// May sign its users in (`POST /v1/apps/self/users/{subject}/sessions`; also needs
    /// `trust_email`).
    pub allow_sessions: bool,
}

/// What revoking or rotating an app ended: the sessions it minted (deleted) and the live
/// access tokens created with them (revoked).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AppCredentialsEnded {
    pub sessions: u64,
    pub tokens: u64,
}

/// Register a linked app that may not mint sessions; returns `(id, "cva_…")` (the key is
/// shown once).
pub async fn create_app(
    db: &sqlx::PgPool,
    name: &str,
    allow_import: bool,
    trust_email: bool,
) -> ApiResult<(Uuid, String)> {
    create_app_with(
        db,
        name,
        AppFlags {
            allow_import,
            trust_email,
            allow_sessions: false,
        },
    )
    .await
}

/// Register a linked app with `flags`; returns `(id, "cva_…")` (the key is shown once).
pub async fn create_app_with(
    db: &sqlx::PgPool,
    name: &str,
    flags: AppFlags,
) -> ApiResult<(Uuid, String)> {
    let name = super::clean_name(Some(name), MAX_DISPLAY_NAME_CHARS)
        .ok_or_else(|| ApiError::bad_request("app name must not be empty"))?;
    let id = ids::uuid_v7();
    let (key, digest) = mint(CredentialKind::App);
    sqlx::query(
        "INSERT INTO apps (id, name, key_sha256, allow_import, trust_email, allow_sessions)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(name)
    .bind(&digest[..])
    .bind(flags.allow_import)
    .bind(flags.trust_email)
    .bind(flags.allow_sessions)
    .execute(db)
    .await?;
    Ok((id, key))
}

const APP_ROW_SQL: &str = "
SELECT a.id, a.name, a.allow_import, a.trust_email, a.allow_sessions, a.created_at,
       a.last_used_at, a.revoked_at,
       (SELECT count(*) FROM app_users au WHERE au.app_id = a.id) AS users,
       (SELECT count(*) FROM sessions s
        WHERE s.created_by_app = a.id AND s.expires_at > now()) AS live_sessions,
       (SELECT count(*) FROM access_tokens t
        WHERE t.created_by_app = a.id AND t.revoked_at IS NULL
          AND (t.expires_at IS NULL OR t.expires_at > now())) AS live_tokens
FROM apps a";

/// Every app, newest first.
pub async fn list_apps(db: &sqlx::PgPool) -> ApiResult<Vec<AppRow>> {
    Ok(sqlx::query_as(&format!(
        "{APP_ROW_SQL} ORDER BY a.created_at DESC, a.id DESC"
    ))
    .fetch_all(db)
    .await?)
}

/// One app, `None` if there is no such app.
pub async fn get_app(db: &sqlx::PgPool, id: Uuid) -> ApiResult<Option<AppRow>> {
    Ok(sqlx::query_as(&format!("{APP_ROW_SQL} WHERE a.id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await?)
}

/// End everything app `app_id` minted: delete its sessions, then revoke the access tokens
/// created with them. Run it in the transaction that revokes or rotates the app (holding the
/// app row): a mint or token creation racing it either commits first and is ended here, or
/// waits and then sees the app (or its session) gone.
pub async fn end_app_credentials(
    conn: &mut sqlx::PgConnection,
    app_id: Uuid,
) -> ApiResult<AppCredentialsEnded> {
    let sessions = sqlx::query("DELETE FROM sessions WHERE created_by_app = $1")
        .bind(app_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    let tokens = sqlx::query(
        "UPDATE access_tokens SET revoked_at = now()
         WHERE created_by_app = $1 AND revoked_at IS NULL",
    )
    .bind(app_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(AppCredentialsEnded { sessions, tokens })
}

/// Revoke an app's key and end the sessions it minted and the access tokens created with them
/// ([`end_app_credentials`]); its users and their canvases stay. Revoking an app that is
/// already revoked ends what it minted again (e.g. sessions minted before 0.2.1 tagged them).
/// Returns `(was_live, ended)`, `None` if there is no such app.
pub async fn revoke_app(
    db: &sqlx::PgPool,
    id: Uuid,
) -> ApiResult<Option<(bool, AppCredentialsEnded)>> {
    let mut tx = db.begin().await?;
    let live: Option<bool> =
        sqlx::query_scalar("SELECT revoked_at IS NULL FROM apps WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(live) = live else {
        return Ok(None);
    };
    if live {
        sqlx::query("UPDATE apps SET revoked_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    let ended = end_app_credentials(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Some((live, ended)))
}

/// Replace an app's key (the old one stops working immediately) and un-revoke it; returns the
/// new `cva_…`. With `end_sessions` (the default for `apps rotate`) also ends what the app
/// minted ([`end_app_credentials`]) — a leaked key may have minted sessions. `None` if no such
/// app.
pub async fn rotate_app_key(
    db: &sqlx::PgPool,
    id: Uuid,
    end_sessions: bool,
) -> ApiResult<Option<(String, AppCredentialsEnded)>> {
    let (key, digest) = mint(CredentialKind::App);
    let mut tx = db.begin().await?;
    let n = sqlx::query("UPDATE apps SET key_sha256 = $2, revoked_at = NULL WHERE id = $1")
        .bind(id)
        .bind(&digest[..])
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        return Ok(None);
    }
    let ended = if end_sessions {
        end_app_credentials(&mut tx, id).await?
    } else {
        AppCredentialsEnded::default()
    };
    tx.commit().await?;
    Ok(Some((key, ended)))
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
    /// No password account exists yet (users provisioned by apps or OIDC do not count): the
    /// first one may always be created, so a fresh server can be claimed.
    pub first_user: bool,
}

impl SignupPolicy {
    /// Load the policy (any executor, so a signup can re-check it inside its transaction).
    pub async fn load<'e, E>(db: E) -> ApiResult<Self>
    where
        E: sqlx::PgExecutor<'e>,
    {
        let (has_password_users, allow, domains): (bool, Option<Value>, Option<Value>) =
            sqlx::query_as(
                "SELECT EXISTS (SELECT 1 FROM users WHERE password_hash IS NOT NULL),
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
            first_user: !has_password_users,
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
