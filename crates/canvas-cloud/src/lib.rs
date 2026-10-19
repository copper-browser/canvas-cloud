//! The `canvas-cloud` binary as a library: app composition, CLI, server bootstrap, doctor and
//! admin commands. Integration tests drive [`app`] directly.

pub mod admin;
pub mod cli;
pub mod doctor;
pub mod serve;

use axum::Router;
use canvas_cloud_core::state::SharedState;
use canvas_cloud_core::Mounts;

/// Every crate's routes and `GET /v1/info` features, as the binary mounts them.
pub fn mounts() -> Mounts {
    Mounts::new()
        .v1(canvas_cloud_engine::router())
        .v1(canvas_cloud_accounts::router())
        .v1(canvas_cloud_apps::router())
        .v1(canvas_cloud_mcp::router())
        .root(canvas_cloud_engine::landing_router())
        .root(canvas_cloud_accounts::root_router())
        .root(canvas_cloud_mcp::root_router())
        .features(canvas_cloud_engine::FEATURES)
        .features(canvas_cloud_accounts::FEATURES)
        .features(canvas_cloud_apps::FEATURES)
        .features(canvas_cloud_mcp::FEATURES)
}

/// The complete HTTP app: `/healthz`, `GET /v1/info`, the canvas API, accounts, linked apps
/// and the agent seams (`/mcp`, `/llms.txt`, `/v1/openapi.json`), the `/join/{token}`
/// landing page, and the configured `web_root` for every other GET — wrapped in the auth
/// rate limiter, CORS and request tracing/metrics (see [`canvas_cloud_core::app_with`]).
pub fn app(state: SharedState) -> Router {
    canvas_cloud_core::app_with(state, mounts())
}

/// Version string: crate version plus the git revision CI stamps in at build time.
pub fn version() -> String {
    match option_env!("CANVAS_CLOUD_GIT_SHA") {
        Some(sha) if !sha.is_empty() => format!("{} ({sha})", env!("CARGO_PKG_VERSION")),
        _ => env!("CARGO_PKG_VERSION").to_owned(),
    }
}
