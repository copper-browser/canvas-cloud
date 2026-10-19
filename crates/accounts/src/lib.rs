//! Human accounts for a standalone Canvas Cloud (CONTRACT §5.2): password sign-up and sign-in,
//! sessions, personal access tokens, generic OIDC sign-in, the caller's event stream and the
//! `/account` page.
//!
//! | Method | Path (under `/v1`) | Auth |
//! |---|---|---|
//! | POST | `/auth/signup` | public (signup policy) |
//! | POST | `/auth/login` | public |
//! | POST | `/auth/logout` | session |
//! | GET, PATCH | `/auth/me` | any credential |
//! | POST | `/auth/password` | session |
//! | GET | `/auth/oidc` | public |
//! | GET | `/auth/oidc/{id}/start`, `/auth/oidc/{id}/callback` | public (browser redirects) |
//! | GET, POST | `/tokens` | session |
//! | DELETE | `/tokens/{id}` | session |
//! | GET | `/events` | any credential (SSE; `?token=` accepted) |
//!
//! At the root ([`root_router`]): `GET /account` (and `/account/`), `/account/app.js`,
//! `/account/app.css` — a self-contained page over the same REST API.
//!
//! See `docs/auth.md` and `docs/events.md`.

use std::sync::Arc;

use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use canvas_cloud_core::state::SharedState;

mod auth;
mod error;
mod events;
mod limits;
mod oidc;
mod page;
mod tokens;

pub use auth::{UserView, MAX_NEW_PASSWORD_CHARS};
pub use events::{KEEPALIVE, REVALIDATE_EVERY};
pub use oidc::{STATE_COOKIE, STATE_TTL_SECS};
pub use page::CONTENT_SECURITY_POLICY;
pub use tokens::{MAX_ACTIVE_TOKENS, MAX_EXPIRES_IN_DAYS};

/// Capabilities this crate adds to `GET /v1/info` `features`.
pub const FEATURES: &[&str] = &["accounts", "access_tokens", "events", "oidc"];

/// Per-router state the accounts handlers share (one instance per composed app, so tests
/// with different configs in one process do not share buckets or caches).
pub(crate) struct Shared {
    pub(crate) limits: limits::EmailLimiter,
    pub(crate) oidc: oidc::Cache,
    /// Offset of every `/v1/events` id (see [`events`]). 0: core's ids already increase across
    /// restarts, so the wire ids equal core's (and `/v1/apps/self/events` ids).
    pub(crate) events_base: u64,
}

impl Shared {
    fn new() -> Self {
        Self {
            limits: limits::EmailLimiter::default(),
            oidc: oidc::Cache::default(),
            events_base: 0,
        }
    }
}

pub(crate) type SharedAccounts = Arc<Shared>;

/// Routes under `/v1` (paths relative to `/v1`). Mounted by `canvas_cloud::app`.
pub fn router() -> Router<SharedState> {
    let shared: SharedAccounts = Arc::new(Shared::new());
    Router::new()
        .route("/auth/signup", post(auth::signup))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me).patch(auth::update_me))
        .route("/auth/password", post(auth::change_password))
        .route("/auth/oidc", get(oidc::list))
        .route("/auth/oidc/{id}/start", get(oidc::start))
        .route("/auth/oidc/{id}/callback", get(oidc::callback))
        .route("/tokens", get(tokens::list).post(tokens::create))
        .route("/tokens/{id}", delete(tokens::revoke))
        .route("/events", get(events::stream))
        .layer(Extension(shared))
}

/// Routes at the root: the `/account` page and its assets. Mounted by `canvas_cloud::app`.
pub fn root_router() -> Router<SharedState> {
    page::router()
}
