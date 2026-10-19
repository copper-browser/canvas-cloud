//! Generic OIDC sign-in (authorization code + PKCE + nonce) for the `[[oidc]]` providers in
//! the config:
//!
//! * `GET /v1/auth/oidc` — the providers, for sign-in buttons;
//! * `GET /v1/auth/oidc/{id}/start?redirect_uri=…` — checks the client's `redirect_uri`
//!   (in the provider's `redirect_uris`, or same-origin with `public_url`), stores
//!   state + nonce + PKCE verifier (table `oidc_states`, 10 minutes, single use), binds the
//!   flow to the browser with the [`STATE_COOKIE`] and redirects to the identity provider;
//! * `GET /v1/auth/oidc/{id}/callback` — verifies the round trip, exchanges the code, verifies
//!   the `id_token`, finds (or, when allowed, creates) the user by verified email, starts a
//!   session and redirects to `redirect_uri#token=cvs_…` (errors: `#error=…&error_description=…`).

mod client;

use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use canvas_cloud_core::config::{Config, OidcProvider};
use canvas_cloud_core::error::ApiError;
use canvas_cloud_core::extract::QueryParams;
use canvas_cloud_core::identity::{self, NewUser, SignupPolicy, MAX_DISPLAY_NAME_CHARS};
use canvas_cloud_core::ids;
use canvas_cloud_core::state::SharedState;
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

pub(crate) use client::Cache;
use client::Fail;

use crate::auth::{account_email, user_agent};
use crate::error::{Error, Result};
use crate::SharedAccounts;

/// How long a started sign-in stays valid.
pub const STATE_TTL_SECS: i64 = 600;
/// Cookie binding a started sign-in to the browser that started it (its value is the
/// `state`), so a callback URL cannot be replayed in someone else's browser.
pub const STATE_COOKIE: &str = "cvc_oidc";
/// Scopes requested from the provider.
const SCOPES: &str = "openid email profile";
/// Longest accepted client `redirect_uri`.
const MAX_REDIRECT_URI_BYTES: usize = 2048;

fn provider<'a>(cfg: &'a Config, id: &str) -> Result<&'a OidcProvider> {
    cfg.oidc.iter().find(|p| p.id == id).ok_or_else(|| {
        ApiError::NotFoundCode {
            code: "unknown_provider",
            message: "no such sign-in provider",
        }
        .into()
    })
}

/// `{public_url}/v1/auth/oidc/{id}/callback` — the redirect URI to register with the identity provider.
pub(crate) fn callback_url(cfg: &Config, id: &str) -> String {
    format!("{}/v1/auth/oidc/{id}/callback", cfg.public_url)
}

/// A client `redirect_uri` is acceptable when it is listed verbatim in the provider's
/// `redirect_uris`, or is an `http(s)` URL with the same origin as `public_url` (a web app
/// served by this server). Never with a fragment (the result goes there) or credentials.
pub(crate) fn redirect_allowed(public_url: &str, p: &OidcProvider, uri: &str) -> bool {
    if uri.is_empty() || uri.len() > MAX_REDIRECT_URI_BYTES {
        return false;
    }
    let Ok(u) = Url::parse(uri) else {
        return false;
    };
    if u.fragment().is_some() || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    if p.redirect_uris.iter().any(|r| r == uri) {
        return true;
    }
    let Ok(public) = Url::parse(public_url) else {
        return false;
    };
    matches!(u.scheme(), "http" | "https") && u.origin() == public.origin()
}

/// `GET /v1/auth/oidc` → `{providers: [{id, name, start}]}`.
pub(crate) async fn list(State(state): State<SharedState>) -> Json<Value> {
    let providers: Vec<Value> = state
        .cfg
        .oidc
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.label(),
                "start": format!("/v1/auth/oidc/{}/start", p.id),
            })
        })
        .collect();
    Json(json!({ "providers": providers }))
}

