//! REST handlers: thin wrappers that map requests onto the canvas service ([`crate::api`])
//! and its results onto the wire (status codes, `{"links":…}`-style envelopes). No canvas
//! logic lives here.

use axum::body::Bytes;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{FromRequest, Path, Query, Request, State, WebSocketUpgrade};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use canvas_cloud_core::error::ApiError;
use canvas_cloud_core::extract::{OptionalJsonBody, QueryParams};
use canvas_cloud_core::identity::AuthUser;
use canvas_cloud_core::state::SharedState;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::chat::ReadWithChat;
use crate::mentions::{self, MarkReadBody, MentionBody, MentionList, MentionsRead, MentionsSent};
use crate::ops::OpsResult;
use crate::rest::{
    self, access, CanvasView, CreatedShareLink, InviteSent, InviteView, MemberView, OpsRequest,
    ReadOptions, ShareLinkPreview,
};
use crate::room::rooms;
use crate::ws::{serve_peer, MAX_MESSAGE_BYTES};

// ---------------------------------------------------------------------------------------------
// Extractors

/// JSON body (any or no `Content-Type`, like core's extractor) whose rejections are reported
/// as [`ApiError`]: `413` over the router's body limit, `400` on malformed JSON.
pub(crate) struct Body<T>(pub T);

impl<T, S> FromRequest<S> for Body<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state).await.map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::PayloadTooLarge
            } else {
                ApiError::bad_request("failed to read request body")
            }
        })?;
        serde_json::from_slice(&bytes)
            .map(Self)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
    }
}

