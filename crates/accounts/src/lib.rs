//! Human accounts: `/v1/auth/*` (signup, login, logout, me, password, OIDC), `/v1/tokens`
//! (personal access tokens) and `/v1/events` (SSE). See CONTRACT §5.2.

use axum::Router;
use canvas_cloud_core::state::SharedState;

/// Capabilities this crate adds to `GET /v1/info` `features`.
pub const FEATURES: &[&str] = &[];

/// Routes under `/v1` (paths relative to `/v1`). Mounted by `canvas_cloud::app`.
pub fn router() -> Router<SharedState> {
    Router::new()
}

/// Routes at the root (e.g. `/account`). Mounted by `canvas_cloud::app`.
pub fn root_router() -> Router<SharedState> {
    Router::new()
}
