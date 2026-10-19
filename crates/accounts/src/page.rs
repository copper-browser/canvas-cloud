//! `GET /account` — one self-contained page for the humans of a standalone instance: sign up /
//! sign in (password and the configured OIDC providers), their canvases, personal access
//! tokens, an MCP config snippet, display name / password, sign out. Every action is a call to
//! the public REST API with the session token kept in `sessionStorage`.
//!
//! No build step and no external requests: the HTML, script and stylesheet are embedded and
//! served from `/account`, `/account/app.js` and `/account/app.css` under a strict
//! [`CONTENT_SECURITY_POLICY`] (no inline script or style).

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use canvas_cloud_core::state::SharedState;

/// The page's `Content-Security-Policy`: same-origin script, style and API calls only.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self'; img-src 'self' data:; connect-src 'self'; form-action 'self'; \
     frame-ancestors 'none'; base-uri 'none'";

const HTML: &str = include_str!("../assets/account.html");
const JS: &str = include_str!("../assets/account.js");
const CSS: &str = include_str!("../assets/account.css");

fn asset(body: &'static str, content_type: &'static str, cache: &'static str) -> Response {
    let mut resp = body.into_response();
    let h = resp.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    h.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    resp
}

async fn page() -> Response {
    asset(HTML, "text/html; charset=utf-8", "no-cache")
}

async fn script() -> Response {
    asset(JS, "text/javascript; charset=utf-8", "no-cache")
}

async fn style() -> Response {
    asset(CSS, "text/css; charset=utf-8", "no-cache")
}

pub(crate) fn router() -> Router<SharedState> {
    Router::new()
        .route("/account", get(page))
        .route("/account/", get(page))
        .route("/account/app.js", get(script))
        .route("/account/app.css", get(style))
}
