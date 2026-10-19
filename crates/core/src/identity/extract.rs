//! The [`AuthUser`], [`AuthUserAnyScope`] and [`AppAuth`] request extractors.

use axum::extract::FromRequestParts;
use axum::http::header::{ACCEPT, AUTHORIZATION, UPGRADE};
use axum::http::request::Parts;
use axum::http::Method;
use uuid::Uuid;

use super::credentials::{credential_digest, parse_credential, CredentialKind};
use super::{AuthUser, Scope, Via, SESSION_TTL_DAYS, TOUCH_INTERVAL_SECS};
use crate::error::{ApiError, ApiResult};
use crate::state::SharedState;

/// Header an app sends with its `cva_` key to act as one of its users (the app's subject).
pub const APP_USER_HEADER: &str = "x-canvas-user";

/// [`AuthUser`] without the read-only-scope method check: a [`Scope::Read`] token may call
/// any method. For routes that enforce scope per operation themselves (e.g. MCP, where a
/// read-only token may still call the read tools over `POST`).
#[derive(Clone, Debug)]
pub struct AuthUserAnyScope(pub AuthUser);

/// A linked app acting as itself (`Authorization: Bearer cva_…` **without**
/// [`APP_USER_HEADER`]) — the `/v1/apps/self/*` routes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppAuth {
    pub app_id: Uuid,
    pub name: String,
    pub allow_import: bool,
    pub trust_email: bool,
}

#[allow(clippy::cast_lossless)]
const TOUCH_SECS: f64 = TOUCH_INTERVAL_SECS as f64;

fn unauthorized() -> ApiError {
    ApiError::Unauthorized("unauthorized")
}

fn disabled() -> ApiError {
    ApiError::UnauthorizedWith {
        code: "account_disabled",
        message: "this account is disabled",
    }
}

fn is_ws_upgrade(parts: &Parts) -> bool {
    parts
        .headers
        .get(UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"))
}

fn wants_event_stream(parts: &Parts) -> bool {
    parts.method == Method::GET
        && parts
            .headers
            .get_all(ACCEPT)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.to_ascii_lowercase().contains("text/event-stream"))
}

/// The credential: `Authorization: Bearer …`, else `?token=` — but only on a WebSocket
/// upgrade or a `text/event-stream` GET (browsers cannot set headers there). `Err` when an
/// `Authorization` header is present but is not a bearer credential.
fn credential(parts: &Parts) -> ApiResult<Option<&str>> {
    if let Some(value) = parts.headers.get(AUTHORIZATION) {
        let value = value.to_str().map_err(|_| unauthorized())?;
        let (scheme, token) = value.split_once(' ').ok_or_else(unauthorized)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(unauthorized());
        }
        return Ok(Some(token.trim()));
    }
    if !(is_ws_upgrade(parts) || wants_event_stream(parts)) {
        return Ok(None);
    }
    Ok(parts
        .uri
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token="))))
}

fn app_subject(parts: &Parts) -> ApiResult<Option<String>> {
    let Some(v) = parts.headers.get(APP_USER_HEADER) else {
        return Ok(None);
    };
    let s = v
        .to_str()
        .map_err(|_| ApiError::bad_request("X-Canvas-User must be printable ASCII"))?
        .trim();
    let len = s.chars().count();
    if len == 0 || len > super::MAX_SUBJECT_CHARS {
        return Err(ApiError::bad_request(
            "X-Canvas-User must be 1-200 characters",
        ));
    }
    Ok(Some(s.to_owned()))
}

const SESSION_SQL: &str = "
WITH s AS (
    SELECT s.id, u.id AS user_id, u.email, u.display_name, u.disabled
    FROM sessions s
    JOIN users u ON u.id = s.user_id
    WHERE s.token_sha256 = $1 AND s.expires_at > now()
), touch AS (
    UPDATE sessions
    SET last_used_at = now(), expires_at = now() + make_interval(days => $2)
    WHERE id IN (SELECT id FROM s WHERE NOT disabled)
      AND last_used_at < now() - make_interval(secs => $3)
)
SELECT id, user_id, email, display_name, disabled FROM s";

const TOKEN_SQL: &str = "
WITH t AS (
    SELECT t.id, t.scope, u.id AS user_id, u.email, u.display_name, u.disabled
    FROM access_tokens t
    JOIN users u ON u.id = t.user_id
    WHERE t.token_sha256 = $1 AND t.revoked_at IS NULL
      AND (t.expires_at IS NULL OR t.expires_at > now())
), touch AS (
    UPDATE access_tokens SET last_used_at = now()
    WHERE id IN (SELECT id FROM t WHERE NOT disabled)
      AND (last_used_at IS NULL OR last_used_at < now() - make_interval(secs => $2))
)
SELECT id, scope, user_id, email, display_name, disabled FROM t";

