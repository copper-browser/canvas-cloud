//! Canvas: canvases REST API + Yjs (y-websocket) rooms for copper-cloud.
//!
//! * [`router`] — every canvas route, without the `/v1` prefix (the binary nests it).
//! * [`ensure_personal_canvas`] — the caller's Personal canvas (created on first use).
//! * [`ops`] / [`read`] — the spec §4 canvas ops and read format over a `yrs` document.
//! * [`rooms_metrics`] — live room / peer counts.
//! * [`admin`] — instance-admin operations (delete any canvas, disconnect a user).
//!
//! See `docs/canvas.md` and `docs/canvas-protocol.md`.

use axum::extract::DefaultBodyLimit;
use axum::routing::{any, delete, get, post};
use axum::Router;
use copper_cloud_core::state::SharedState;

pub mod admin;
pub mod geometry;
pub mod ops;
pub mod read;
mod rest;
mod room;
pub mod schema;
pub mod shape;
mod store;
mod ws;

pub use rest::{
    ensure_personal_canvas, CanvasView, InviteView, MemberView, UserRef,
    INVITE_NUDGE_INTERVAL_SECS, PERSONAL,
};
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
/// | DELETE | `/canvases/{id}/invites/{invite_id}` |
/// | GET | `/invites` |
/// | POST | `/invites/{id}/accept`, `/invites/{id}/decline` |
/// | POST | `/canvases/{id}/links` |
/// | GET | `/canvases/{id}/links` |
/// | DELETE | `/canvases/{id}/links`, `/canvases/{id}/links/{link_id}` |
/// | GET, POST | `/canvas-links/{token}`, `/canvas-links/{token}/join` |
/// | GET (upgrade) | `/canvases/{id}/ws` (WebSocket; HTTP/2 CONNECT too) |
/// | GET | `/canvases/{id}/state`, `/canvases/{id}/read` |
/// | POST | `/canvases/{id}/ops` |
pub fn router() -> Router<SharedState> {
    Router::new()
        .route(
            "/canvases",
            get(rest::list_canvases).post(rest::create_canvas),
        )
        .route("/people", get(rest::list_people))
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
        .route(
            "/canvases/{id}/invites/{invite_id}",
            delete(rest::revoke_invite),
        )
        .route("/invites", get(rest::my_invites))
        .route("/invites/{id}/accept", post(rest::accept_invite))
        .route("/invites/{id}/decline", post(rest::decline_invite))
        .route(
            "/canvases/{id}/links",
            get(rest::list_share_links)
                .post(rest::create_share_link)
                .delete(rest::revoke_all_share_links),
        )
        .route(
            "/canvases/{id}/links/{link_id}",
            axum::routing::delete(rest::revoke_share_link),
        )
        .route("/canvas-links/{token}", get(rest::preview_share_link))
        .route("/canvas-links/{token}/join", post(rest::join_share_link))
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

/// The public landing page for web-form share links. It is merged at the application root rather
/// than under `/v1`, so the instance-key gate does not run for it.
pub fn landing_router() -> Router<SharedState> {
    Router::new().route("/join/{token}", get(rest::join_landing))
}
