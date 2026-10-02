//! Canvases REST API (mounted under `/v1` by the binary).
//!
//! Access control: every canvas lookup joins `canvas_members` on the caller, so a canvas the
//! caller is not a member of is indistinguishable from one that does not exist (404).

use axum::body::Bytes;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{FromRequest, Path, Query, Request, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use copper_cloud_core::auth::{user_id_by_email, AuthUser};
use copper_cloud_core::crypto::Crypto;
use copper_cloud_core::error::ApiError;
use copper_cloud_core::events::Event;
use copper_cloud_core::extract::{OptionalJsonBody, QueryParams};
use copper_cloud_core::ids;
use copper_cloud_core::state::{AppState, SharedState};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;
use yrs::updates::decoder::Decode as _;
use yrs::{Doc, Map as _, ReadTxn as _, StateVector, Transact as _, WriteTxn as _};

use crate::geometry::Point;
use crate::ops::{
    apply_json_ops, read_agent, write_agent, Actor, AgentPatch, OpsCtx, OpsResult, WRITING_MS,
};
use crate::read::{read_canvas_txn, CanvasRef, ReadOpts, ReadResult};
use crate::room::{rooms, SERVER_ORIGIN};
use crate::schema::{out_to_json, AGENTS, META, SHAPES};
use crate::store;
use crate::ws::{serve_peer, MAX_MESSAGE_BYTES};

/// Path segment that resolves to the caller's Personal canvas.
pub const PERSONAL: &str = "personal";
/// Longest canvas name (characters).
pub const MAX_NAME_CHARS: usize = 200;

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

// ---------------------------------------------------------------------------------------------
// Views

/// A user as shown inside canvas/member/invite views.
#[derive(Debug, Clone, Serialize)]
pub struct UserRef {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
}

/// `GET /canvases` item.
#[derive(Debug, Clone, Serialize)]
pub struct CanvasView {
    pub id: Uuid,
    pub name: String,
    /// `personal` | `shared`.
    pub kind: String,
    /// The caller's role: `owner` | `editor`.
    pub role: String,
    pub owner: UserRef,
    pub member_count: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct CanvasRow {
    id: Uuid,
    name: String,
    kind: String,
    role: String,
    owner_id: Uuid,
    owner_email: String,
    owner_display_name: String,
    member_count: i64,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl From<CanvasRow> for CanvasView {
    fn from(r: CanvasRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            kind: r.kind,
            role: r.role,
            owner: UserRef {
                id: r.owner_id,
                email: r.owner_email,
                display_name: r.owner_display_name,
            },
            member_count: r.member_count,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

const CANVAS_VIEW_SQL: &str = "
SELECT c.id, c.name, c.kind, m.role, c.owner_id,
       u.email AS owner_email, u.display_name AS owner_display_name,
       (SELECT count(*) FROM canvas_members x WHERE x.canvas_id = c.id) AS member_count,
       c.created_at, c.updated_at
FROM canvas_members m
JOIN canvases c ON c.id = m.canvas_id
JOIN users u ON u.id = c.owner_id
WHERE m.user_id = $1 AND ($2::uuid IS NULL OR c.id = $2)
ORDER BY (c.kind = 'personal') DESC, c.updated_at DESC, c.id";

/// `GET /canvases/:id/members` item.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MemberView {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: String,
    #[serde(with = "time::serde::rfc3339")]
    pub added_at: OffsetDateTime,
}

/// An invite as shown to the inviter's canvas and to the invitee.
#[derive(Debug, Clone, Serialize)]
pub struct InviteView {
    pub id: Uuid,
    pub canvas_id: Uuid,
    pub canvas_name: String,
    pub email: String,
    /// `pending` | `accepted` | `declined`.
    pub status: String,
    pub invited_by: Option<UserRef>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A share link as returned by the owner's link list.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ShareLinkView {
    pub id: Uuid,
    pub role: String,
    pub created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub uses: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CreatedShareLink {
    id: Uuid,
    token: String,
    canvas_id: Uuid,
    role: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ShareLinkPreviewRow {
    canvas_id: Uuid,
    name: String,
    owner_id: Uuid,
    owner_display_name: String,
    role: String,
}

/// `GET /people` item. This intentionally omits account state and other private fields.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PersonView {
    pub id: Uuid,
    pub display_name: String,
    pub email: String,
}

#[derive(sqlx::FromRow)]
struct InviteRow {
    id: Uuid,
    canvas_id: Uuid,
    canvas_name: String,
    email: String,
    status: String,
    created_at: OffsetDateTime,
    inviter_id: Option<Uuid>,
    inviter_email: Option<String>,
    inviter_display_name: Option<String>,
}

impl From<InviteRow> for InviteView {
    fn from(r: InviteRow) -> Self {
        let invited_by = match (r.inviter_id, r.inviter_email) {
            (Some(id), Some(email)) => Some(UserRef {
                id,
                email,
                display_name: r.inviter_display_name.unwrap_or_default(),
            }),
            _ => None,
        };
        Self {
            id: r.id,
            canvas_id: r.canvas_id,
            canvas_name: r.canvas_name,
            email: r.email,
            status: r.status,
            invited_by,
            created_at: r.created_at,
        }
    }
}

const INVITE_VIEW_SQL: &str = "
SELECT i.id, i.canvas_id, c.name AS canvas_name, i.email, i.status, i.created_at,
       u.id AS inviter_id, u.email AS inviter_email, u.display_name AS inviter_display_name
FROM canvas_invites i
JOIN canvases c ON c.id = i.canvas_id
LEFT JOIN users u ON u.id = i.invited_by";

const SHARE_LINK_NOT_FOUND: &str = "link_not_found";

// ---------------------------------------------------------------------------------------------
// Helpers

/// The caller's access to one canvas.
#[derive(Debug, Clone)]
pub(crate) struct Access {
    pub canvas_id: Uuid,
    pub name: String,
    pub kind: String,
    pub role: String,
}

impl Access {
    fn is_personal(&self) -> bool {
        self.kind == "personal"
    }

    fn is_owner(&self) -> bool {
        self.role == "owner"
    }
}

/// Resolves `raw` (a UUID or `personal`) to a canvas the caller is a member of, else 404.
pub(crate) async fn access(state: &AppState, user_id: Uuid, raw: &str) -> Result<Access, ApiError> {
    let canvas_id = if raw == PERSONAL {
        ensure_personal_canvas(state, user_id).await?
    } else {
        Uuid::parse_str(raw).map_err(|_| ApiError::NotFound)?
    };
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT c.name, c.kind, m.role FROM canvases c
         JOIN canvas_members m ON m.canvas_id = c.id AND m.user_id = $2
         WHERE c.id = $1",
    )
    .bind(canvas_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let (name, kind, role) = row.ok_or(ApiError::NotFound)?;
    Ok(Access {
        canvas_id,
        name,
        kind,
        role,
    })
}

fn clean_name(raw: &str, max_chars: usize) -> Option<String> {
    let s: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect();
    let s = s.trim().to_owned();
    (!s.is_empty()).then_some(s)
}

fn canvas_name(raw: &str) -> Result<String, ApiError> {
    if raw.trim().chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::bad_request(format!(
            "name must be at most {MAX_NAME_CHARS} characters"
        )));
    }
    clean_name(raw, MAX_NAME_CHARS).ok_or_else(|| ApiError::bad_request("name must not be empty"))
}

/// Trimmed, lower-cased, plausibly-shaped email address.
fn invite_email(raw: &str) -> Result<String, ApiError> {
    let email = raw.trim().to_lowercase();
    let valid = (3..=254).contains(&email.len())
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !domain.contains('@')
        });
    if valid {
        Ok(email)
    } else {
        Err(ApiError::bad_request("invalid email address"))
    }
}

/// A fresh document update that sets `meta.name` / `meta.createdBy`.
fn initial_update(name: &str, created_by: Uuid) -> Vec<u8> {
    let doc = Doc::new();
    let meta = doc.get_or_insert_map(META);
    {
        let mut txn = doc.transact_mut();
        meta.insert(&mut txn, "name", name);
        meta.insert(&mut txn, "createdBy", created_by.to_string());
    }
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    update
}

/// Inserts a canvas, its owner membership and its initial document inside `tx`.
async fn insert_canvas(
    tx: &mut sqlx::PgConnection,
    state: &AppState,
    owner: Uuid,
    name: &str,
    kind: &str,
) -> Result<Option<Uuid>, ApiError> {
    let id = ids::uuid_v7();
    let key = Crypto::new_key();
    let wrapped = state.crypto.wrap_key(&key);
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO canvases (id, owner_id, name, kind, doc_key_wrapped)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (owner_id) WHERE kind = 'personal' DO NOTHING
         RETURNING id",
    )
    .bind(id)
    .bind(owner)
    .bind(name)
    .bind(kind)
    .bind(&wrapped)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_none() {
        return Ok(None);
    }
    sqlx::query("INSERT INTO canvas_members (canvas_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(id)
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    let sealed = Crypto::seal(&key, &store::aad(id), &initial_update(name, owner));
    sqlx::query(r#"INSERT INTO canvas_updates (canvas_id, "update") VALUES ($1, $2)"#)
        .bind(id)
        .bind(&sealed)
        .execute(&mut *tx)
        .await?;
    Ok(Some(id))
}

/// Returns the caller's Personal canvas, creating it (kind `personal`, name `Personal`) on
/// first use. Safe under concurrency (unique partial index on `owner_id`).
pub async fn ensure_personal_canvas(state: &AppState, user_id: Uuid) -> Result<Uuid, ApiError> {
    let find = || {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM canvases WHERE owner_id = $1 AND kind = 'personal'",
        )
        .bind(user_id)
    };
    if let Some(id) = find().fetch_optional(&state.db).await? {
        return Ok(id);
    }
    let mut tx = state.db.begin().await?;
    let created = insert_canvas(&mut tx, state, user_id, "Personal", "personal").await?;
    if let Some(id) = created {
        tx.commit().await?;
        tracing::info!(user_id = %user_id, canvas_id = %id, "personal canvas created");
        return Ok(id);
    }
    tx.rollback().await?;
    find()
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)
}