#[derive(Deserialize)]
pub(crate) struct StartQuery {
    redirect_uri: Option<String>,
}

fn cookie(cfg: &Config, value: &str, max_age: i64) -> HeaderValue {
    let secure = if cfg.public_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{STATE_COOKIE}={value}; Max-Age={max_age}; Path=/v1/auth/oidc/; HttpOnly; SameSite=Lax{secure}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("cvc_oidc=; Max-Age=0"))
}

fn redirect(location: &str, set_cookie: HeaderValue) -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(location) {
        h.insert(LOCATION, v);
    }
    h.insert(SET_COOKIE, set_cookie);
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    resp
}

/// `GET /v1/auth/oidc/{id}/start?redirect_uri=…` → `302` to the provider.
pub(crate) async fn start(
    State(state): State<SharedState>,
    Extension(shared): Extension<SharedAccounts>,
    Path(id): Path<String>,
    QueryParams(q): QueryParams<StartQuery>,
) -> Result<Response> {
    let p = provider(&state.cfg, &id)?;
    let redirect_uri = q.redirect_uri.unwrap_or_default();
    if !redirect_allowed(&state.cfg.public_url, p, &redirect_uri) {
        return Err(Error::coded(
            StatusCode::BAD_REQUEST,
            "redirect_uri_not_allowed",
            "redirect_uri must be one of the provider's redirect_uris or on this server's origin",
        ));
    }
    let d = shared.oidc.discovery(p).await.map_err(|f| {
        tracing::warn!(provider = %p.id, error = %f.message, "OIDC discovery failed");
        Error::coded(
            StatusCode::BAD_GATEWAY,
            "oidc_unavailable",
            "the sign-in provider is unreachable or misconfigured",
        )
    })?;
    let state_token = ids::random_token();
    let nonce = ids::random_token();
    let verifier = ids::random_token();
    let challenge = ids::b64url(&ids::sha256(verifier.as_bytes()));
    // Rows outlive their expiry by a day so a late return still says "took too long"
    // (`state_expired`) rather than "invalid"; it also keeps concurrent starts from
    // sweeping a sign-in that is being answered right now.
    sqlx::query("DELETE FROM oidc_states WHERE expires_at < now() - interval '1 day'")
        .execute(&state.db)
        .await?;
    sqlx::query(
        "INSERT INTO oidc_states (state_sha256, provider, nonce, code_verifier, redirect_uri,
                                  expires_at)
         VALUES ($1, $2, $3, $4, $5, now() + $6::bigint * interval '1 second')",
    )
    .bind(&ids::sha256(state_token.as_bytes())[..])
    .bind(&p.id)
    .bind(&nonce)
    .bind(&verifier)
    .bind(&redirect_uri)
    .bind(STATE_TTL_SECS)
    .execute(&state.db)
    .await?;
    let mut auth = Url::parse(&d.authorization_endpoint)
        .map_err(|e| ApiError::internal(format!("authorization_endpoint: {e}")))?;
    auth.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &p.client_id)
        .append_pair("redirect_uri", &callback_url(&state.cfg, &p.id))
        .append_pair("scope", SCOPES)
        .append_pair("state", &state_token)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(redirect(
        auth.as_str(),
        cookie(&state.cfg, &state_token, STATE_TTL_SECS),
    ))
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

fn state_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == STATE_COOKIE).then_some(v)
        })
}

/// A small self-contained HTML error page for a callback that has nowhere to go back to.
fn error_page(cfg: &Config, message: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Sign-in failed</title></head><body style=\"font:16px/1.5 system-ui,sans-serif;\
         max-width:32rem;margin:15vh auto;padding:0 1rem\"><h1 style=\"font-size:1.25rem\">\
         Sign-in failed</h1><p>{message}</p><p><a href=\"/account\">Back to sign in</a></p>\
         </body></html>"
    );
    let mut resp = (StatusCode::BAD_REQUEST, html).into_response();
    let h = resp.headers_mut();
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(SET_COOKIE, cookie(cfg, "", 0));
    resp
}

