//! Linked apps: `/v1/apps/self/*` (provisioning, events, admin listing, import, digest).
//! See CONTRACT §5.3 (and §6 for the import format), `docs/apps.md` and
//! `docs/import-format.md`.
//!
//! Every route authenticates with the app's own key (`Authorization: Bearer cva_…` without
//! `X-Canvas-User`, core's [`canvas_cloud_core::identity::AppAuth`]).

use axum::routing::{get, post, put};
use axum::Router;
use canvas_cloud_core::state::SharedState;

pub mod canvases;
pub mod common;
pub mod digest;
pub mod directory;
pub mod events;
pub mod import;
pub mod users;

/// Capabilities this crate adds to `GET /v1/info` `features`.
pub const FEATURES: &[&str] = &["app_events", "app_import"];

/// Routes under `/v1` (paths relative to `/v1`). Mounted by `canvas_cloud::app`.
pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/apps/self/users", get(users::list))
        // A literal segment containing a colon (axum 0.8 only rejects segments that *start*
        // with `:`). It is a different segment from `users` + `/{subject}`, so the two never
        // collide (tested).
        .route("/apps/self/users:batch", post(users::batch))
        .route(
            "/apps/self/users/{subject}",
            put(users::put_user).delete(users::delete),
        )
        .route(
            "/apps/self/users/{subject}/disconnect",
            post(users::disconnect),
        )
        .route("/apps/self/events", get(events::stream))
        .route("/apps/self/canvases", get(canvases::list))
        .route(
            "/apps/self/canvases/{id}",
            axum::routing::delete(canvases::delete),
        )
        .route("/apps/self/canvases/{id}/digest", get(canvases::digest))
        .route("/apps/self/stats", get(canvases::stats))
        .route("/apps/self/import", post(import::import))
}
