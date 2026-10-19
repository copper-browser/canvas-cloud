//! `/v1/apps/self/users*`: provisioning an app's users (CONTRACT §5.3).

use axum::extract::{Path, State};
use axum::Json;
use canvas_cloud_core::error::{is_unique_violation, ApiError};
use canvas_cloud_core::extract::{JsonBody, QueryParams};
use canvas_cloud_core::identity::{self, AppAuth, NewUser};
use canvas_cloud_core::ids;
use canvas_cloud_core::state::SharedState;
use canvas_cloud_engine::admin;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::common::{
    check_subject, clean_display_name, display_name_json, Error, ListQuery, Result,
};
use crate::directory;

/// Most users one `users:batch` request may carry.
pub const MAX_BATCH: usize = 1000;
/// Request body limits.
const UPSERT_BODY_LIMIT: usize = 16 * 1024;
const BATCH_BODY_LIMIT: usize = 4 * 1024 * 1024;

/// The audit `actor` of an app's own actions.
pub fn actor(app: &AppAuth) -> String {
    format!("app:{}", app.app_id)
}

// ---------------------------------------------------------------------------------------------
// Upsert

/// Absent → `None`; `null` → `Some(None)`; a value → `Some(Some(v))`.
#[allow(clippy::option_option)] // absent vs null is the point
fn double_option<'de, D, T>(de: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

/// `PUT /v1/apps/self/users/{subject}` body (and a batch item without its `subject`).
#[derive(Debug, Deserialize)]
pub struct UpsertBody {
    pub email: String,
    /// Absent: unchanged (`""` for a new user); `null`/blank: no display name.
    #[allow(clippy::option_option)]
    #[serde(default, deserialize_with = "double_option")]
    pub display_name: Option<Option<String>>,
    /// Absent: unchanged (`false` for a new user).
    #[serde(default)]
    pub disabled: Option<bool>,
    /// Preferred id of a new user: used when it is a UUID and free, else a fresh `UUIDv7`.
    #[serde(default)]
    pub id: Option<Value>,
}

/// One provisioned user, as `PUT` and `users:batch` report it.
#[derive(Debug)]
pub struct Upserted {
    pub user_id: Uuid,
    pub subject: String,
    pub email: String,
    pub display_name: String,
    pub disabled: bool,
    pub created: bool,
}

impl Upserted {
    fn json(&self) -> Value {
        json!({
            "user_id": self.user_id,
            "subject": self.subject,
            "email": self.email,
            "display_name": display_name_json(&self.display_name),
            "disabled": self.disabled,
            "created": self.created,
        })
    }
}

fn email_taken(message: &str) -> Error {
    Error::conflict("email_taken", message)
}

struct Clean {
    email: String,
    display_name: Option<String>,
    disabled: Option<bool>,
    id_hint: Option<Uuid>,
}

fn clean(body: UpsertBody) -> Result<Clean> {
    let email = identity::normalize_email(&body.email)?;
    Ok(Clean {
        email,
        display_name: body.display_name.map(|n| clean_display_name(n.as_deref())),
        disabled: body.disabled,
        id_hint: body
            .id
            .as_ref()
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .filter(|id| !id.is_nil()),
    })
}

/// Upsert `subject` of `app` (one transaction; retried once on a concurrent insert).
pub async fn upsert(
    state: &SharedState,
    app: &AppAuth,
    subject: &str,
    body: UpsertBody,
) -> Result<Upserted> {
    let subject = check_subject(subject)?.to_owned();
    let c = clean(body)?;
    let mut attempt = 0;
    let (out, was_disabled) = loop {
        attempt += 1;
        let mut tx = state.db.begin().await?;
        match upsert_tx(&mut tx, app, &subject, &c).await {
            Ok(done) => {
                tx.commit().await.map_err(map_unique)?;
                break done;
            }
            Err(e) if attempt == 1 && retryable(&e) => {
                // A concurrent upsert of the same subject or email may have won; look again
                // (a real conflict fails the same way the second time).
                drop(tx);
            }
            Err(e) => return Err(map_internal_unique(e)),
        }
    };
    directory::linked(app.app_id, out.user_id, &out.subject);
    if out.disabled && !was_disabled {
        admin::disconnect_user(out.user_id);
    }
    if out.created {
        identity::audit(
            &state.db,
            &actor(app),
            "app.user.create",
            &out.user_id.to_string(),
            json!({ "subject": out.subject, "email": out.email }),
        )
        .await;
    }
    Ok(out)
}