async fn canvas_view(state: &AppState, user_id: Uuid, id: Uuid) -> Result<CanvasView, ApiError> {
    sqlx::query_as::<_, CanvasRow>(CANVAS_VIEW_SQL)
        .bind(user_id)
        .bind(Some(id))
        .fetch_optional(&state.db)
        .await?
        .map(CanvasView::from)
        .ok_or(ApiError::NotFound)
}

pub(crate) async fn member_ids(state: &AppState, canvas_id: Uuid) -> Result<Vec<Uuid>, ApiError> {
    Ok(
        sqlx::query_scalar("SELECT user_id FROM canvas_members WHERE canvas_id = $1")
            .bind(canvas_id)
            .fetch_all(&state.db)
            .await?,
    )
}

pub(crate) fn publish(state: &AppState, users: &[Uuid], canvas_id: Uuid, kind: &str) {
    for user in users {
        state.events.publish(
            *user,
            Event::Canvas {
                canvas_id,
                kind: kind.to_owned(),
            },
        );
    }
}

async fn publish_members(state: &AppState, canvas_id: Uuid, kind: &str) {
    match member_ids(state, canvas_id).await {
        Ok(users) => publish(state, &users, canvas_id, kind),
        Err(e) => {
            tracing::warn!(%canvas_id, error = %e, "listing canvas members for an event failed");
        }
    }
}