const APP_USER_SQL: &str = "
WITH a AS (
    SELECT id FROM apps WHERE key_sha256 = $1 AND revoked_at IS NULL
), touch AS (
    UPDATE apps SET last_used_at = now()
    WHERE id IN (SELECT id FROM a)
      AND (last_used_at IS NULL OR last_used_at < now() - make_interval(secs => $3))
)
SELECT a.id, u.id, u.email, u.display_name, u.disabled
FROM a
LEFT JOIN app_users au ON au.app_id = a.id AND au.subject = $2
LEFT JOIN users u ON u.id = au.user_id";

const APP_SQL: &str = "
WITH a AS (
    SELECT id, name, allow_import, trust_email
    FROM apps WHERE key_sha256 = $1 AND revoked_at IS NULL
), touch AS (
    UPDATE apps SET last_used_at = now()
    WHERE id IN (SELECT id FROM a)
      AND (last_used_at IS NULL OR last_used_at < now() - make_interval(secs => $2))
)
SELECT id, name, allow_import, trust_email FROM a";

type UserRow = (Uuid, String, String, bool);
/// `(app_id, user_id?, email?, display_name?, disabled?)` — the user half is NULL when the
/// subject is not provisioned.
type AppUserRow = (
    Uuid,
    Option<Uuid>,
    Option<String>,
    Option<String>,
    Option<bool>,
);

fn display_name(raw: String) -> Option<String> {
    (!raw.is_empty()).then_some(raw)
}

/// Resolve the request's credential to a user (no scope check). Cached per request.
async fn authenticate(parts: &mut Parts, state: &SharedState) -> ApiResult<AuthUser> {
    if let Some(user) = parts.extensions.get::<AuthUser>() {
        return Ok(user.clone());
    }
    let token = credential(parts)?.ok_or_else(unauthorized)?;
    let kind = parse_credential(token).ok_or_else(unauthorized)?;
    let digest = credential_digest(token);
    let user = match kind {
        CredentialKind::Session => {
            let row: Option<(Uuid, Uuid, String, String, bool)> = sqlx::query_as(SESSION_SQL)
                .bind(&digest[..])
                .bind(SESSION_TTL_DAYS)
                .bind(TOUCH_SECS)
                .fetch_optional(&state.db)
                .await?;
            let (session_id, user_id, email, name, is_disabled) = row.ok_or_else(unauthorized)?;
            if is_disabled {
                return Err(disabled());
            }
            AuthUser {
                user_id,
                email,
                display_name: display_name(name),
                via: Via::Session { session_id },
            }
        }
        CredentialKind::Token => {
            let row: Option<(Uuid, String, Uuid, String, String, bool)> = sqlx::query_as(TOKEN_SQL)
                .bind(&digest[..])
                .bind(TOUCH_SECS)
                .fetch_optional(&state.db)
                .await?;
            let (token_id, scope, user_id, email, name, is_disabled) =
                row.ok_or_else(unauthorized)?;
            if is_disabled {
                return Err(disabled());
            }
            let scope = Scope::parse(&scope)
                .ok_or_else(|| ApiError::internal(format!("access token scope {scope:?}")))?;
            AuthUser {
                user_id,
                email,
                display_name: display_name(name),
                via: Via::Token { token_id, scope },
            }
        }
        CredentialKind::App => {
            let Some(subject) = app_subject(parts)? else {
                return Err(ApiError::UnauthorizedWith {
                    code: "unauthorized",
                    message: "an app key acts for a user only with X-Canvas-User",
                });
            };
            let row: Option<AppUserRow> = sqlx::query_as(APP_USER_SQL)
                .bind(&digest[..])
                .bind(&subject)
                .bind(TOUCH_SECS)
                .fetch_optional(&state.db)
                .await?;
            let (app_id, user) = match row.ok_or_else(unauthorized)? {
                (app_id, Some(user_id), Some(email), Some(name), Some(is_disabled)) => {
                    (app_id, Some((user_id, email, name, is_disabled) as UserRow))
                }
                (app_id, ..) => (app_id, None),
            };
            let (user_id, email, name, is_disabled) = user.ok_or(ApiError::UnauthorizedWith {
                code: "unknown_user",
                message:
                    "this app has not provisioned that user (PUT /v1/apps/self/users/{subject})",
            })?;
            if is_disabled {
                return Err(disabled());
            }
            AuthUser {
                user_id,
                email,
                display_name: display_name(name),
                via: Via::App { app_id, subject },
            }
        }
    };
    tracing::Span::current().record("user_id", tracing::field::display(user.user_id));
    parts.extensions.insert(user.clone());
    Ok(user)
}

/// The read-only rule: a [`Scope::Read`] token may only GET/HEAD, and never upgrade to a
/// WebSocket (it cannot join canvas rooms).
fn check_scope(parts: &Parts, user: &AuthUser) -> ApiResult<()> {
    if user.is_read_only()
        && (!(parts.method == Method::GET || parts.method == Method::HEAD) || is_ws_upgrade(parts))
    {
        return Err(ApiError::Denied {
            code: "read_only_token",
            message: "this access token is read-only",
        });
    }
    Ok(())
}

