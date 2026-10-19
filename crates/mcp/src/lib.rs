//! Agent seams: `/mcp` (MCP streamable HTTP), `/llms.txt` and `/v1/openapi.json`.
//! See CONTRACT §5.4.

use axum::Router;
use canvas_cloud_core::state::SharedState;

/// Capabilities this crate adds to `GET /v1/info` `features`.
pub const FEATURES: &[&str] = &[];

/// Routes under `/v1` (paths relative to `/v1`, e.g. `/openapi.json`). Mounted by
/// `canvas_cloud::app`.
pub fn router() -> Router<SharedState> {
    Router::new()
}

/// Routes at the root (`/mcp`, `/llms.txt`). Mounted by `canvas_cloud::app`.
pub fn root_router() -> Router<SharedState> {
    Router::new()
}