/// The display name used for `by` when a REST caller does not say who they are.
async fn display_name(state: &AppState, user: &AuthUser) -> Result<String, ApiError> {
    let name: Option<String> = sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
        .bind(user.user_id)
        .fetch_optional(&state.db)
        .await?;
    Ok(name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| user.email.clone()))
}

fn ok() -> Json<Value> {
    Json(json!({ "ok": true }))
}

fn link_not_found() -> ApiError {
    ApiError::NotFoundCode {
        code: SHARE_LINK_NOT_FOUND,
        message: "share link not found",
    }
}

fn token_digest(token: &str) -> Option<[u8; 32]> {
    ids::is_token_shape(token).then(|| ids::sha256(token.as_bytes()))
}

// ---------------------------------------------------------------------------------------------
// Canvases

pub(crate) async fn list_canvases(
    State(state): State<SharedState>,
    user: AuthUser,
) -> Result<Json<Vec<CanvasView>>, ApiError> {
    ensure_personal_canvas(&state, user.user_id).await?;
    let rows = sqlx::query_as::<_, CanvasRow>(CANVAS_VIEW_SQL)
        .bind(user.user_id)
        .bind(None::<Uuid>)
        .fetch_all(&state.db)
        .await?;
    Ok(Json(rows.into_iter().map(CanvasView::from).collect()))
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
    let name = canvas_name(&body.name)?;
    let mut tx = state.db.begin().await?;
    let id = insert_canvas(&mut tx, &state, user.user_id, &name, "shared")
        .await?
        .ok_or_else(|| ApiError::internal("canvas insert returned nothing"))?;
    tx.commit().await?;
    tracing::info!(user_id = %user.user_id, canvas_id = %id, "canvas created");
    publish(&state, &[user.user_id], id, "created");
    Ok((
        StatusCode::CREATED,
        Json(canvas_view(&state, user.user_id, id).await?),
    ))
}