impl FromRequestParts<SharedState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let user = authenticate(parts, state).await?;
        check_scope(parts, &user)?;
        Ok(user)
    }
}

impl FromRequestParts<SharedState> for AuthUserAnyScope {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(authenticate(parts, state).await?))
    }
}

impl FromRequestParts<SharedState> for AppAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(app) = parts.extensions.get::<AppAuth>() {
            return Ok(app.clone());
        }
        let token = credential(parts)?.ok_or_else(unauthorized)?;
        if parse_credential(token) != Some(CredentialKind::App) {
            return Err(unauthorized());
        }
        if parts.headers.contains_key(APP_USER_HEADER) {
            return Err(ApiError::bad_request(
                "X-Canvas-User is not accepted on /v1/apps/self routes",
            ));
        }
        let row: Option<(Uuid, String, bool, bool)> = sqlx::query_as(APP_SQL)
            .bind(&credential_digest(token)[..])
            .bind(TOUCH_SECS)
            .fetch_optional(&state.db)
            .await?;
        let (app_id, name, allow_import, trust_email) = row.ok_or_else(unauthorized)?;
        tracing::Span::current().record("app_id", tracing::field::display(app_id));
        let app = Self {
            app_id,
            name,
            allow_import,
            trust_email,
        };
        parts.extensions.insert(app.clone());
        Ok(app)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn parts(req: Request<()>) -> Parts {
        req.into_parts().0
    }

    #[test]
    fn query_token_only_for_ws_and_sse() {
        let p = parts(Request::get("/v1/canvases?token=cvs_x").body(()).unwrap());
        assert_eq!(credential(&p).unwrap(), None);
        let p = parts(
            Request::get("/v1/canvases/x/ws?a=1&token=cvs_x")
                .header("upgrade", "WebSocket")
                .body(())
                .unwrap(),
        );
        assert_eq!(credential(&p).unwrap(), Some("cvs_x"));
        let p = parts(
            Request::get("/v1/events?token=cvt_y")
                .header("accept", "text/event-stream")
                .body(())
                .unwrap(),
        );
        assert_eq!(credential(&p).unwrap(), Some("cvt_y"));
        let p = parts(
            Request::post("/v1/events?token=cvt_y")
                .header("accept", "text/event-stream")
                .body(())
                .unwrap(),
        );
        assert_eq!(credential(&p).unwrap(), None, "SSE means GET");
    }

    #[test]
    fn authorization_header_wins_and_must_be_bearer() {
        let p = parts(
            Request::get("/x?token=cvs_q")
                .header("authorization", "bearer  cvs_h ")
                .header("upgrade", "websocket")
                .body(())
                .unwrap(),
        );
        assert_eq!(credential(&p).unwrap(), Some("cvs_h"));
        let p = parts(
            Request::get("/x")
                .header("authorization", "Basic abc")
                .body(())
                .unwrap(),
        );
        assert!(credential(&p).is_err());
    }

    #[test]
    fn read_only_scope_rule() {
        let user = AuthUser {
            user_id: Uuid::nil(),
            email: "a@b.co".into(),
            display_name: None,
            via: Via::Token {
                token_id: Uuid::nil(),
                scope: Scope::Read,
            },
        };
        let get = parts(Request::get("/x").body(()).unwrap());
        let head = parts(Request::head("/x").body(()).unwrap());
        assert!(check_scope(&get, &user).is_ok());
        assert!(check_scope(&head, &user).is_ok());
        for m in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            let p = parts(Request::builder().method(m).uri("/x").body(()).unwrap());
            assert_eq!(
                check_scope(&p, &user).unwrap_err().code(),
                "read_only_token"
            );
        }
        let ws = parts(
            Request::get("/x")
                .header("upgrade", "websocket")
                .body(())
                .unwrap(),
        );
        assert_eq!(
            check_scope(&ws, &user).unwrap_err().code(),
            "read_only_token"
        );
        let writer = AuthUser {
            via: Via::Token {
                token_id: Uuid::nil(),
                scope: Scope::Write,
            },
            ..user
        };
        let post = parts(Request::post("/x").body(()).unwrap());
        assert!(check_scope(&post, &writer).is_ok());
        assert!(check_scope(&ws, &writer).is_ok());
    }

    #[test]
    fn subject_header_is_bounded() {
        let ok = parts(
            Request::get("/x")
                .header(APP_USER_HEADER, " u-1 ")
                .body(())
                .unwrap(),
        );
        assert_eq!(app_subject(&ok).unwrap().as_deref(), Some("u-1"));
        let long = parts(
            Request::get("/x")
                .header(APP_USER_HEADER, "x".repeat(201))
                .body(())
                .unwrap(),
        );
        assert!(app_subject(&long).is_err());
        let none = parts(Request::get("/x").body(()).unwrap());
        assert_eq!(app_subject(&none).unwrap(), None);
    }
}
