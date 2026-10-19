//! `/v1/apps/self/canvases*` and `/v1/apps/self/stats`: what an app's users own.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::Json;
use canvas_cloud_core::error::ApiError;
use canvas_cloud_core::extract::QueryParams;
use canvas_cloud_core::identity::{self, AppAuth};
use canvas_cloud_core::state::SharedState;
use canvas_cloud_engine::{admin, store};
use futures::TryStreamExt as _;
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;
use yrs::updates::decoder::Decode as _;
use yrs::{Doc, Transact as _, Update};

use crate::common::{display_name_json, Error, ListQuery, Result};
use crate::digest::document_digest;
use crate::users::actor;

fn parse_id(raw: &str) -> Result<Uuid> {
    Uuid::parse_str(raw).map_err(|_| Error::Api(ApiError::NotFound))
}

fn rfc3339(t: OffsetDateTime) -> Value {
    t.format(&time::format_description::well_known::Rfc3339)
        .map_or(Value::Null, Value::from)
}

/// Canvases this app may manage: owned by one of its users, or imported by it.
const APP_CANVAS: &str = "(EXISTS (SELECT 1 FROM app_users au
                                   WHERE au.app_id = $1 AND au.user_id = c.owner_id)
                           OR c.imported_by_app = $1)";

/// Whether canvas `id` exists and is this app's to manage.
pub async fn is_app_canvas(state: &SharedState, app_id: Uuid, id: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM canvases c WHERE c.id = $2 AND {APP_CANVAS})"
    ))
    .bind(app_id)
    .bind(id)
    .fetch_one(&state.db)
    .await?)
}

#[derive(sqlx::FromRow)]
struct CanvasRow {
    id: Uuid,
    name: String,
    kind: String,
    owner_id: Uuid,
    owner_subject: Option<String>,
    owner_email: String,
    owner_display_name: String,
    member_count: i64,
    share_link_count: i64,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    imported_at: Option<OffsetDateTime>,
}