pub(crate) async fn get_canvas(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<CanvasView>, ApiError> {
    let a = access(&state, user.user_id, &raw).await?;
    Ok(Json(canvas_view(&state, user.user_id, a.canvas_id).await?))
}

pub(crate) async fn rename_canvas(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    Body(body): Body<NameBody>,
) -> Result<Json<CanvasView>, ApiError> {
    let a = access(&state, user.user_id, &raw).await?;
    if a.is_personal() {
        return Err(ApiError::bad_request(
            "the Personal canvas cannot be renamed",
        ));
    }
    if !a.is_owner() {
        return Err(ApiError::Forbidden);
    }
    let name = canvas_name(&body.name)?;
    sqlx::query("UPDATE canvases SET name = $2, updated_at = now() WHERE id = $1")
        .bind(a.canvas_id)
        .bind(&name)
        .execute(&state.db)
        .await?;
    // Keep the document's `meta.name` in step (persisted + broadcast like any update).
    let room = rooms().get(a.canvas_id);
    room.transact(&state, SERVER_ORIGIN, |txn| {
        let meta = txn.get_or_insert_map(META);
        meta.try_update(txn, "name", name.as_str());
    })
    .await?;
    publish_members(&state, a.canvas_id, "renamed").await;
    Ok(Json(canvas_view(&state, user.user_id, a.canvas_id).await?))
}

pub(crate) async fn delete_canvas(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let a = access(&state, user.user_id, &raw).await?;
    if a.is_personal() {
        return Err(ApiError::bad_request(
            "the Personal canvas cannot be deleted",
        ));
    }
    if !a.is_owner() {
        return Err(ApiError::Forbidden);
    }
    let members = member_ids(&state, a.canvas_id).await?;
    let deleted = sqlx::query("DELETE FROM canvases WHERE id = $1 AND owner_id = $2")
        .bind(a.canvas_id)
        .bind(user.user_id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    rooms().close(a.canvas_id);
    tracing::info!(user_id = %user.user_id, canvas_id = %a.canvas_id, "canvas deleted");
    publish(&state, &members, a.canvas_id, "deleted");
    Ok(ok())
}

// ---------------------------------------------------------------------------------------------
// Members

pub(crate) async fn list_members(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Vec<MemberView>>, ApiError> {
    let a = access(&state, user.user_id, &raw).await?;
    let rows = sqlx::query_as::<_, MemberView>(
        "SELECT m.user_id, u.email, u.display_name, m.role, m.added_at
         FROM canvas_members m JOIN users u ON u.id = m.user_id
         WHERE m.canvas_id = $1
         ORDER BY (m.role = 'owner') DESC, m.added_at, m.user_id",
    )
    .bind(a.canvas_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

pub(crate) async fn remove_member(
    State(state): State<SharedState>,
    user: AuthUser,
    Path((raw, target)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let a = access(&state, user.user_id, &raw).await?;
    let target = Uuid::parse_str(&target).map_err(|_| ApiError::NotFound)?;
    if a.is_personal() {
        return Err(ApiError::bad_request("the Personal canvas has no members"));
    }
    if target == user.user_id {
        if a.is_owner() {
            return Err(ApiError::bad_request(
                "the owner cannot leave a canvas; delete it instead",
            ));
        }
    } else if !a.is_owner() {
        return Err(ApiError::Forbidden);
    }
    let removed = sqlx::query(
        "DELETE FROM canvas_members WHERE canvas_id = $1 AND user_id = $2 AND role <> 'owner'",
    )
    .bind(a.canvas_id)
    .bind(target)
    .execute(&state.db)
    .await?
    .rows_affected();
    if removed == 0 {
        return Err(ApiError::NotFound);
    }
    rooms().kick_user(a.canvas_id, target);
    publish(&state, &[target], a.canvas_id, "member_removed");
    publish_members(&state, a.canvas_id, "member_removed").await;
    Ok(ok())
}

// ---------------------------------------------------------------------------------------------
// Invites

#[derive(Deserialize)]
pub(crate) struct InviteBody {
    email: String,
}

async fn invite_view(state: &AppState, id: Uuid) -> Result<InviteView, ApiError> {
    sqlx::query_as::<_, InviteRow>(&format!("{INVITE_VIEW_SQL} WHERE i.id = $1"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .map(InviteView::from)
        .ok_or(ApiError::NotFound)
}

pub(crate) async fn create_invite(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    Body(body): Body<InviteBody>,
) -> Result<(StatusCode, Json<InviteView>), ApiError> {
    let a = access(&state, user.user_id, &raw).await?;
    if a.is_personal() {
        return Err(ApiError::bad_request(
            "the Personal canvas cannot be shared",
        ));
    }
    let email = invite_email(&body.email)?;
    if email == user.email.trim().to_lowercase() {
        return Err(ApiError::bad_request(
            "you are already a member of this canvas",
        ));
    }
    let already_member: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM canvas_members m JOIN users u ON u.id = m.user_id
                        WHERE m.canvas_id = $1 AND lower(u.email) = $2)",
    )
    .bind(a.canvas_id)
    .bind(&email)
    .fetch_one(&state.db)
    .await?;
    if already_member {
        return Err(ApiError::Conflict(
            json!({ "message": "that user is already a member of this canvas" }),
        ));
    }
    let token = ids::sha256_hex(ids::random_token().as_bytes());
    let created: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO canvas_invites (id, canvas_id, email, invited_by, token)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (canvas_id, email) WHERE status = 'pending' DO NOTHING
         RETURNING id",
    )
    .bind(ids::uuid_v7())
    .bind(a.canvas_id)
    .bind(&email)
    .bind(user.user_id)
    .bind(&token)
    .fetch_optional(&state.db)
    .await?;
    let (status, id) = if let Some(id) = created {
        (StatusCode::CREATED, id)
    } else {
        let id: Uuid = sqlx::query_scalar(
            "SELECT id FROM canvas_invites
             WHERE canvas_id = $1 AND email = $2 AND status = 'pending'",
        )
        .bind(a.canvas_id)
        .bind(&email)
        .fetch_one(&state.db)
        .await?;
        (StatusCode::OK, id)
    };
    if status == StatusCode::CREATED {
        tracing::info!(user_id = %user.user_id, canvas_id = %a.canvas_id, invite_id = %id, "canvas invite created");
        if let Some(invitee) = user_id_by_email(&state.db, &email).await? {
            publish(&state, &[invitee], a.canvas_id, "invited");
        }
    }
    Ok((status, Json(invite_view(&state, id).await?)))
}

pub(crate) async fn list_canvas_invites(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Vec<InviteView>>, ApiError> {
    let a = access(&state, user.user_id, &raw).await?;
    let rows = sqlx::query_as::<_, InviteRow>(&format!(
        "{INVITE_VIEW_SQL} WHERE i.canvas_id = $1 AND i.status = 'pending' ORDER BY i.created_at DESC"
    ))
    .bind(a.canvas_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(InviteView::from).collect()))
}

pub(crate) async fn my_invites(
    State(state): State<SharedState>,
    user: AuthUser,
) -> Result<Json<Vec<InviteView>>, ApiError> {
    let rows = sqlx::query_as::<_, InviteRow>(&format!(
        "{INVITE_VIEW_SQL} WHERE i.email = lower($1) AND i.status = 'pending'
         ORDER BY i.created_at DESC"
    ))
    .bind(user.email.trim())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(InviteView::from).collect()))
}

pub(crate) async fn accept_invite(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<CanvasView>, ApiError> {
    let id = Uuid::parse_str(&raw).map_err(|_| ApiError::NotFound)?;
    let mut tx = state.db.begin().await?;
    let canvas_id: Uuid = sqlx::query_scalar(
        "SELECT canvas_id FROM canvas_invites
         WHERE id = $1 AND email = lower($2) AND status = 'pending'
         FOR UPDATE",
    )
    .bind(id)
    .bind(user.email.trim())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    sqlx::query(
        "INSERT INTO canvas_members (canvas_id, user_id, role) VALUES ($1, $2, 'editor')
         ON CONFLICT (canvas_id, user_id) DO NOTHING",
    )
    .bind(canvas_id)
    .bind(user.user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE canvas_invites SET status = 'accepted', accepted_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(user_id = %user.user_id, %canvas_id, invite_id = %id, "canvas invite accepted");
    publish_members(&state, canvas_id, "member_added").await;
    Ok(Json(canvas_view(&state, user.user_id, canvas_id).await?))
}

pub(crate) async fn decline_invite(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = Uuid::parse_str(&raw).map_err(|_| ApiError::NotFound)?;
    let row: Option<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "UPDATE canvas_invites SET status = 'declined'
         WHERE id = $1 AND email = lower($2) AND status = 'pending'
         RETURNING canvas_id, invited_by",
    )
    .bind(id)
    .bind(user.email.trim())
    .fetch_optional(&state.db)
    .await?;
    let (canvas_id, inviter) = row.ok_or(ApiError::NotFound)?;
    if let Some(inviter) = inviter {
        publish(&state, &[inviter], canvas_id, "invite_declined");
    }
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
    let access = access(&state, user.user_id, &raw).await?;
    if access.is_personal() {
        return Err(ApiError::bad_request(
            "the Personal canvas cannot be shared",
        ));
    }
    match body.role.as_deref() {
        None | Some("editor") => {}
        Some(_) => return Err(ApiError::bad_request("share link role must be editor")),
    }

    let token = ids::random_token();
    let digest = ids::sha256(token.as_bytes());
    let (id, role, created_at): (Uuid, String, OffsetDateTime) = sqlx::query_as(
        "INSERT INTO canvas_share_links (id, canvas_id, token_sha256, role, created_by)
         VALUES ($1, $2, $3, 'editor', $4)
         RETURNING id, role, created_at",
    )
    .bind(ids::uuid_v7())
    .bind(access.canvas_id)
    .bind(&digest[..])
    .bind(user.user_id)
    .fetch_one(&state.db)
    .await?;
    tracing::info!(user_id = %user.user_id, canvas_id = %access.canvas_id, link_id = %id, "canvas share link created");
    Ok((
        StatusCode::CREATED,
        Json(CreatedShareLink {
            id,
            token,
            canvas_id: access.canvas_id,
            role,
            created_at,
        }),
    ))
}

pub(crate) async fn list_share_links(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let access = access(&state, user.user_id, &raw).await?;
    if !access.is_owner() {
        return Err(ApiError::Forbidden);
    }
    let links = sqlx::query_as::<_, ShareLinkView>(
        "SELECT id, role, created_by, created_at, uses
         FROM canvas_share_links WHERE canvas_id = $1
         ORDER BY created_at DESC, id DESC",
    )
    .bind(access.canvas_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "links": links })))
}

pub(crate) async fn revoke_share_link(
    State(state): State<SharedState>,
    user: AuthUser,
    Path((raw, link_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let access = access(&state, user.user_id, &raw).await?;
    if !access.is_owner() {
        return Err(ApiError::Forbidden);
    }
    let link_id = Uuid::parse_str(&link_id).map_err(|_| ApiError::NotFound)?;
    let deleted = sqlx::query("DELETE FROM canvas_share_links WHERE id = $1 AND canvas_id = $2")
        .bind(link_id)
        .bind(access.canvas_id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn revoke_all_share_links(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
) -> Result<StatusCode, ApiError> {
    let access = access(&state, user.user_id, &raw).await?;
    if !access.is_owner() {
        return Err(ApiError::Forbidden);
    }
    sqlx::query("DELETE FROM canvas_share_links WHERE canvas_id = $1")
        .bind(access.canvas_id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn share_link_preview_row(
    state: &AppState,
    token: &str,
) -> Result<ShareLinkPreviewRow, ApiError> {
    let digest = token_digest(token).ok_or_else(link_not_found)?;
    sqlx::query_as::<_, ShareLinkPreviewRow>(
        "SELECT l.canvas_id, c.name, c.owner_id, u.display_name AS owner_display_name, l.role
         FROM canvas_share_links l
         JOIN canvases c ON c.id = l.canvas_id
         JOIN users u ON u.id = c.owner_id
         WHERE l.token_sha256 = $1",
    )
    .bind(&digest[..])
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(link_not_found)
}

pub(crate) async fn preview_share_link(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(token): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let row = share_link_preview_row(&state, &token).await?;
    let member: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM canvas_members WHERE canvas_id = $1 AND user_id = $2)",
    )
    .bind(row.canvas_id)
    .bind(user.user_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({
        "canvas_id": row.canvas_id,
        "name": row.name,
        "owner": { "id": row.owner_id, "display_name": row.owner_display_name },
        "role": row.role,
        "member": member,
    })))
}

pub(crate) async fn join_share_link(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(token): Path<String>,
) -> Result<Json<CanvasView>, ApiError> {
    let digest = token_digest(&token).ok_or_else(link_not_found)?;
    let mut tx = state.db.begin().await?;
    let row: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT l.canvas_id, l.role, c.kind FROM canvas_share_links l
         JOIN canvases c ON c.id = l.canvas_id
         WHERE l.token_sha256 = $1 FOR UPDATE",
    )
    .bind(&digest[..])
    .fetch_optional(&mut *tx)
    .await?;
    let Some((canvas_id, role, kind)) = row else {
        return Err(link_not_found());
    };
    if kind == "personal" {
        return Err(link_not_found());
    }
    let added: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO canvas_members (canvas_id, user_id, role) VALUES ($1, $2, $3)
         ON CONFLICT (canvas_id, user_id) DO NOTHING RETURNING user_id",
    )
    .bind(canvas_id)
    .bind(user.user_id)
    .bind(&role)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE canvas_invites SET status = 'accepted', accepted_at = COALESCE(accepted_at, now())
         WHERE canvas_id = $1 AND lower(email) = lower($2) AND status = 'pending'",
    )
    .bind(canvas_id)
    .bind(user.email.trim())
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE canvas_share_links SET uses = uses + 1 WHERE token_sha256 = $1")
        .bind(&digest[..])
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    publish_members(&state, canvas_id, "member_added").await;
    tracing::info!(user_id = %user.user_id, %canvas_id, joined = added.is_some(), "canvas share link joined");
    Ok(Json(canvas_view(&state, user.user_id, canvas_id).await?))
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
    let limit = q.limit.unwrap_or(20);
    if limit < 1 {
        return Err(ApiError::bad_request("limit must be at least 1"));
    }
    let limit = limit.min(50);
    let needle = q.q.unwrap_or_default();
    let people = sqlx::query_as::<_, PersonView>(
        "SELECT id, display_name, email FROM users
         WHERE NOT disabled AND id <> $1
           AND ($2 = '' OR strpos(lower(display_name), lower($2)) > 0
                OR strpos(lower(email), lower($2)) > 0)
         ORDER BY lower(display_name), id
         LIMIT $3",
    )
    .bind(user.user_id)
    .bind(needle)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "people": people })))
}

