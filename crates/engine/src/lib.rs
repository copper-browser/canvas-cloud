//! The Canvas Cloud engine: canvases, members, invites, share links, mentions, the agent ops
//! and read format, and Yjs (y-websocket) rooms.
//!
//! * [`api`] — the canvas service: every operation as a plain async function over
//!   `(&SharedState, &AuthUser, …)`. Other crates (MCP, app admin, import) call these
//!   instead of going through HTTP.
//! * [`router`] — every canvas route, without the `/v1` prefix (the binary nests it); thin
//!   handlers over [`api`].
//! * [`landing_router`] — the share-link landing page (`/join/{token}`, outside `/v1`).
//! * [`ensure_personal_canvas`] — the caller's Personal canvas (created on first use).
//! * [`ops`] / [`read`] — the spec §4 canvas ops and read format over a `yrs` document.
//! * [`rooms_metrics`] — live room / peer counts.
//! * [`admin`] — instance-admin operations (delete any canvas, disconnect a user).
//! * [`chat`] / [`mentions`] — the document's `chat` array as `/read` shows it, and chat
//!   @mention notifications (0.6.0).
//!
//! See `docs/canvas.md` and `docs/canvas-protocol.md`.

use axum::extract::DefaultBodyLimit;
use axum::routing::{any, delete, get, post};
use axum::Router;
use canvas_cloud_core::state::SharedState;

pub mod admin;
pub mod chat;
pub mod checklist;
pub mod compact;
pub mod digest;
pub mod geometry;
mod http;
pub mod mentions;
pub mod ops;
pub mod read;
mod rest;
mod room;
pub mod schema;
pub mod shape;
pub mod store;
mod ws;

pub use rest::{
    ensure_personal_canvas, CanvasView, InviteView, MemberView, UserRef,
    INVITE_NUDGE_INTERVAL_SECS, PERSONAL,
};

/// The canvas service: what the REST API does, callable in-process. Every function takes
/// `(&SharedState, &AuthUser, …)`, enforces the same membership and role rules as the routes
/// and publishes the same `canvas` events; errors are [`ApiError`]s, so a caller that is
/// itself an HTTP handler can return them as-is. A `canvas` argument is a canvas UUID or
/// [`PERSONAL`].
///
/// [`ApiError`]: canvas_cloud_core::error::ApiError
pub mod api {
    pub use crate::chat::ReadWithChat;
    pub use crate::mentions::{
        list_mentions, mark_read, post_mentions, MarkReadBody, MentionBody, MentionCanvas,
        MentionList, MentionView, MentionsRead, MentionsSent,
    };
    pub use crate::ops::{Actor, OpsResult};
    pub use crate::rest::{
        accept_invite, apply_ops, create_canvas, create_invite, create_share_link, decline_invite,
        delete_canvas, ensure_personal_canvas, get_canvas, get_state, join_share_link,
        list_canvas_invites, list_canvases, list_members, list_people, list_share_links,
        my_invites, preview_share_link, read_canvas, remove_member, rename_canvas,
        revoke_all_share_links, revoke_invite, revoke_share_link, CanvasView, CreatedShareLink,
        InviteSent, InviteView, MemberView, OpsRequest, PersonView, ReadOptions, ShareLinkOwner,
        ShareLinkPreview, ShareLinkView, UserRef, PERSONAL,
    };
}

/// Capabilities this crate adds to `GET /v1/info` `features`.
pub const FEATURES: &[&str] = &["canvases", "mentions", "share_links", "people"];
pub use admin::{canvases_deleted, disconnect_user, owned_canvases};
pub use room::{
    close_all_rooms, evict_room_now, open_rooms, room_in_memory, rooms_metrics, RoomsMetrics,
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
/// | POST | `/canvases/{id}/mentions` |
/// | GET | `/mentions` |
/// | POST | `/mentions/read` |
pub fn router() -> Router<SharedState> {
    Router::new()
        .route(
            "/canvases",
            get(http::list_canvases).post(http::create_canvas),
        )
        .route("/people", get(http::list_people))
        .route(
            "/canvases/{id}",
            get(http::get_canvas)
                .patch(http::rename_canvas)
                .delete(http::delete_canvas),
        )
        .route("/canvases/{id}/members", get(http::list_members))
        .route(
            "/canvases/{id}/members/{user_id}",
            delete(http::remove_member),
        )
        .route(
            "/canvases/{id}/invites",
            get(http::list_canvas_invites).post(http::create_invite),
        )
        .route(
            "/canvases/{id}/invites/{invite_id}",
            delete(http::revoke_invite),
        )
        .route("/invites", get(http::my_invites))
        .route("/invites/{id}/accept", post(http::accept_invite))
        .route("/invites/{id}/decline", post(http::decline_invite))
        .route(
            "/canvases/{id}/links",
            get(http::list_share_links)
                .post(http::create_share_link)
                .delete(http::revoke_all_share_links),
        )
        .route(
            "/canvases/{id}/links/{link_id}",
            axum::routing::delete(http::revoke_share_link),
        )
        .route("/canvas-links/{token}", get(http::preview_share_link))
        .route("/canvas-links/{token}/join", post(http::join_share_link))
        // `any`: HTTP/1.1 GET upgrades and HTTP/2 extended CONNECT (RFC 8441) both work.
        .route("/canvases/{id}/ws", any(http::ws_upgrade))
        .route("/canvases/{id}/state", get(http::get_state))
        .route("/canvases/{id}/read", get(http::read_canvas))
        .route(
            "/canvases/{id}/ops",
            post(http::post_ops).layer(DefaultBodyLimit::max(MAX_OPS_BODY_BYTES)),
        )
        .route("/canvases/{id}/mentions", post(http::post_mentions))
        .route("/mentions", get(http::list_mentions))
        .route("/mentions/read", post(http::mark_read))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

/// The share-link landing page (`GET /join/{token}`), merged at the application root rather
/// than under `/v1`. Unauthenticated; it never touches the database.
pub fn landing_router() -> Router<SharedState> {
    Router::new().route("/join/{token}", get(http::join_landing))
}
