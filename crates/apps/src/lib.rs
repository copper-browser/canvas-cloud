//! Linked apps: `/v1/apps/self/*` (provisioning, events, admin listing, import, digest).
//! See CONTRACT §5.3 (and §6 for the import format).

use axum::Router;
use canvas_cloud_core::state::SharedState;

/// Capabilities this crate adds to `GET /v1/info` `features`.
pub const FEATURES: &[&str] = &[];

/// Routes under `/v1` (paths relative to `/v1`). Mounted by `canvas_cloud::app`.
pub fn router() -> Router<SharedState> {
    Router::new()
}
