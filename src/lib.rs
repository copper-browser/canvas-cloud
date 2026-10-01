//! Canvas: canvases REST API + Yjs (y-websocket) rooms for copper-cloud.
//!
//! * [`router`] — every canvas route, without the `/v1` prefix (the binary nests it).
//! * [`ensure_personal_canvas`] — the caller's Personal canvas (created on first use).
//! * [`ops`] / [`read`] — the spec §4 canvas ops and read format over a `yrs` document.
//! * [`rooms_metrics`] — live room / peer counts.
//!
//! See `docs/canvas.md` and `docs/canvas-protocol.md`.

use axum::extract::DefaultBodyLimit;
use axum::routing::{any, delete, get, post};
use axum::Router;
use copper_cloud_core::state::SharedState;

pub mod geometry;
pub mod ops;
pub mod read;
mod rest;
mod room;
pub mod schema;
pub mod shape;
mod store;
mod ws;

pub use rest::{ensure_personal_canvas, CanvasView, InviteView, MemberView, UserRef, PERSONAL};
pub use room::{
    close_all_rooms, evict_room_now, room_in_memory, rooms_metrics, RoomsMetrics,
    BROADCAST_CAPACITY, COMPACT_EVERY, EVENT_THROTTLE, IDLE_EVICT,
};
pub use ws::{close as close_codes, MAX_MESSAGE_BYTES, PING_EVERY, READ_TIMEOUT};

/// Largest accepted REST request body (except ops).
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Largest accepted `POST /canvases/:id/ops` body: room for a 2 MiB `data:` image plus JSON.
pub const MAX_OPS_BODY_BYTES: usize = 4 << 20;

/// All canvas routes (mount under `/v1`). `:id` may be a canvas UUID or `personal`.
///
/// | Method | Path |
/// |---|---|
/// | GET, POST | `/canvases` |
/// | GET, PATCH, DELETE | `/canvases/{id}` |
/// | GET | `/canvases/{id}/members` |
/// | DELETE | `/canvases/{id}/members/{user_id}` |
/// | GET, POST | `/canvases/{id}/invites` |
/// | GET | `/invites` |
/// | POST | `/invites/{id}/accept`, `/invites/{id}/decline` |
/// | GET (upgrade) | `/canvases/{id}/ws` (WebSocket; HTTP/2 CONNECT too) |
/// | GET | `/canvases/{id}/state`, `/canvases/{id}/read` |
/// | POST | `/canvases/{id}/ops` |
pub fn router() -> Router<SharedState> {
    Router::new()
        .route(
            "/canvases",
            get(rest::list_canvases).post(rest::create_canvas),
        )
        .route(
            "/canvases/{id}",
            get(rest::get_canvas)
                .patch(rest::rename_canvas)
                .delete(rest::delete_canvas),
        )
        .route("/canvases/{id}/members", get(rest::list_members))
        .route(
            "/canvases/{id}/members/{user_id}",
            delete(rest::remove_member),
        )
        .route(
            "/canvases/{id}/invites",
            get(rest::list_canvas_invites).post(rest::create_invite),
        )
        .route("/invites", get(rest::my_invites))
        .route("/invites/{id}/accept", post(rest::accept_invite))
        .route("/invites/{id}/decline", post(rest::decline_invite))
        // `any`: HTTP/1.1 GET upgrades and HTTP/2 extended CONNECT (RFC 8441) both work.
        .route("/canvases/{id}/ws", any(rest::ws_upgrade))
        .route("/canvases/{id}/state", get(rest::get_state))
        .route("/canvases/{id}/read", get(rest::read_canvas))
        .route(
            "/canvases/{id}/ops",
            post(rest::post_ops).layer(DefaultBodyLimit::max(MAX_OPS_BODY_BYTES)),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}