/// `redirect_uri` with `fragment` (replacing any).
fn with_fragment(redirect_uri: &str, fragment: &str) -> String {
    match Url::parse(redirect_uri) {
        Ok(mut u) => {
            u.set_fragment(Some(fragment));
            u.into()
        }
        Err(_) => format!("{redirect_uri}#{fragment}"),
    }
}

fn error_fragment(code: &str, message: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("error", code)
        .append_pair("error_description", message)
        .finish()
}

/// `GET /v1/auth/oidc/{id}/callback?code=…&state=…` → `302` to the client's `redirect_uri`
/// with `#token=cvs_…` or `#error=…&error_description=…`.
pub(crate) async fn callback(
    State(state): State<SharedState>,
    Extension(shared): Extension<SharedAccounts>,
    Path(id): Path<String>,
    headers: HeaderMap,
    QueryParams(q): QueryParams<CallbackQuery>,
) -> Result<Response> {
    let p = provider(&state.cfg, &id)?;
    let Some(state_token) = q.state.filter(|s| ids::is_token_shape(s)) else {
        return Ok(error_page(
            &state.cfg,
            "This sign-in link is incomplete. Go back and sign in again.",
        ));
    };
    let row: Option<(String, String, String, bool)> = sqlx::query_as(
        "DELETE FROM oidc_states WHERE state_sha256 = $1 AND provider = $2
         RETURNING nonce, code_verifier, redirect_uri, expires_at > now()",
    )
    .bind(&ids::sha256(state_token.as_bytes())[..])
    .bind(&p.id)
    .fetch_optional(&state.db)
    .await?;
    let Some((nonce, verifier, redirect_uri, live)) = row else {
        return Ok(error_page(
            &state.cfg,
            "This sign-in link is invalid or was already used. Go back and sign in again.",
        ));
    };
    let clear = cookie(&state.cfg, "", 0);
    let fail = |f: Fail| {
        tracing::info!(provider = %p.id, code = f.code, reason = %f.message, "OIDC sign-in failed");
        redirect(
            &with_fragment(&redirect_uri, &error_fragment(f.code, &f.message)),
            clear.clone(),
        )
    };
    if !live {
        return Ok(fail(Fail::new(
            "state_expired",
            "the sign-in took too long; please try again",
        )));
    }
    if state_cookie(&headers) != Some(state_token.as_str()) {
        return Ok(fail(Fail::new(
            "state_mismatch",
            "this sign-in was started in another browser; please try again",
        )));
    }
    if let Some(err) = q.error {
        let err: String = err
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        return Ok(fail(Fail::new(
            "access_denied",
            format!("the sign-in provider did not sign you in ({err})"),
        )));
    }
    let Some(code) = q.code.filter(|c| !c.is_empty() && c.len() <= 4096) else {
        return Ok(fail(Fail::new(
            "invalid_request",
            "the provider sent no authorization code",
        )));
    };
    match complete(
        &state,
        &shared,
        p,
        &code,
        &nonce,
        &verifier,
        user_agent(&headers),
    )
    .await
    {
        Ok((user_id, token)) => {
            tracing::Span::current().record("user_id", tracing::field::display(user_id));
            tracing::info!(provider = %p.id, user_id = %user_id, "OIDC sign-in");
            Ok(redirect(
                &with_fragment(&redirect_uri, &format!("token={token}")),
                clear,
            ))
        }
        Err(f) => Ok(fail(f)),
    }
}

fn email_domain(email: &str) -> String {
    email
        .rsplit_once('@')
        .map(|(_, d)| d.to_ascii_lowercase())
        .unwrap_or_default()
}

fn normalize_domains(list: &[String]) -> Vec<String> {
    list.iter()
        .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect()
}