fn retryable(e: &Error) -> bool {
    match e {
        Error::Api(ApiError::Internal(inner)) => inner
            .downcast_ref::<sqlx::Error>()
            .is_some_and(is_unique_violation),
        Error::Coded { code, .. } => *code == "email_taken",
        Error::Api(_) => false,
    }
}

fn map_unique(e: sqlx::Error) -> Error {
    if is_unique_violation(&e) {
        email_taken("another account already uses this email")
    } else {
        e.into()
    }
}

fn map_internal_unique(e: Error) -> Error {
    match e {
        Error::Api(ApiError::Internal(inner))
            if inner
                .downcast_ref::<sqlx::Error>()
                .is_some_and(is_unique_violation) =>
        {
            email_taken("another account already uses this email")
        }
        other => other,
    }
}

/// `(upserted, was disabled before)`.
async fn upsert_tx(
    tx: &mut PgConnection,
    app: &AppAuth,
    subject: &str,
    c: &Clean,
) -> Result<(Upserted, bool)> {
    let linked: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM app_users WHERE app_id = $1 AND subject = $2 FOR UPDATE",
    )
    .bind(app.app_id)
    .bind(subject)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(user_id) = linked {
        let (out, was) = update_user(tx, user_id, subject, c, true).await?;
        return Ok((out, was));
    }
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM users WHERE lower(email) = lower($1) FOR UPDATE")
            .bind(&c.email)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(user_id) = existing {
        let other: Option<String> =
            sqlx::query_scalar("SELECT subject FROM app_users WHERE app_id = $1 AND user_id = $2")
                .bind(app.app_id)
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await?;
        if other.is_some() {
            return Err(email_taken(
                "the account with this email is already linked to another subject of this app",
            ));
        }
        if !app.trust_email {
            return Err(email_taken(
                "an account with this email already exists (this app does not link existing accounts by email)",
            ));
        }
        link(tx, app.app_id, subject, user_id).await?;
        // The existing account keeps its email (equal ignoring case).
        return update_user(tx, user_id, subject, c, false).await;
    }
    let id = match c.id_hint {
        Some(hint) => {
            let taken: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE id = $1)")
                    .bind(hint)
                    .fetch_one(&mut *tx)
                    .await?;
            (!taken).then_some(hint)
        }
        None => None,
    };
    let display_name = c.display_name.clone().unwrap_or_default();
    let disabled = c.disabled.unwrap_or(false);
    let user_id = identity::insert_user(
        &mut *tx,
        &NewUser {
            id: Some(id.unwrap_or_else(ids::uuid_v7)),
            email: &c.email,
            display_name: Some(&display_name),
            password_hash: None,
            disabled,
        },
    )
    .await
    .map_err(|e| match e {
        ApiError::Conflict(_) => email_taken("another account already uses this email"),
        other => other.into(),
    })?;
    link(tx, app.app_id, subject, user_id).await?;
    Ok((
        Upserted {
            user_id,
            subject: subject.to_owned(),
            email: c.email.clone(),
            display_name,
            disabled,
            created: true,
        },
        false,
    ))
}

async fn link(tx: &mut PgConnection, app_id: Uuid, subject: &str, user_id: Uuid) -> Result<()> {
    sqlx::query("INSERT INTO app_users (app_id, subject, user_id) VALUES ($1, $2, $3)")
        .bind(app_id)
        .bind(subject)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Applies `c` to an existing user (`set_email`: also change the email).
async fn update_user(
    tx: &mut PgConnection,
    user_id: Uuid,
    subject: &str,
    c: &Clean,
    set_email: bool,
) -> Result<(Upserted, bool)> {
    let (email, display_name, disabled): (String, String, bool) =
        sqlx::query_as("SELECT email, display_name, disabled FROM users WHERE id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
    let new_email = if set_email && c.email != email {
        if !c.email.eq_ignore_ascii_case(&email) {
            let taken: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM users WHERE lower(email) = lower($1) AND id <> $2)",
            )
            .bind(&c.email)
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
            if taken {
                return Err(email_taken("another account already uses this email"));
            }
        }
        c.email.clone()
    } else {
        email.clone()
    };
    let new_name = c
        .display_name
        .clone()
        .unwrap_or_else(|| display_name.clone());
    let new_disabled = c.disabled.unwrap_or(disabled);
    if new_email != email || new_name != display_name || new_disabled != disabled {
        sqlx::query(
            "UPDATE users SET email = $2, display_name = $3, disabled = $4, updated_at = now()
             WHERE id = $1",
        )
        .bind(user_id)
        .bind(&new_email)
        .bind(&new_name)
        .bind(new_disabled)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                email_taken("another account already uses this email")
            } else {
                e.into()
            }
        })?;
    }
    Ok((
        Upserted {
            user_id,
            subject: subject.to_owned(),
            email: new_email,
            display_name: new_name,
            disabled: new_disabled,
            created: false,
        },
        disabled,
    ))
}