/// `GET /v1/apps/self/canvases?kind=&limit=&offset=`: canvases owned by this app's users,
/// newest first: `{canvases:[{id,name,kind,owner:{subject,user_id,email,display_name},
/// member_count,share_link_count,created_at,updated_at,imported_at}],total,limit,offset}`.
pub async fn list(
    State(state): State<SharedState>,
    app: AppAuth,
    QueryParams(q): QueryParams<ListQuery>,
) -> Result<Json<Value>> {
    let (limit, offset) = q.page();
    let kind = q.kind.as_deref().map(str::trim).filter(|k| !k.is_empty());
    if kind.is_some_and(|k| !matches!(k, "personal" | "shared")) {
        return Err(Error::bad_request("kind must be personal or shared"));
    }
    let rows = sqlx::query_as::<_, CanvasRow>(
        "SELECT c.id, c.name, c.kind, c.owner_id, au.subject AS owner_subject,
                u.email AS owner_email, u.display_name AS owner_display_name,
                (SELECT count(*) FROM canvas_members m WHERE m.canvas_id = c.id) AS member_count,
                (SELECT count(*) FROM canvas_share_links l WHERE l.canvas_id = c.id)
                    AS share_link_count,
                c.created_at, c.updated_at, c.imported_at
         FROM canvases c
         JOIN app_users au ON au.user_id = c.owner_id AND au.app_id = $1
         JOIN users u ON u.id = c.owner_id
         WHERE ($2::text IS NULL OR c.kind = $2)
         ORDER BY c.created_at DESC, c.id DESC
         LIMIT $3 OFFSET $4",
    )
    .bind(app.app_id)
    .bind(kind)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM canvases c
         JOIN app_users au ON au.user_id = c.owner_id AND au.app_id = $1
         WHERE ($2::text IS NULL OR c.kind = $2)",
    )
    .bind(app.app_id)
    .bind(kind)
    .fetch_one(&state.db)
    .await?;
    let canvases: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id,
                "name": r.name,
                "kind": r.kind,
                "owner": {
                    "subject": r.owner_subject,
                    "user_id": r.owner_id,
                    "email": r.owner_email,
                    "display_name": display_name_json(&r.owner_display_name),
                },
                "member_count": r.member_count,
                "share_link_count": r.share_link_count,
                "created_at": rfc3339(r.created_at),
                "updated_at": rfc3339(r.updated_at),
                "imported_at": r.imported_at.map_or(Value::Null, rfc3339),
            })
        })
        .collect();
    Ok(Json(json!({
        "canvases": canvases,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

/// `DELETE /v1/apps/self/canvases/{id}`: delete a canvas owned by one of this app's users (or
/// imported by this app). Its room closes with 4404 and members get a `deleted` event.
pub async fn delete(
    State(state): State<SharedState>,
    app: AppAuth,
    Path(raw): Path<String>,
) -> Result<Json<Value>> {
    let id = parse_id(&raw)?;
    if !is_app_canvas(&state, app.app_id, id).await? {
        return Err(Error::Api(ApiError::NotFound));
    }
    let name = admin::delete_canvas(&state, id)
        .await?
        .ok_or(Error::Api(ApiError::NotFound))?;
    identity::audit(
        &state.db,
        &actor(&app),
        "app.canvas.delete",
        &id.to_string(),
        json!({ "name": name }),
    )
    .await;
    Ok(Json(json!({ "ok": true, "id": id, "name": name })))
}

/// `GET /v1/apps/self/stats`: `{app:{id,name,allow_import,trust_email}, allow_import, users,
/// canvases, rooms:{rooms,peers}}` — canvases owned by this app's users, and the rooms of
/// those canvases currently open on this server. (`allow_import` is repeated at the top level
/// for clients that look for it there.)
pub async fn stats(State(state): State<SharedState>, app: AppAuth) -> Result<Json<Value>> {
    let (users, canvases): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM app_users WHERE app_id = $1),
                (SELECT count(*) FROM canvases c
                 JOIN app_users au ON au.user_id = c.owner_id AND au.app_id = $1)",
    )
    .bind(app.app_id)
    .fetch_one(&state.db)
    .await?;
    let open: HashMap<Uuid, usize> = admin::open_rooms().into_iter().collect();
    let (mut rooms, mut peers) = (0_usize, 0_usize);
    if !open.is_empty() {
        let ids: Vec<Uuid> = open.keys().copied().collect();
        let mine: Vec<Uuid> = sqlx::query_scalar(
            "SELECT c.id FROM canvases c
             JOIN app_users au ON au.user_id = c.owner_id AND au.app_id = $1
             WHERE c.id = ANY($2)",
        )
        .bind(app.app_id)
        .bind(&ids)
        .fetch_all(&state.db)
        .await?;
        for id in mine {
            rooms += 1;
            peers += open.get(&id).copied().unwrap_or(0);
        }
    }
    Ok(Json(json!({
        "app": {
            "id": app.app_id,
            "name": app.name,
            "allow_import": app.allow_import,
            "trust_email": app.trust_email,
        },
        "allow_import": app.allow_import,
        "users": users,
        "canvases": canvases,
        "rooms": { "rooms": rooms, "peers": peers },
    })))
}

/// Loads the stored document of `canvas_id` (compacted snapshot + update tail, one
/// consistent read). `None` if the canvas does not exist.
pub async fn load_doc(state: &SharedState, canvas_id: Uuid) -> Result<Option<Doc>> {
    let mut tx = state.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let wrapped: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT doc_key_wrapped FROM canvases WHERE id = $1")
            .bind(canvas_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(wrapped) = wrapped else {
        return Ok(None);
    };
    let key = state.crypto.unwrap_key(&wrapped)?;
    let snapshot: Option<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT seq, state FROM canvas_snapshots WHERE canvas_id = $1")
            .bind(canvas_id)
            .fetch_optional(&mut *tx)
            .await?;
    let since = snapshot.as_ref().map_or(0, |s| s.0);
    let doc = Doc::new();
    if let Some((_, sealed)) = &snapshot {
        apply_sealed(&doc, &key, canvas_id, sealed)?;
    }
    // Streamed: only the document itself is held in memory.
    let mut rows = sqlx::query_scalar::<_, Vec<u8>>(
        r#"SELECT "update" FROM canvas_updates WHERE canvas_id = $1 AND seq > $2 ORDER BY seq"#,
    )
    .bind(canvas_id)
    .bind(since)
    .fetch(&mut *tx);
    while let Some(sealed) = rows.try_next().await? {
        apply_sealed(&doc, &key, canvas_id, &sealed)?;
    }
    drop(rows);
    tx.commit().await?;
    Ok(Some(doc))
}

fn apply_sealed(doc: &Doc, key: &[u8; 32], canvas_id: Uuid, sealed: &[u8]) -> Result<()> {
    let plain = store::open(key, canvas_id, sealed)?;
    let update = Update::decode_v1(&plain)
        .map_err(|e| Error::Api(ApiError::internal(format!("stored update: {e}"))))?;
    doc.transact_mut()
        .apply_update(update)
        .map_err(|e| Error::Api(ApiError::internal(format!("stored update: {e}"))))?;
    Ok(())
}

/// The digest (CONTRACT §6.3) of the stored document of `canvas_id`.
pub async fn stored_digest(state: &SharedState, canvas_id: Uuid) -> Result<Option<String>> {
    Ok(load_doc(state, canvas_id)
        .await?
        .map(|doc| document_digest(&doc)))
}

/// `GET /v1/apps/self/canvases/{id}/digest`: `{"canvas_id","digest"}` of the stored document
/// (reloaded from the database), for canvases this app may manage.
pub async fn digest(
    State(state): State<SharedState>,
    app: AppAuth,
    Path(raw): Path<String>,
) -> Result<Json<Value>> {
    let id = parse_id(&raw)?;
    if !is_app_canvas(&state, app.app_id, id).await? {
        return Err(Error::Api(ApiError::NotFound));
    }
    let digest = stored_digest(&state, id)
        .await?
        .ok_or(Error::Api(ApiError::NotFound))?;
    Ok(Json(json!({ "canvas_id": id, "digest": digest })))
}