// ---------------------------------------------------------------------------------------------
// Documents

/// `GET /canvases/:id/ws` — upgrade to a y-websocket room.
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
    let a = access(&state, user.user_id, &raw).await?;
    let sv = match q.sv.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => {
            let bytes =
                ids::b64_decode_any(s).map_err(|_| ApiError::bad_request("sv is not base64"))?;
            StateVector::decode_v1(&bytes)
                .map_err(|_| ApiError::bad_request("sv is not a state vector"))?
        }
        None => StateVector::default(),
    };
    let room = rooms().get(a.canvas_id);
    let g = room.read(&state).await?;
    let update = g.doc().transact().encode_state_as_update_v1(&sv);
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
    s.map(|s| {
        s.split(',')
            .map(|p| crate::shape::bare_id(p.trim()).to_owned())
            .filter(|p| !p.is_empty())
            .collect()
    })
}

pub(crate) async fn read_canvas(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    Query(q): Query<ReadQuery>,
) -> Result<Json<ReadResult>, ApiError> {
    let a = access(&state, user.user_id, &raw).await?;
    let room = rooms().get(a.canvas_id);
    let g = room.read(&state).await?;
    let opts = ReadOpts {
        full: q.full,
        ids: csv(q.ids.as_deref()),
        types: csv(q.types.as_deref()),
        now_ms: 0,
    };
    let mut out = read_canvas_txn(&g.doc().transact(), &opts);
    out.canvas = Some(CanvasRef {
        id: a.canvas_id.to_string(),
        name: a.name,
        kind: a.kind,
    });
    Ok(Json(out))
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
    let a = access(&state, user.user_id, &raw).await?;
    let (ops, actor, confirm, near) = match body {
        Value::Array(ops) => (ops, None, false, None),
        Value::Object(mut o) => {
            let Some(Value::Array(ops)) = o.remove("ops") else {
                return Ok(Json(OpsResult::request_error("`ops` must be an array")));
            };
            let actor = match Actor::parse(o.get("as")) {
                Ok(actor) => actor,
                Err(e) => return Ok(Json(OpsResult::request_error(e))),
            };
            let near = o.get("near").and_then(|n| {
                Some(Point {
                    x: n.get("x")?.as_f64()?,
                    y: n.get("y")?.as_f64()?,
                })
            });
            (
                ops,
                actor,
                o.get("confirm") == Some(&Value::Bool(true)),
                near,
            )
        }
        _ => {
            return Ok(Json(OpsResult::request_error(
                "expected {ops:[...], as?:{id,name,color}} or an array of ops",
            )))
        }
    };
    let by = match &actor {
        Some(actor) => actor.name.clone(),
        None => display_name(&state, &user).await?,
    };
    let mut ctx = OpsCtx::new(by);
    ctx.confirm_clear = confirm;
    if let Some(p) = near {
        ctx.origin = p;
    }

    let room = rooms().get(a.canvas_id);
    let (result, wrote) = room
        .transact(&state, SERVER_ORIGIN, |txn| {
            let shapes = txn.get_or_insert_map(SHAPES);
            let result = apply_json_ops(txn, &shapes, &ops, &ctx);
            let wrote = match &actor {
                Some(actor) if result.applied > 0 => {
                    let agents = txn.get_or_insert_map(AGENTS);
                    write_agent(
                        txn,
                        &agents,
                        &actor.id,
                        &actor.writing(result.anchor),
                        ctx.now_ms,
                    );
                    Some(ctx.now_ms)
                }
                _ => None,
            };
            (result, wrote)
        })
        .await?;
    if let (Some(actor), Some(at)) = (actor, wrote) {
        schedule_idle(Arc::clone(&state), a.canvas_id, actor.id, at);
    }
    tracing::debug!(
        user_id = %user.user_id,
        canvas_id = %a.canvas_id,
        applied = result.applied,
        errors = result.errors.len(),
        "canvas ops applied"
    );
    Ok(Json(result))
}