pub async fn put_user(
    State(state): State<SharedState>,
    app: AppAuth,
    Path(subject): Path<String>,
    JsonBody(body): JsonBody<UpsertBody, UPSERT_BODY_LIMIT>,
) -> Result<Json<Value>> {
    let out = upsert(&state, &app, &subject, body).await?;
    Ok(Json(out.json()))
}

// ---------------------------------------------------------------------------------------------
// Batch

#[derive(Debug, Deserialize)]
pub struct BatchBody {
    pub users: Vec<Value>,
}

/// `POST /v1/apps/self/users:batch`: each item upserted on its own (one transaction each);
/// results in request order.
pub async fn batch(
    State(state): State<SharedState>,
    app: AppAuth,
    JsonBody(body): JsonBody<BatchBody, BATCH_BODY_LIMIT>,
) -> Result<Json<Value>> {
    if body.users.len() > MAX_BATCH {
        return Err(Error::bad_request(format!(
            "at most {MAX_BATCH} users per batch"
        )));
    }
    let mut results = Vec::with_capacity(body.users.len());
    for item in body.users {
        let subject = item.get("subject").cloned().unwrap_or(Value::Null);
        let outcome = match (subject.as_str(), serde_json::from_value::<UpsertBody>(item)) {
            (None, _) => Err(Error::bad_request("subject is required (a string)")),
            (Some(_), Err(e)) => Err(Error::bad_request(format!("invalid user: {e}"))),
            (Some(s), Ok(body)) => upsert(&state, &app, s, body).await,
        };
        results.push(match outcome {
            Ok(out) => {
                let mut v = out.json();
                v["ok"] = Value::Bool(true);
                v
            }
            Err(e) => {
                if matches!(e, Error::Api(ApiError::Internal(_))) {
                    tracing::error!(error = ?e, "users:batch item failed");
                }
                json!({
                    "subject": subject,
                    "ok": false,
                    "error": e.code(),
                    "message": e.message(),
                })
            }
        });
    }
    Ok(Json(json!({ "results": results })))
}

// ---------------------------------------------------------------------------------------------
// List

#[derive(sqlx::FromRow)]
struct UserRow {
    subject: String,
    user_id: Uuid,
    email: String,
    display_name: String,
    disabled: bool,
    linked_at: OffsetDateTime,
    created_at: OffsetDateTime,
    canvas_count: i64,
}

fn rfc3339(t: OffsetDateTime) -> Value {
    t.format(&time::format_description::well_known::Rfc3339)
        .map_or(Value::Null, Value::from)
}

