//! Agent seams (CONTRACT §5.4):
//!
//! * [`root_router`] — `POST /mcp`: a [Model Context Protocol] server over the Streamable HTTP
//!   transport (JSON-RPC 2.0, protocol [`PROTOCOL_VERSION`], also [`PROTOCOL_VERSION_2025_03_26`];
//!   single JSON responses, no server-initiated streams, stateless — no `Mcp-Session-Id`).
//!   Tools ([`tools`]) call the engine's in-process canvas service
//!   (`canvas_cloud_engine::api`), never HTTP. Plus `GET /llms.txt`, a plain-text guide for
//!   agents ([`llms`]).
//! * [`router`] — `GET /v1/openapi.json`: the hand-maintained `OpenAPI` 3.1 description of every
//!   `/v1` route ([`openapi`]), with `servers` set to the configured `public_url`.
//!
//! Every MCP request authenticates like the REST API: a session (`cvs_`), a personal access
//! token (`cvt_`) or an app key (`cva_`) plus `X-Canvas-User`. Read-only tokens see and may
//! call only the read tools. See `docs/agents.md`.
//!
//! [Model Context Protocol]: https://modelcontextprotocol.io/specification/2025-06-18

use axum::routing::{get, post};
use axum::Router;
use canvas_cloud_core::state::SharedState;

pub mod llms;
pub mod openapi;
pub mod tools;
mod transport;

pub use transport::{
    MAX_BODY_BYTES, PROTOCOL_VERSION, PROTOCOL_VERSION_2025_03_26, SUPPORTED_PROTOCOL_VERSIONS,
};

/// Capabilities this crate adds to `GET /v1/info` `features`.
pub const FEATURES: &[&str] = &["mcp"];

/// Routes under `/v1` (paths relative to `/v1`): `GET /openapi.json`. Mounted by
/// `canvas_cloud::app`.
pub fn router() -> Router<SharedState> {
    Router::new().route("/openapi.json", get(openapi::serve))
}

/// Routes at the root: `POST /mcp` (other methods → `405`, `Allow: POST`) and
/// `GET /llms.txt`. Mounted by `canvas_cloud::app`.
pub fn root_router() -> Router<SharedState> {
    Router::new()
        .route(
            "/mcp",
            post(transport::post_mcp).fallback(transport::method_not_allowed),
        )
        .route("/llms.txt", get(llms::serve))
}