/// After [`WRITING_MS`], flips the agent back to `idle` unless it wrote again since.
fn schedule_idle(state: SharedState, canvas_id: Uuid, agent_id: String, written_at: i64) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(WRITING_MS)).await;
        let Some(room) = rooms().peek(canvas_id) else {
            return;
        };
        let res = room
            .transact(&state, SERVER_ORIGIN, |txn| {
                let agents = txn.get_or_insert_map(AGENTS);
                let current = agents
                    .get(txn, &agent_id)
                    .map(|o| out_to_json(txn, &o))
                    .and_then(|v| read_agent(&v));
                #[allow(clippy::cast_precision_loss)]
                let still = current.is_some_and(|c| {
                    c.status == "writing" && (c.updated_at - written_at as f64).abs() < 0.5
                });
                if still {
                    let idle = AgentPatch {
                        status: Some("idle"),
                        ..AgentPatch::default()
                    };
                    write_agent(txn, &agents, &agent_id, &idle, crate::schema::now_ms());
                }
            })
            .await;
        if let Err(e) = res {
            tracing::debug!(%canvas_id, error = %e, "agent idle write skipped");
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Ungated web landing page

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn percent_encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Accept only a host name/IP and an optional numeric port. In particular, reject user-info,
/// paths and control characters before putting the value in a custom URL scheme.
fn validated_host(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    // HTTP/1.1 carries the target in a literal `Host` header; HTTP/2 (our
    // only transport once TLS is on, since rustls negotiates h2 by ALPN)
    // forbids that header and sends `:authority` instead, which hyper/h2
    // surface as the request URI's authority, not a header — so a host
    // taken from headers alone is always absent on this listener.
    let from_header = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let from_authority = uri.authority().map(|a| a.as_str().to_owned());
    let raw = from_header.or(from_authority)?;
    let raw = raw.as_str();
    if raw.is_empty()
        || raw.trim() != raw
        || raw.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    if raw.contains(['@', '/', '?', '#', '%']) {
        return None;
    }
    let host = if let Some(rest) = raw.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let suffix = &rest[end + 1..];
        if suffix.is_empty() {
            host
        } else {
            let port = suffix.strip_prefix(':').filter(|p| !p.is_empty())?;
            if port.parse::<u16>().is_err() {
                return None;
            }
            host
        }
    } else {
        if raw.matches(':').count() > 1 {
            return None;
        }
        match raw.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() => {
                if port.parse::<u16>().is_err() {
                    return None;
                }
                host
            }
            Some((_, _)) => return None,
            None => raw,
        }
    };
    if host.is_empty()
        || host.parse::<std::net::IpAddr>().is_err() && url::Host::parse(host).is_err()
    {
        return None;
    }
    Some(raw.to_owned())
}