/// Whether a verified `email` without an account may get one through this provider: the
/// provider's `allowed_domains` names its domain, or self-signup is on (`allow_signup`) and
/// `signup_domains` (if any) admits it. The "first password account" rule does not apply.
pub(crate) fn may_create(p: &OidcProvider, policy: &SignupPolicy, email: &str) -> bool {
    let domain = email_domain(email);
    let provider_domains = normalize_domains(&p.allowed_domains);
    if !provider_domains.is_empty() && provider_domains.contains(&domain) {
        return true;
    }
    policy.allow_signup
        && (policy.signup_domains.is_empty() || policy.signup_domains.contains(&domain))
}

async fn read_secret(p: &OidcProvider) -> std::result::Result<Option<String>, Fail> {
    let Some(path) = &p.client_secret_file else {
        return Ok(None);
    };
    let raw = tokio::fs::read_to_string(path).await.map_err(|e| {
        Fail::new(
            "provider_unavailable",
            format!("reading client_secret_file {}: {e}", path.display()),
        )
    })?;
    Ok(Some(raw.trim().to_owned()).filter(|s| !s.is_empty()))
}

/// Code → verified identity → user → session `(user_id, "cvs_…")`.
async fn complete(
    state: &SharedState,
    shared: &SharedAccounts,
    p: &OidcProvider,
    code: &str,
    nonce: &str,
    verifier: &str,
    user_agent: Option<&str>,
) -> std::result::Result<(Uuid, String), Fail> {
    let internal = |e: ApiError| {
        tracing::error!(error = %e, "OIDC callback database error");
        Fail::new("server_error", "the server could not complete the sign-in")
    };
    let d = shared.oidc.discovery(p).await?;
    let secret = read_secret(p).await?;
    let id_token = shared
        .oidc
        .exchange(
            p,
            &d,
            code,
            &callback_url(&state.cfg, &p.id),
            verifier,
            secret.as_deref(),
        )
        .await?;
    let claims = shared.oidc.verify(p, &d, &id_token, nonce).await?;
    let email = claims
        .email
        .as_deref()
        .and_then(|e| account_email(e).ok())
        .ok_or_else(|| {
            Fail::new(
                "email_missing",
                "the provider did not share an email address",
            )
        })?;
    if !claims.email_verified() {
        return Err(Fail::new(
            "email_not_verified",
            "the provider has not verified this email address",
        ));
    }
    let provider_domains = normalize_domains(&p.allowed_domains);
    if !provider_domains.is_empty() && !provider_domains.contains(&email_domain(&email)) {
        return Err(Fail::new(
            "domain_not_allowed",
            "this email domain may not sign in with this provider",
        ));
    }
    let existing: Option<(Uuid, bool)> =
        sqlx::query_as("SELECT id, disabled FROM users WHERE lower(email) = lower($1)")
            .bind(&email)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| internal(e.into()))?;
    let user_id = match existing {
        Some((_, true)) => {
            return Err(Fail::new("account_disabled", "this account is disabled"));
        }
        Some((id, false)) => id,
        None => {
            let policy = SignupPolicy::load(&state.db).await.map_err(internal)?;
            if !may_create(p, &policy, &email) {
                return Err(Fail::new(
                    "signup_closed",
                    "there is no account for this email and sign-up is closed",
                ));
            }
            let name = identity::clean_name(
                claims.name.as_deref().or(claims.given_name.as_deref()),
                MAX_DISPLAY_NAME_CHARS,
            );
            match identity::insert_user(
                &state.db,
                &NewUser {
                    email: &email,
                    display_name: name.as_deref(),
                    ..NewUser::default()
                },
            )
            .await
            {
                Ok(id) => {
                    tracing::info!(provider = %p.id, user_id = %id, "OIDC signup");
                    id
                }
                // Created concurrently (e.g. a double-clicked callback): use it.
                Err(ApiError::Conflict(_)) => identity::user_id_by_email(&state.db, &email)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| internal(ApiError::internal("user vanished")))?,
                Err(e) => return Err(internal(e)),
            }
        }
    };
    let token = identity::create_session(&state.db, user_id, user_agent)
        .await
        .map_err(internal)?;
    Ok((user_id, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(redirects: &[&str], domains: &[&str]) -> OidcProvider {
        let toml = format!(
            "listen = \"127.0.0.1:0\"\ndatabase_url = \"postgres://x/y\"\nmaster_key = \"{}\"\n\
             [tls]\nmode = \"off\"\n[[oidc]]\nid = \"idp\"\nissuer = \"https://idp.example.com\"\n\
             client_id = \"c\"\nredirect_uris = {:?}\nallowed_domains = {:?}\n",
            ids::b64url(&[7u8; 32]),
            redirects,
            domains
        );
        Config::from_toml_str(&toml, &|_| None)
            .unwrap()
            .oidc
            .remove(0)
    }

    #[test]
    fn redirect_uris() {
        let p = provider(&["myapp://signed-in", "https://app.example.org/cb"], &[]);
        let public = "https://canvas.example.com";
        assert!(redirect_allowed(public, &p, "myapp://signed-in"));
        assert!(redirect_allowed(public, &p, "https://app.example.org/cb"));
        assert!(!redirect_allowed(public, &p, "https://app.example.org/cb/"));
        assert!(!redirect_allowed(
            public,
            &p,
            "https://app.example.org/other"
        ));
        assert!(redirect_allowed(public, &p, "https://canvas.example.com/"));
        assert!(redirect_allowed(
            public,
            &p,
            "https://canvas.example.com/account"
        ));
        assert!(!redirect_allowed(
            public,
            &p,
            "https://canvas.example.com/#x"
        ));
        assert!(!redirect_allowed(public, &p, "http://canvas.example.com/"));
        assert!(!redirect_allowed(
            public,
            &p,
            "https://canvas.example.com:8443/"
        ));
        assert!(!redirect_allowed(
            public,
            &p,
            "https://evil@canvas.example.com/"
        ));
        assert!(!redirect_allowed(
            public,
            &p,
            "https://canvas.example.com.evil.org/"
        ));
        assert!(!redirect_allowed(public, &p, "javascript:alert(1)"));
        assert!(!redirect_allowed(public, &p, ""));
    }

    #[test]
    fn creation_rules() {
        let open = SignupPolicy {
            allow_signup: true,
            ..SignupPolicy::default()
        };
        let closed_first = SignupPolicy {
            first_user: true,
            ..SignupPolicy::default()
        };
        let any = provider(&[], &[]);
        assert!(may_create(&any, &open, "a@x.org"));
        assert!(
            !may_create(&any, &closed_first, "a@x.org"),
            "no first-user rule"
        );
        let corp = provider(&[], &["@Corp.example"]);
        assert!(may_create(
            &corp,
            &SignupPolicy::default(),
            "a@corp.example"
        ));
        assert!(!may_create(&corp, &SignupPolicy::default(), "a@x.org"));
        let narrowed = SignupPolicy {
            allow_signup: true,
            signup_domains: vec!["ok.example".into()],
            first_user: false,
        };
        assert!(may_create(&any, &narrowed, "a@ok.example"));
        assert!(!may_create(&any, &narrowed, "a@no.example"));
    }

    #[test]
    fn fragments() {
        assert_eq!(
            with_fragment("https://a.example/x?y=1", "token=cvs_1"),
            "https://a.example/x?y=1#token=cvs_1"
        );
        assert_eq!(
            error_fragment("domain_not_allowed", "no & no"),
            "error=domain_not_allowed&error_description=no+%26+no"
        );
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(COOKIE, HeaderValue::from_static("a=1; cvc_oidc=abc; b=2"));
        assert_eq!(state_cookie(&h), Some("abc"));
        h.insert(COOKIE, HeaderValue::from_static("xcvc_oidc=abc"));
        assert_eq!(state_cookie(&h), None);
    }
}