fn ok() -> Json<Value> {
    Json(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------------------------
// Canvases

pub(crate) async fn list_canvases(
    State(state): State<SharedState>,
    user: AuthUser,
) -> Result<Json<Vec<CanvasView>>, ApiError> {
    Ok(Json(rest::list_canvases(&state, &user).await?))
}

#[derive(Deserialize)]
pub(crate) struct NameBody {
    name: String,
}

pub(crate) async fn create_canvas(
    State(state): State<SharedState>,
    user: AuthUser,
    Body(body): Body<NameBody>,
) -> Result<(StatusCode, Json<CanvasView>), ApiError> {
    let view = rest::create_canvas(&state, &user, &body.name).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

pub(crate) async fn get_canvas(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<CanvasView>, ApiError> {
    Ok(Json(rest::get_canvas(&state, &user, &raw).await?))
}

pub(crate) async fn rename_canvas(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    Body(body): Body<NameBody>,
) -> Result<Json<CanvasView>, ApiError> {
    Ok(Json(
        rest::rename_canvas(&state, &user, &raw, &body.name).await?,
    ))
}

pub(crate) async fn delete_canvas(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Value>, ApiError> {
    rest::delete_canvas(&state, &user, &raw).await?;
    Ok(ok())
}

// ---------------------------------------------------------------------------------------------
// Members

pub(crate) async fn list_members(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Vec<MemberView>>, ApiError> {
    Ok(Json(rest::list_members(&state, &user, &raw).await?))
}

pub(crate) async fn remove_member(
    State(state): State<SharedState>,
    user: AuthUser,
    Path((raw, target)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    rest::remove_member(&state, &user, &raw, &target).await?;
    Ok(ok())
}

// ---------------------------------------------------------------------------------------------
// Invites

#[derive(Deserialize)]
pub(crate) struct InviteBody {
    email: String,
}

pub(crate) async fn create_invite(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    Body(body): Body<InviteBody>,
) -> Result<(StatusCode, Json<InviteSent>), ApiError> {
    let sent = rest::create_invite(&state, &user, &raw, &body.email).await?;
    let status = if sent.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(sent)))
}

/// `DELETE /canvases/:id/invites/:invite_id` — the canvas owner or the original inviter
/// withdraws a pending invite (`204`). Other members get `403`; an unknown invite, one of
/// another canvas, or one that is no longer pending is `404`.
pub(crate) async fn revoke_invite(
    State(state): State<SharedState>,
    user: AuthUser,
    Path((raw, invite)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    rest::revoke_invite(&state, &user, &raw, &invite).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn list_canvas_invites(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Vec<InviteView>>, ApiError> {
    Ok(Json(rest::list_canvas_invites(&state, &user, &raw).await?))
}

pub(crate) async fn my_invites(
    State(state): State<SharedState>,
    user: AuthUser,
) -> Result<Json<Vec<InviteView>>, ApiError> {
    Ok(Json(rest::my_invites(&state, &user).await?))
}

pub(crate) async fn accept_invite(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<CanvasView>, ApiError> {
    Ok(Json(rest::accept_invite(&state, &user, &raw).await?))
}

pub(crate) async fn decline_invite(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Value>, ApiError> {
    rest::decline_invite(&state, &user, &raw).await?;
    Ok(ok())
}

// ---------------------------------------------------------------------------------------------
// Share links

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ShareLinkBody {
    role: Option<String>,
}

pub(crate) async fn create_share_link(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    OptionalJsonBody(body): OptionalJsonBody<ShareLinkBody, { crate::MAX_BODY_BYTES }>,
) -> Result<(StatusCode, Json<CreatedShareLink>), ApiError> {
    let link = rest::create_share_link(&state, &user, &raw, body.role.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(link)))
}

pub(crate) async fn list_share_links(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let links = rest::list_share_links(&state, &user, &raw).await?;
    Ok(Json(json!({ "links": links })))
}

pub(crate) async fn revoke_share_link(
    State(state): State<SharedState>,
    user: AuthUser,
    Path((raw, link_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    rest::revoke_share_link(&state, &user, &raw, &link_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn revoke_all_share_links(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<StatusCode, ApiError> {
    rest::revoke_all_share_links(&state, &user, &raw).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn preview_share_link(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(token): Path<String>,
) -> Result<Json<ShareLinkPreview>, ApiError> {
    Ok(Json(rest::preview_share_link(&state, &user, &token).await?))
}

pub(crate) async fn join_share_link(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(token): Path<String>,
) -> Result<Json<CanvasView>, ApiError> {
    Ok(Json(rest::join_share_link(&state, &user, &token).await?))
}

// ---------------------------------------------------------------------------------------------
// People

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PeopleQuery {
    q: Option<String>,
    limit: Option<i64>,
}

pub(crate) async fn list_people(
    State(state): State<SharedState>,
    user: AuthUser,
    QueryParams(q): QueryParams<PeopleQuery>,
) -> Result<Json<Value>, ApiError> {
    let people = rest::list_people(&state, &user, q.q.as_deref(), q.limit).await?;
    Ok(Json(json!({ "people": people })))
}

// ---------------------------------------------------------------------------------------------
// Documents

/// `GET /canvases/:id/ws` — upgrade to a y-websocket room. Read-only access tokens are
/// refused (`403 read_only_token`) by the [`AuthUser`] extractor before this runs.
pub(crate) async fn ws_upgrade(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let a = access(&state, user.user_id, &raw).await?;
    let ws = ws.map_err(|_| ApiError::bad_request("expected a WebSocket upgrade"))?;
    let room = rooms().get(a.canvas_id);
    room.ensure_loaded(&state).await?;
    let user_id = user.user_id;
    Ok(ws
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| serve_peer(state, room, user_id, socket))
        .into_response())
}

#[derive(Deserialize)]
pub(crate) struct StateQuery {
    /// Optional base64 state vector: return only what the caller is missing.
    sv: Option<String>,
}

pub(crate) async fn get_state(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    Query(q): Query<StateQuery>,
) -> Result<Json<Value>, ApiError> {
    let update = rest::get_state(&state, &user, &raw, q.sv.as_deref()).await?;
    Ok(Json(json!({ "state": STANDARD.encode(update) })))
}

#[derive(Deserialize)]
pub(crate) struct ReadQuery {
    #[serde(default)]
    full: bool,
    /// Comma-separated shape ids.
    ids: Option<String>,
    /// Comma-separated shape types.
    types: Option<String>,
}

fn csv(s: Option<&str>) -> Option<Vec<String>> {
    s.map(|s| s.split(',').map(str::to_owned).collect())
}

pub(crate) async fn read_canvas(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    Query(q): Query<ReadQuery>,
) -> Result<Json<ReadWithChat>, ApiError> {
    let options = ReadOptions {
        full: q.full,
        ids: csv(q.ids.as_deref()),
        types: csv(q.types.as_deref()),
    };
    Ok(Json(
        rest::read_canvas(&state, &user, &raw, &options).await?,
    ))
}

/// `POST /canvases/:id/ops` — `{ops:[…], as?:{id,name,color}|"name", confirm?, near?:{x,y}}`
/// or a bare array of ops. Envelope problems are reported like the page does: `200` with
/// `errors:[{index:-1,…}]`.
pub(crate) async fn post_ops(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    Body(body): Body<Value>,
) -> Result<Json<OpsResult>, ApiError> {
    // Membership first: a stranger gets 404 even for a malformed envelope.
    let a = access(&state, user.user_id, &raw).await?;
    let request = match OpsRequest::from_json(body) {
        Ok(request) => request,
        Err(e) => return Ok(Json(OpsResult::request_error(e))),
    };
    Ok(Json(rest::apply_ops_at(&state, &user, a, request).await?))
}

// ---------------------------------------------------------------------------------------------
// Mentions

pub(crate) async fn post_mentions(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    Body(body): Body<MentionBody>,
) -> Result<Json<MentionsSent>, ApiError> {
    Ok(Json(
        mentions::post_mentions(&state, &user, &raw, body).await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct MentionsQuery {
    unread: Option<String>,
    limit: Option<i64>,
}

pub(crate) async fn list_mentions(
    State(state): State<SharedState>,
    user: AuthUser,
    QueryParams(q): QueryParams<MentionsQuery>,
) -> Result<Json<MentionList>, ApiError> {
    let unread_only = mentions::flag(q.unread.as_deref())?;
    Ok(Json(
        mentions::list_mentions(&state, &user, unread_only, q.limit).await?,
    ))
}

pub(crate) async fn mark_read(
    State(state): State<SharedState>,
    user: AuthUser,
    Body(body): Body<MarkReadBody>,
) -> Result<Json<MentionsRead>, ApiError> {
    Ok(Json(mentions::mark_read(&state, &user, body).await?))
}

// ---------------------------------------------------------------------------------------------
// Share-link landing page (outside `/v1`)

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const LANDING_STYLE: &str = "body{margin:0;min-height:100vh;display:grid;place-items:center;background:#f7f7f5;color:#171717;font:16px system-ui,sans-serif}main{max-width:28rem;padding:2rem;text-align:center}a{display:inline-block;padding:.75rem 1.1rem;border-radius:.5rem;background:#171717;color:#fff;text-decoration:none}a:focus-visible{outline:3px solid #6d5dfc;outline-offset:3px}";

/// The landing page for a share link someone opened in a browser. It never consults the
/// database (the token is a credential; this page is unauthenticated). With `web_app_url`
/// configured, a well-formed token is handed to that web app (`{token}` substituted; meta
/// refresh plus a link); otherwise, or for a malformed token, a plain "open this link in your
/// canvas app" page.
pub(crate) async fn join_landing(
    State(state): State<SharedState>,
    Path(token): Path<String>,
) -> Response {
    let target = rest::token_digest(&token).and_then(|_| state.cfg.web_app_link(&token));
    let html = match target {
        Some(href) => {
            let href = escape_html(&href);
            format!(
                "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"refresh\" content=\"0;url={href}\"><title>Open shared canvas</title><style>{LANDING_STYLE}</style></head><body><main><h1>Canvas share link</h1><p>Opening the canvas…</p><a href=\"{href}\">Open the canvas</a></main></body></html>"
            )
        }
        None => format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Open shared canvas</title><style>{LANDING_STYLE}</style></head><body><main><h1>Canvas share link</h1><p>Open this link in your canvas app to continue.</p></main></body></html>"
        ),
    };
    let mut response = Html(html).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
    );
    response
}