/// `GET /join/:token` deliberately does not consult the database. It is outside `/v1`, so it is
/// reachable before a client has an instance key and can hand the opaque token to Copper.
pub(crate) async fn join_landing(Path(token): Path<String>, uri: Uri, headers: HeaderMap) -> Response {
    let href = token_digest(&token).and_then(|_| {
        validated_host(&headers, &uri).map(|host| {
            format!(
                "copper://canvas/join/{}?cloud={}",
                percent_encode_component(&token),
                host
            )
        })
    });
    let html = match href {
        Some(href) => format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Open canvas in Copper</title><style>body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#f7f7f5;color:#171717;font:16px system-ui,sans-serif}}main{{max-width:28rem;padding:2rem;text-align:center}}a{{display:inline-block;padding:.75rem 1.1rem;border-radius:.5rem;background:#171717;color:#fff;text-decoration:none}}a:focus-visible{{outline:3px solid #6d5dfc;outline-offset:3px}}</style></head><body><main><h1>Canvas share link</h1><p>Open this canvas in Copper to continue.</p><a href=\"{}\">Open this canvas in Copper</a></main></body></html>",
            escape_html(&href)
        ),
        None => "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Open canvas in Copper</title><style>body{margin:0;min-height:100vh;display:grid;place-items:center;background:#f7f7f5;color:#171717;font:16px system-ui,sans-serif}main{max-width:28rem;padding:2rem;text-align:center}</style></head><body><main><h1>Canvas share link</h1><p>Open this link in Copper to continue.</p></main></body></html>".to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_falls_back_to_authority_when_no_host_header() {
        // HTTP/2 requests never carry a literal `Host` header (RFC 9113 §8.3.1
        // forbids it); hyper/h2 surface the `:authority` pseudo-header as the
        // request URI's authority instead. The join landing page is only ever
        // served over h2 once TLS is on, so this path must work from the URI
        // alone, not just from headers.
        let empty = HeaderMap::new();
        let uri: Uri = "https://127.0.0.1:8445/join/abc".parse().unwrap();
        assert_eq!(
            validated_host(&empty, &uri).as_deref(),
            Some("127.0.0.1:8445")
        );

        // A literal Host header (HTTP/1.1) still wins when present.
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("example.com"));
        assert_eq!(
            validated_host(&headers, &uri).as_deref(),
            Some("example.com")
        );

        // Neither a header nor a URI authority: no host to offer.
        let relative: Uri = "/join/abc".parse().unwrap();
        assert_eq!(validated_host(&empty, &relative), None);
    }

    #[test]
    fn names() {
        assert_eq!(canvas_name("  Plan \u{7}A ").unwrap(), "Plan A");
        assert!(canvas_name("   ").is_err());
        assert!(canvas_name(&"x".repeat(201)).is_err());
        assert_eq!(canvas_name(&"é".repeat(200)).unwrap().chars().count(), 200);
    }

    #[test]
    fn emails() {
        assert_eq!(
            invite_email(" Bob@Example.COM ").unwrap(),
            "bob@example.com"
        );
        for bad in ["", "bob", "bob@", "@x.io", "a b@c.io", "a@b", "a@.b.io"] {
            assert!(invite_email(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn initial_update_sets_meta() {
        let id = Uuid::now_v7();
        let update = initial_update("Board", id);
        let doc = Doc::new();
        doc.transact_mut()
            .apply_update(yrs::Update::decode_v1(&update).unwrap())
            .unwrap();
        let txn = doc.transact();
        assert_eq!(crate::read::meta_name(&txn).as_deref(), Some("Board"));
    }
}
