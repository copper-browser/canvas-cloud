//! Identity: users, sessions, personal access tokens, linked apps, and the request
//! extractors every crate authenticates with (CONTRACT §4).
//!
//! Credentials are 32 random bytes, base64url without padding, behind a prefix that says
//! what they are; the database stores only SHA-256 of the whole credential string:
//!
//! | Prefix | What | How it is sent |
//! |---|---|---|
//! | `cvs_` | session token (login / signup / OIDC) | `Authorization: Bearer cvs_…`, or `?token=` on WebSocket upgrades and `text/event-stream` GETs |
//! | `cvt_` | personal access token ([`Scope::Read`] or [`Scope::Write`]) | same as a session |
//! | `cva_` | linked-app key | `Authorization: Bearer cva_…` plus [`APP_USER_HEADER`] to act as one of the app's users; without it only [`AppAuth`] routes (`/v1/apps/self/*`) accept it |
//!
//! [`AuthUser`] resolves exactly one of those per request, with one SQL round trip that is
//! cached in the request extensions; [`AppAuth`] authenticates an app acting as itself.

mod credentials;
mod extract;
mod passwords;
mod store;

pub use credentials::{
    credential_digest, mint, parse_credential, CredentialKind, APP_PREFIX, SESSION_PREFIX,
    TOKEN_PREFIX,
};
pub use extract::{AppAuth, AuthUserAnyScope, APP_USER_HEADER};
pub use passwords::{
    clean_name, hash_password, normalize_email, validate_password, verify_password,
    MAX_PASSWORD_BYTES, MIN_PASSWORD_CHARS,
};
pub use store::{
    audit, count_users, create_access_token, create_access_token_for, create_app,
    create_app_session, create_app_with, create_session, create_session_with_expiry,
    default_display_name, end_app_credentials, get_app, get_setting, insert_user, list_apps,
    revoke_app, revoke_session, rotate_app_key, set_setting, signup_allowed, user_email,
    user_id_by_email, AppCredentialsEnded, AppFlags, AppRow, NewUser, SignupPolicy,
    MAX_DISPLAY_NAME_CHARS, MAX_SUBJECT_CHARS, MAX_USER_AGENT_CHARS,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Sliding session lifetime: days of inactivity before a session dies.
pub const SESSION_TTL_DAYS: i32 = 30;
/// Sessions slide, and tokens/apps record `last_used_at`, at most this often.
pub const TOUCH_INTERVAL_SECS: i32 = 60;

/// What a personal access token may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// GET/HEAD only; cannot join canvas rooms.
    Read,
    /// Everything the owning user can do.
    Write,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    /// `"read"` / `"write"` (the `access_tokens.scope` column).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the caller authenticated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Via {
    /// A `cvs_` session.
    Session { session_id: Uuid },
    /// A `cvt_` personal access token.
    Token { token_id: Uuid, scope: Scope },
    /// A `cva_` app key acting as the app user `subject`.
    App { app_id: Uuid, subject: String },
}

/// The authenticated caller. Extract it in any handler on a `Router<SharedState>`:
///
/// * one credential from `Authorization: Bearer …` (or `?token=` on WebSocket upgrades and
///   `Accept: text/event-stream` GETs only), resolved with one SQL round trip and cached in
///   the request extensions, so several extractors in one handler cost one query;
/// * sessions slide their expiry, tokens and apps record `last_used_at`, each at most once a
///   minute ([`TOUCH_INTERVAL_SECS`]);
/// * a disabled user → `401 account_disabled`; an app subject that is not provisioned →
///   `401 unknown_user`; a [`Scope::Read`] token on anything but GET/HEAD, or on a WebSocket
///   upgrade → `403 read_only_token` (use [`AuthUserAnyScope`] to enforce scope yourself).
///
/// The user id is recorded on the request span (`user_id`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthUser {
    pub user_id: Uuid,
    pub email: String,
    /// `None` when the user has no display name (stored as `''`).
    pub display_name: Option<String>,
    pub via: Via,
}

impl AuthUser {
    /// `true` for a [`Scope::Read`] personal access token.
    pub fn is_read_only(&self) -> bool {
        matches!(
            self.via,
            Via::Token {
                scope: Scope::Read,
                ..
            }
        )
    }

    /// The session id when authenticated with a session.
    pub fn session_id(&self) -> Option<Uuid> {
        match self.via {
            Via::Session { session_id } => Some(session_id),
            _ => None,
        }
    }

    /// The app id when an app acts for this user.
    pub fn app_id(&self) -> Option<Uuid> {
        match self.via {
            Via::App { app_id, .. } => Some(app_id),
            _ => None,
        }
    }
}