/// `GET /v1/apps/self/users?limit&offset`: this app's users (oldest link first) with the
/// number of canvases each is a member of.
pub async fn list(
    State(state): State<SharedState>,
    app: AppAuth,
    QueryParams(q): QueryParams<ListQuery>,
) -> Result<Json<Value>> {
    let (limit, offset) = q.page();
    let rows = sqlx::query_as::<_, UserRow>(
        "SELECT au.subject, u.id AS user_id, u.email, u.display_name, u.disabled,
                au.created_at AS linked_at, u.created_at,
                (SELECT count(*) FROM canvas_members m WHERE m.user_id = u.id) AS canvas_count
         FROM app_users au JOIN users u ON u.id = au.user_id
         WHERE au.app_id = $1
         ORDER BY au.created_at, au.subject
         LIMIT $2 OFFSET $3",
    )
    .bind(app.app_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM app_users WHERE app_id = $1")
        .bind(app.app_id)
        .fetch_one(&state.db)
        .await?;
    let users: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "subject": r.subject,
                "user_id": r.user_id,
                "email": r.email,
                "display_name": display_name_json(&r.display_name),
                "disabled": r.disabled,
                "canvas_count": r.canvas_count,
                "linked_at": rfc3339(r.linked_at),
                "created_at": rfc3339(r.created_at),
            })
        })
        .collect();
    Ok(Json(json!({
        "users": users,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

// ---------------------------------------------------------------------------------------------
// Delete / disconnect

/// The user linked as `subject`, or `404`.
async fn linked_user(state: &SharedState, app: &AppAuth, subject: &str) -> Result<Uuid> {
    sqlx::query_scalar("SELECT user_id FROM app_users WHERE app_id = $1 AND subject = $2")
        .bind(app.app_id)
        .bind(subject)
        .fetch_optional(&state.db)
        .await?
        .ok_or(Error::Api(ApiError::NotFound))
}

/// `DELETE /v1/apps/self/users/{subject}`.
///
/// A **pure app user** — no password, no other app link, no session and no personal access
/// token — is deleted with everything they own, exactly like `canvas-cloud admin users delete`:
/// their canvases are deleted (rooms closed with 4404,
/// members get a `deleted` event), invites addressed to their email are removed, their
/// memberships and mentions go with the account. Anyone else is only unlinked from this app
/// (the account and its canvases stay). Either way their live sockets are disconnected.
pub async fn delete(
    State(state): State<SharedState>,
    app: AppAuth,
    Path(subject): Path<String>,
) -> Result<Json<Value>> {
    let user_id = linked_user(&state, &app, &subject).await?;
    let pure: bool = sqlx::query_scalar(
        "SELECT u.password_hash IS NULL
            AND NOT EXISTS (SELECT 1 FROM app_users a WHERE a.user_id = u.id AND a.app_id <> $2)
            AND NOT EXISTS (SELECT 1 FROM sessions s WHERE s.user_id = u.id)
            AND NOT EXISTS (SELECT 1 FROM access_tokens t WHERE t.user_id = u.id)
         FROM users u WHERE u.id = $1",
    )
    .bind(user_id)
    .bind(app.app_id)
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if !pure {
        let n = sqlx::query("DELETE FROM app_users WHERE app_id = $1 AND subject = $2")
            .bind(app.app_id)
            .bind(&subject)
            .execute(&state.db)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::Api(ApiError::NotFound));
        }
        directory::unlinked(app.app_id, user_id);
        admin::disconnect_user(user_id);
        identity::audit(
            &state.db,
            &actor(&app),
            "app.user.unlink",
            &user_id.to_string(),
            json!({ "subject": subject }),
        )
        .await;
        return Ok(Json(json!({
            "ok": true,
            "subject": subject,
            "user_id": user_id,
            "deleted": false,
            "canvases_deleted": 0,
        })));
    }
    let owned = admin::owned_canvases(&state, user_id).await?;
    let mut tx = state.db.begin().await?;
    let email: Option<String> = sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(email) = email else {
        return Err(Error::Api(ApiError::NotFound));
    };
    sqlx::query("DELETE FROM canvas_invites WHERE lower(email) = lower($1)")
        .bind(&email)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    directory::unlinked(app.app_id, user_id);
    admin::canvases_deleted(&state, &owned);
    admin::disconnect_user(user_id);
    identity::audit(
        &state.db,
        &actor(&app),
        "app.user.delete",
        &user_id.to_string(),
        json!({ "subject": subject, "email": email, "canvases": owned.len() }),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "subject": subject,
        "user_id": user_id,
        "deleted": true,
        "canvases_deleted": owned.len(),
    })))
}

/// `POST /v1/apps/self/users/{subject}/disconnect`: close the user's live canvas sockets.
pub async fn disconnect(
    State(state): State<SharedState>,
    app: AppAuth,
    Path(subject): Path<String>,
) -> Result<Json<Value>> {
    let user_id = linked_user(&state, &app, &subject).await?;
    admin::disconnect_user(user_id);
    Ok(Json(
        json!({ "ok": true, "subject": subject, "user_id": user_id }),
    ))
}
