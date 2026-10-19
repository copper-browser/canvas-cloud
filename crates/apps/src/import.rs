//! `POST /v1/apps/self/import[?mode=create|replace]`: import format v1 (CONTRACT §6; the
//! full wire description is `docs/import-format.md`).
//!
//! The body is one canvas as NDJSON, read as a stream: rows (members, invites, links,
//! mentions) are inserted inside one database transaction as their lines arrive, and updates
//! are applied to an in-memory `yrs::Doc` — so apart from that document, memory stays bounded
//! by one line (`limits.import_max_line_bytes`), whatever the body size
//! (`limits.import_max_bytes`). After the `end` line the document is stored as **one**
//! compacted snapshot sealed under a fresh doc key, and the transaction commits.

use std::collections::{HashMap, HashSet};

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use bytes::BytesMut;
use canvas_cloud_core::crypto::Crypto;
use canvas_cloud_core::error::{is_unique_violation, ApiError};
use canvas_cloud_core::events::Event;
use canvas_cloud_core::extract::QueryParams;
use canvas_cloud_core::identity::{self, AppAuth};
use canvas_cloud_core::ids;
use canvas_cloud_core::state::SharedState;
use canvas_cloud_engine::mentions::{EXCERPT_AAD_PREFIX, MAX_EXCERPT_CHARS, MAX_MESSAGE_ID_CHARS};
use canvas_cloud_engine::{admin, store};
use futures::{Stream, StreamExt as _};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use yrs::updates::decoder::Decode as _;
use yrs::{Doc, ReadTxn as _, StateVector, Transact as _, Update};

use crate::canvases::{is_app_canvas, load_doc, stored_digest};
use crate::common::{Error, Result};
use crate::digest::{canonical_json, digest_txn};
use crate::users::actor;

/// The only import format version.
pub const VERSION: u32 = 1;

// ---------------------------------------------------------------------------------------------
// Errors

fn unprocessable(code: &'static str, line: u64, message: impl std::fmt::Display) -> Error {
    Error::coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        code,
        format!("line {line}: {message}"),
    )
    .with("line", line)
}

fn bad_line(line: u64, message: impl std::fmt::Display) -> Error {
    unprocessable("bad_line", line, message)
}

fn too_large(message: String) -> Error {
    Error::coded(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", message)
}

// ---------------------------------------------------------------------------------------------
// NDJSON reader

/// Splits a request body into lines with a per-line and a total byte limit.
pub struct Lines<S> {
    stream: S,
    buf: BytesMut,
    /// Bytes of `buf` already searched for a newline.
    scanned: usize,
    total: usize,
    max_total: usize,
    max_line: usize,
    /// Number of the last line returned (1-based).
    pub line: u64,
    eof: bool,
}

impl<S, E> Lines<S>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    pub fn new(stream: S, max_total: usize, max_line: usize) -> Self {
        Self {
            stream,
            buf: BytesMut::new(),
            scanned: 0,
            total: 0,
            max_total,
            max_line,
            line: 0,
            eof: false,
        }
    }

    /// The next line (without `\n` / `\r\n`), `None` at the end of the body.
    pub async fn next(&mut self) -> Result<Option<BytesMut>> {
        loop {
            if let Some(pos) = memchr(&self.buf[self.scanned..]).map(|p| p + self.scanned) {
                let mut line = self.buf.split_to(pos + 1);
                self.scanned = 0;
                line.truncate(pos);
                if line.last() == Some(&b'\r') {
                    line.truncate(pos - 1);
                }
                self.line += 1;
                if line.len() > self.max_line {
                    return Err(self.line_too_long(self.line));
                }
                return Ok(Some(line));
            }
            self.scanned = self.buf.len();
            if self.buf.len() > self.max_line + 1 {
                return Err(self.line_too_long(self.line + 1));
            }
            if self.eof {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                // A last line without a trailing newline.
                self.line += 1;
                self.scanned = 0;
                let mut line = self.buf.split();
                if line.last() == Some(&b'\r') {
                    line.truncate(line.len() - 1);
                }
                if line.len() > self.max_line {
                    return Err(self.line_too_long(self.line));
                }
                return Ok(Some(line));
            }
            match self.stream.next().await {
                None => self.eof = true,
                Some(Err(e)) => {
                    return Err(Error::bad_request(format!(
                        "reading the request body failed: {e}"
                    )))
                }
                Some(Ok(chunk)) => {
                    self.total = self.total.saturating_add(chunk.len());
                    if self.total > self.max_total {
                        return Err(too_large(format!(
                            "the import body exceeds limits.import_max_bytes ({} bytes)",
                            self.max_total
                        )));
                    }
                    self.buf.extend_from_slice(&chunk);
                }
            }
        }
    }

    fn line_too_long(&self, line: u64) -> Error {
        too_large(format!(
            "line {line} exceeds limits.import_max_line_bytes ({} bytes)",
            self.max_line
        ))
        .with("line", line)
    }

    /// Reads and discards the rest of the body (up to the total limit), so an early error
    /// response reaches a client that is still sending.
    pub async fn drain(&mut self) {
        self.buf.clear();
        self.scanned = 0;
        while !self.eof {
            match self.stream.next().await {
                Some(Ok(chunk)) => {
                    self.total = self.total.saturating_add(chunk.len());
                    if self.total > self.max_total {
                        return;
                    }
                }
                Some(Err(_)) | None => self.eof = true,
            }
        }
    }
}

fn memchr(hay: &[u8]) -> Option<usize> {
    hay.iter().position(|b| *b == b'\n')
}

// ---------------------------------------------------------------------------------------------
// Wire

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Line {
    Canvas {
        version: u32,
        id: Uuid,
        name: String,
        kind: String,
        owner: String,
        #[serde(with = "time::serde::rfc3339")]
        created_at: OffsetDateTime,
        #[serde(with = "time::serde::rfc3339")]
        updated_at: OffsetDateTime,
    },
    Member {
        subject: String,
        role: String,
        #[serde(with = "time::serde::rfc3339")]
        added_at: OffsetDateTime,
    },
    Invite {
        id: Uuid,
        email: String,
        #[serde(default)]
        invited_by: Option<String>,
        status: String,
        #[serde(with = "time::serde::rfc3339")]
        created_at: OffsetDateTime,
        #[serde(default, with = "time::serde::rfc3339::option")]
        accepted_at: Option<OffsetDateTime>,
        #[serde(default, with = "time::serde::rfc3339::option")]
        nudged_at: Option<OffsetDateTime>,
        token_sha256_hex: String,
    },
    Link {
        id: Uuid,
        token_sha256_hex: String,
        role: String,
        #[serde(default)]
        created_by: Option<String>,
        #[serde(with = "time::serde::rfc3339")]
        created_at: OffsetDateTime,
        uses: i64,
    },
    Mention {
        id: Uuid,
        message_id: String,
        from: String,
        to: String,
        excerpt: String,
        #[serde(with = "time::serde::rfc3339")]
        created_at: OffsetDateTime,
        #[serde(default, with = "time::serde::rfc3339::option")]
        read_at: Option<OffsetDateTime>,
    },
    Update {
        b64: String,
    },
    End {
        updates: u64,
        members: u64,
        invites: u64,
        links: u64,
        mentions: u64,
    },
}

impl Line {
    /// Position in the required line order.
    fn phase(&self) -> u8 {
        match self {
            Self::Canvas { .. } => 0,
            Self::Member { .. } => 1,
            Self::Invite { .. } => 2,
            Self::Link { .. } => 3,
            Self::Mention { .. } => 4,
            Self::Update { .. } => 5,
            Self::End { .. } => 6,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Canvas { .. } => "canvas",
            Self::Member { .. } => "member",
            Self::Invite { .. } => "invite",
            Self::Link { .. } => "link",
            Self::Mention { .. } => "mention",
            Self::Update { .. } => "update",
            Self::End { .. } => "end",
        }
    }
}

const ORDER: &str = "canvas, member…, invite…, link…, mention…, update…, end";

fn parse_line(line: u64, bytes: &[u8]) -> Result<Line> {
    serde_json::from_slice(bytes).map_err(|e| {
        bad_line(
            line,
            canvas_cloud_core::error::loggable_message(&format!("not a valid import line: {e}")),
        )
    })
}

fn token_hex(line: u64, raw: &str) -> Result<String> {
    let t = raw.trim().to_ascii_lowercase();
    if t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(t)
    } else {
        Err(bad_line(line, "token_sha256_hex must be 64 hex digits"))
    }
}

// ---------------------------------------------------------------------------------------------
// Handler

#[derive(Debug, Default, Deserialize)]
pub struct ImportQuery {
    pub mode: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Create,
    Replace,
}

/// What the response reports.
#[derive(Debug, Default)]
struct Counts {
    updates: u64,
    members: u64,
    invites: u64,
    links: u64,
    mentions: u64,
}

pub async fn import(
    State(state): State<SharedState>,
    app: AppAuth,
    QueryParams(q): QueryParams<ImportQuery>,
    headers: axum::http::HeaderMap,
    body: Body,
) -> Result<Json<Value>> {
    // A body declared too large is refused before reading any of it.
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let max = state.cfg.limits.import_max_bytes;
    if app.allow_import && declared.is_some_and(|n| n > max as u64) {
        return Err(too_large(format!(
            "the body is larger than the import limit of {max} bytes"
        )));
    }
    let mut lines = Lines::new(
        body.into_data_stream(),
        state.cfg.limits.import_max_bytes,
        state.cfg.limits.import_max_line_bytes,
    );
    let result = run(&state, &app, q.mode.as_deref(), &mut lines).await;
    if let Err(e) = &result {
        if e.status() != StatusCode::PAYLOAD_TOO_LARGE
            && !matches!(e, Error::Api(ApiError::BadRequest(m)) if m.starts_with("reading the request body"))
        {
            lines.drain().await;
        }
    }
    result.map(Json)
}

/// The canvas line, validated.
struct Head {
    id: Uuid,
    name: String,
    kind: String,
    owner_subject: String,
    owner: Uuid,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

/// What the import replaces, decided before the transaction (and re-checked inside it).
#[derive(Default)]
struct Plan {
    /// A canvas with the same id, imported earlier by this app (`mode=replace`).
    same_id: bool,
    /// The owner's other Personal canvas, deleted by the import.
    personal: Option<Uuid>,
    /// … and whether it had content (then the status is `replaced`).
    personal_has_content: bool,
}

/// Resolves subjects of this app to user ids (cached for the request).
struct Subjects<'a> {
    state: &'a SharedState,
    app_id: Uuid,
    cache: HashMap<String, Uuid>,
}

impl Subjects<'_> {
    async fn resolve(&mut self, line: u64, subject: &str) -> Result<Uuid> {
        if let Some(id) = self.cache.get(subject) {
            return Ok(*id);
        }
        let id: Option<Uuid> =
            sqlx::query_scalar("SELECT user_id FROM app_users WHERE app_id = $1 AND subject = $2")
                .bind(self.app_id)
                .bind(subject)
                .fetch_optional(&self.state.db)
                .await?;
        let id = id.ok_or_else(|| {
            unprocessable(
                "unknown_subject",
                line,
                format!(
                    "subject {} is not provisioned for this app (PUT /v1/apps/self/users/{{subject}})",
                    serde_json::to_string(subject).unwrap_or_default()
                ),
            )
            .with("subject", subject)
        })?;
        self.cache.insert(subject.to_owned(), id);
        Ok(id)
    }

    async fn resolve_opt(&mut self, line: u64, subject: Option<&str>) -> Result<Option<Uuid>> {
        match subject {
            Some(s) => Ok(Some(self.resolve(line, s).await?)),
            None => Ok(None),
        }
    }
}

async fn run<S, E>(
    state: &SharedState,
    app: &AppAuth,
    mode: Option<&str>,
    lines: &mut Lines<S>,
) -> Result<Value>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    if !app.allow_import {
        return Err(Error::Api(ApiError::Denied {
            code: "import_not_allowed",
            message:
                "this app may not import canvases (canvas-cloud apps set <id> --allow-import true)",
        }));
    }
    let mode = match mode.map(str::trim) {
        None | Some("" | "create") => Mode::Create,
        Some("replace") => Mode::Replace,
        Some(_) => return Err(Error::bad_request("mode must be create or replace")),
    };
    let mut subjects = Subjects {
        state,
        app_id: app.app_id,
        cache: HashMap::new(),
    };
    let head = read_head(lines, &mut subjects).await?;
    let plan = plan(state, app, mode, &head).await?;
    let (body, deleted) = if plan.same_id {
        admin::with_room_shut(
            head.id,
            store_canvas(state, app, &head, &plan, lines, &mut subjects),
        )
        .await?
    } else {
        store_canvas(state, app, &head, &plan, lines, &mut subjects).await?
    };
    // Committed: tell everyone involved.
    admin::canvases_deleted(state, &deleted.personal);
    for user in &deleted.same_id_members_gone {
        state.events.publish(
            *user,
            Event::Canvas {
                canvas_id: head.id,
                kind: "deleted".into(),
            },
        );
    }
    for user in &deleted.members {
        state.events.publish(
            *user,
            Event::Canvas {
                canvas_id: head.id,
                kind: "created".into(),
            },
        );
    }
    identity::audit(
        &state.db,
        &actor(app),
        "app.canvas.import",
        &head.id.to_string(),
        json!({
            "status": body["status"],
            "kind": head.kind,
            "owner": head.owner_subject,
            "replaced_personal": plan.personal,
            "digest": body["digest"],
        }),
    )
    .await;
    Ok(body)
}

async fn read_head<S, E>(lines: &mut Lines<S>, subjects: &mut Subjects<'_>) -> Result<Head>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let (n, line) = loop {
        let Some(raw) = lines.next().await? else {
            return Err(bad_line(
                lines.line.max(1),
                "the body is empty (expected a canvas line)",
            ));
        };
        if !raw.iter().all(u8::is_ascii_whitespace) {
            break (lines.line, parse_line(lines.line, &raw)?);
        }
    };
    let Line::Canvas {
        version,
        id,
        name,
        kind,
        owner,
        created_at,
        updated_at,
    } = line
    else {
        return Err(bad_line(
            n,
            format!("expected the canvas line first, got {}", line.name()),
        ));
    };
    if version != VERSION {
        return Err(bad_line(
            n,
            format!("unsupported import version {version} (this server reads version {VERSION})"),
        ));
    }
    if id.is_nil() {
        return Err(bad_line(n, "the canvas id must not be the nil UUID"));
    }
    let chars = name.chars().count();
    if name.trim().is_empty() || chars > 200 {
        return Err(bad_line(n, "the canvas name must be 1-200 characters"));
    }
    if !matches!(kind.as_str(), "personal" | "shared") {
        return Err(bad_line(n, "kind must be personal or shared"));
    }
    let owner_id = subjects.resolve(n, &owner).await?;
    Ok(Head {
        id,
        name,
        kind,
        owner_subject: owner,
        owner: owner_id,
        created_at,
        updated_at,
    })
}

/// Checks the conflicts (`409`s) before reading the rest of the body.
async fn plan(state: &SharedState, app: &AppAuth, mode: Mode, head: &Head) -> Result<Plan> {
    let mut plan = Plan::default();
    let existing: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT imported_by_app FROM canvases WHERE id = $1")
            .bind(head.id)
            .fetch_optional(&state.db)
            .await?;
    if let Some(imported_by) = existing {
        match mode {
            Mode::Create => return Err(canvas_exists(state, app, head.id).await),
            Mode::Replace if imported_by == Some(app.app_id) => plan.same_id = true,
            Mode::Replace => {
                return Err(Error::conflict(
                    "not_replaceable",
                    "a canvas with this id exists and was not imported by this app; it cannot be replaced",
                ))
            }
        }
    }
    if head.kind == "personal" {
        let other: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM canvases WHERE owner_id = $1 AND kind = 'personal' AND id <> $2",
        )
        .bind(head.owner)
        .bind(head.id)
        .fetch_optional(&state.db)
        .await?;
        if let Some(other) = other {
            let has_content = !personal_is_empty(state, other).await?;
            if has_content && mode == Mode::Create {
                return Err(Error::conflict(
                    "personal_exists",
                    "the owner already has a Personal canvas with content (import with mode=replace to replace it)",
                )
                .with("canvas_id", other.to_string()));
            }
            plan.personal = Some(other);
            plan.personal_has_content = has_content;
        }
    }
    Ok(plan)
}

/// `409 canvas_exists`, with the stored digest when the canvas is this app's.
async fn canvas_exists(state: &SharedState, app: &AppAuth, id: Uuid) -> Error {
    let mut err = Error::conflict("canvas_exists", "a canvas with this id already exists");
    if let Ok(true) = is_app_canvas(state, app.app_id, id).await {
        match stored_digest(state, id).await {
            Ok(Some(d)) => err = err.with("digest", d),
            Ok(None) => {}
            Err(e) => tracing::warn!(canvas_id = %id, error = ?e, "digest of an existing canvas"),
        }
    }
    err
}

/// An auto-created Personal canvas nobody used: no members besides the owner, no invites,
/// links or mentions, and nothing in the document but its `meta`.
async fn personal_is_empty(state: &SharedState, id: Uuid) -> Result<bool> {
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM canvas_members m JOIN canvases c ON c.id = m.canvas_id
                        WHERE m.canvas_id = $1 AND m.user_id <> c.owner_id)
             OR EXISTS (SELECT 1 FROM canvas_invites WHERE canvas_id = $1)
             OR EXISTS (SELECT 1 FROM canvas_share_links WHERE canvas_id = $1)
             OR EXISTS (SELECT 1 FROM canvas_mentions WHERE canvas_id = $1)",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    if used {
        return Ok(false);
    }
    let Some(doc) = load_doc(state, id).await? else {
        return Ok(true);
    };
    let json = canonical_json(&doc.transact());
    let roots: serde_json::Map<String, Value> =
        serde_json::from_str(&json).map_err(|e| Error::Api(ApiError::internal(e)))?;
    Ok(roots.keys().all(|k| k == "meta"))
}

/// Who to notify after the commit.
#[derive(Default)]
struct Deleted {
    /// The replaced Personal canvas with its members (room closed with 4404, `deleted`).
    personal: Vec<(Uuid, Vec<Uuid>)>,
    /// Members of the replaced same-id canvas that are not members of the import.
    same_id_members_gone: Vec<Uuid>,
    /// Members of the imported canvas (`created`).
    members: Vec<Uuid>,
}

async fn member_ids(tx: &mut PgConnection, canvas_id: Uuid) -> Result<Vec<Uuid>> {
    Ok(
        sqlx::query_scalar("SELECT user_id FROM canvas_members WHERE canvas_id = $1")
            .bind(canvas_id)
            .fetch_all(&mut *tx)
            .await?,
    )
}

/// Inside the import transaction: re-checks the plan under row locks (a concurrent import or
/// delete may have raced it) and deletes what the import replaces. Returns the replaced
/// Personal canvas (in `Deleted::personal`) and the members of a replaced same-id canvas.
async fn clear_way(
    tx: &mut PgConnection,
    state: &SharedState,
    app: &AppAuth,
    head: &Head,
    plan: &Plan,
) -> Result<(Deleted, Vec<Uuid>)> {
    let mut deleted = Deleted::default();
    let mut old_members: Vec<Uuid> = Vec::new();
    // Re-check the plan under row locks (a concurrent import or delete may have raced it).
    let existing: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT imported_by_app FROM canvases WHERE id = $1 FOR UPDATE")
            .bind(head.id)
            .fetch_optional(&mut *tx)
            .await?;
    match existing {
        Some(by) if plan.same_id && by == Some(app.app_id) => {
            old_members = member_ids(tx, head.id).await?;
            sqlx::query("DELETE FROM canvases WHERE id = $1")
                .bind(head.id)
                .execute(&mut *tx)
                .await?;
        }
        Some(_) => return Err(canvas_exists(state, app, head.id).await),
        None => {}
    }
    if head.kind == "personal" {
        let other: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM canvases WHERE owner_id = $1 AND kind = 'personal' FOR UPDATE",
        )
        .bind(head.owner)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(other) = other {
            if Some(other) != plan.personal {
                return Err(Error::conflict(
                    "personal_exists",
                    "the owner's Personal canvas changed during the import; retry",
                )
                .with("canvas_id", other.to_string()));
            }
            let members = member_ids(tx, other).await?;
            sqlx::query("DELETE FROM canvases WHERE id = $1")
                .bind(other)
                .execute(&mut *tx)
                .await?;
            deleted.personal.push((other, members));
        }
    }
    Ok((deleted, old_members))
}

/// The transaction: replace what the plan says, insert the canvas and its rows as the lines
/// arrive, store one compacted snapshot, commit. Returns the response body.
async fn store_canvas<S, E>(
    state: &SharedState,
    app: &AppAuth,
    head: &Head,
    plan: &Plan,
    lines: &mut Lines<S>,
    subjects: &mut Subjects<'_>,
) -> Result<(Value, Deleted)>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut tx = state.db.begin().await?;
    let (mut deleted, old_members) = clear_way(&mut tx, state, app, head, plan).await?;
    let key = store::new_doc_key();
    insert_head(&mut tx, state, app, head, &key).await?;

    let mut rows = Rows {
        tx: &mut tx,
        head,
        key: &key,
        subjects,
        doc: Doc::new(),
        upper: StateVector::default(),
        counts: Counts::default(),
        members: HashSet::new(),
    };
    let (end_line, expected) = rows.read_body(lines).await?;
    let Rows {
        doc,
        upper,
        counts,
        mut members,
        ..
    } = rows;
    check_counts(end_line, expected, &counts)?;
    let (state_bytes, digest) = {
        let doc = doc.clone();
        tokio::task::spawn_blocking(move || compacted(&doc, &upper, end_line))
            .await
            .map_err(|e| Error::Api(ApiError::internal(format!("compacting the import: {e}"))))??
    };
    drop(doc);
    if !members.contains(&head.owner) {
        sqlx::query(
            "INSERT INTO canvas_members (canvas_id, user_id, role, added_at)
             VALUES ($1, $2, 'owner', $3)",
        )
        .bind(head.id)
        .bind(head.owner)
        .bind(head.created_at)
        .execute(&mut *tx)
        .await?;
        members.insert(head.owner);
    }
    store::write_snapshot_tx(&mut tx, head.id, &state_bytes, &key).await?;
    tx.commit().await?;

    deleted.same_id_members_gone = old_members
        .into_iter()
        .filter(|u| !members.contains(u))
        .collect();
    deleted.members = members.into_iter().collect();
    let status = if plan.same_id || plan.personal_has_content {
        "replaced"
    } else {
        "created"
    };
    tracing::info!(
        canvas_id = %head.id,
        app_id = %app.app_id,
        status,
        updates = counts.updates,
        bytes = state_bytes.len(),
        "canvas imported"
    );
    Ok((
        json!({
            "canvas_id": head.id,
            "status": status,
            "updates_applied": counts.updates,
            "members": counts.members,
            "invites": counts.invites,
            "links": counts.links,
            "mentions": counts.mentions,
            "digest": digest,
        }),
        deleted,
    ))
}

async fn insert_head(
    tx: &mut PgConnection,
    state: &SharedState,
    app: &AppAuth,
    head: &Head,
    key: &[u8; 32],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO canvases (id, owner_id, name, kind, doc_key_wrapped, created_at, updated_at,
                               imported_by_app, imported_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())",
    )
    .bind(head.id)
    .bind(head.owner)
    .bind(&head.name)
    .bind(&head.kind)
    .bind(store::wrap(&state.crypto, key))
    .bind(head.created_at)
    .bind(head.updated_at)
    .bind(app.app_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            Error::conflict(
                "canvas_exists",
                "a canvas with this id was created concurrently",
            )
        } else {
            e.into()
        }
    })?;
    Ok(())
}

/// `422 count_mismatch` unless the `end` line's counts equal what was received.
fn check_counts(end_line: u64, expected: [u64; 5], counts: &Counts) -> Result<()> {
    let got = [
        counts.updates,
        counts.members,
        counts.invites,
        counts.links,
        counts.mentions,
    ];
    if expected == got {
        return Ok(());
    }
    let names = ["updates", "members", "invites", "links", "mentions"];
    let diffs: Vec<String> = names
        .iter()
        .zip(expected.iter().zip(got.iter()))
        .filter(|(_, (e, g))| e != g)
        .map(|(name, (e, g))| format!("{name}: end says {e}, received {g}"))
        .collect();
    Err(unprocessable("count_mismatch", end_line, diffs.join("; ")))
}

/// The document as one compacted update, and its digest; `422 incomplete_history` when
/// structs are still pending, or when some client's updates have a gap: yrs 0.28 integrates
/// an update that starts past a client's known clock without marking anything pending (its
/// state vector then stops short of what the updates carried), so both are checked.
fn compacted(doc: &Doc, upper: &StateVector, end_line: u64) -> Result<(Vec<u8>, String)> {
    let txn = doc.transact();
    let have = txn.state_vector();
    let gap = upper
        .iter()
        .any(|(client, clock)| have.get(client) < *clock);
    if gap || txn.has_missing_updates() {
        return Err(unprocessable(
            "incomplete_history",
            end_line,
            "the updates leave parts of the document pending (missing updates)",
        ));
    }
    Ok((
        txn.encode_state_as_update_v1(&StateVector::default()),
        digest_txn(&txn),
    ))
}

/// The rows of one import, inserted inside its transaction as their lines arrive.
struct Rows<'t, 's> {
    tx: &'t mut PgConnection,
    head: &'t Head,
    key: &'t [u8; 32],
    subjects: &'t mut Subjects<'s>,
    doc: Doc,
    /// Per client, the highest clock any update reached.
    upper: StateVector,
    counts: Counts,
    members: HashSet<Uuid>,
}

impl Rows<'_, '_> {
    /// Reads every line after the canvas line, in order, through `end` (and checks that
    /// only blank lines follow it). Returns the `end` line's number and counts.
    async fn read_body<S, E>(&mut self, lines: &mut Lines<S>) -> Result<(u64, [u64; 5])>
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        let mut phase = 0_u8;
        let end = loop {
            let Some(raw) = lines.next().await? else {
                return Err(bad_line(
                    lines.line + 1,
                    "the body ended before the end line",
                ));
            };
            let n = lines.line;
            if raw.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let line = parse_line(n, &raw)?;
            drop(raw);
            let p = line.phase();
            if p == 0 || p < phase {
                return Err(bad_line(
                    n,
                    format!("a {} line is out of order (expected: {ORDER})", line.name()),
                ));
            }
            phase = p;
            if let Some(counts) = self.row(n, line).await? {
                break (n, counts);
            }
        };
        while let Some(raw) = lines.next().await? {
            if !raw.iter().all(u8::is_ascii_whitespace) {
                return Err(bad_line(lines.line, "nothing may follow the end line"));
            }
        }
        Ok(end)
    }

    /// Stores one line after the canvas line; `Some(counts)` for the `end` line.
    async fn row(&mut self, n: u64, line: Line) -> Result<Option<[u64; 5]>> {
        match line {
            Line::Canvas { .. } => return Err(bad_line(n, "a second canvas line")),
            Line::Member {
                subject,
                role,
                added_at,
            } => self.member(n, &subject, &role, added_at).await?,
            Line::Invite {
                id,
                email,
                invited_by,
                status,
                created_at,
                accepted_at,
                nudged_at,
                token_sha256_hex,
            } => {
                let invite = Invite {
                    id,
                    email,
                    invited_by,
                    status,
                    created_at,
                    accepted_at,
                    nudged_at,
                    token_sha256_hex,
                };
                self.invite(n, invite).await?;
            }
            Line::Link {
                id,
                token_sha256_hex,
                role,
                created_by,
                created_at,
                uses,
            } => {
                let share_link = Link {
                    id,
                    token_sha256_hex,
                    role,
                    created_by,
                    created_at,
                    uses,
                };
                self.link(n, share_link).await?;
            }
            Line::Mention {
                id,
                message_id,
                from,
                to,
                excerpt,
                created_at,
                read_at,
            } => {
                let mention = Mention {
                    id,
                    message_id,
                    from,
                    to,
                    excerpt,
                    created_at,
                    read_at,
                };
                self.mention(n, mention).await?;
            }
            Line::Update { b64 } => {
                let upper = apply_update(&self.doc, n, b64).await?;
                for (client, clock) in upper.iter() {
                    self.upper.set_max(*client, *clock);
                }
                self.counts.updates += 1;
            }
            Line::End {
                updates,
                members,
                invites,
                links,
                mentions,
            } => return Ok(Some([updates, members, invites, links, mentions])),
        }
        Ok(None)
    }

    async fn member(
        &mut self,
        n: u64,
        subject: &str,
        role: &str,
        added_at: OffsetDateTime,
    ) -> Result<()> {
        let user = self.subjects.resolve(n, subject).await?;
        if user != self.head.owner && self.head.kind == "personal" {
            return Err(bad_line(
                n,
                "a Personal canvas has no members besides its owner",
            ));
        }
        let expected = if user == self.head.owner {
            "owner"
        } else {
            "editor"
        };
        if role != expected {
            return Err(bad_line(
                n,
                format!("member {subject} must have role {expected} (got {role})"),
            ));
        }
        insert(
            self.tx,
            n,
            "a duplicate member",
            sqlx::query(
                "INSERT INTO canvas_members (canvas_id, user_id, role, added_at)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(self.head.id)
            .bind(user)
            .bind(role)
            .bind(added_at),
        )
        .await?;
        self.members.insert(user);
        self.counts.members += 1;
        Ok(())
    }

    async fn invite(&mut self, n: u64, i: Invite) -> Result<()> {
        if self.head.kind == "personal" {
            return Err(bad_line(n, "a Personal canvas has no invites"));
        }
        // Stored lower-case, like the engine's own invites (it matches `email = lower($1)`).
        let email = identity::normalize_email(&i.email)
            .map_err(|_| bad_line(n, "the invite email is not a valid address"))?
            .to_lowercase();
        if !matches!(
            i.status.as_str(),
            "pending" | "accepted" | "declined" | "revoked"
        ) {
            return Err(bad_line(
                n,
                "invite status must be pending, accepted, declined or revoked",
            ));
        }
        let token = token_hex(n, &i.token_sha256_hex)?;
        let by = self
            .subjects
            .resolve_opt(n, i.invited_by.as_deref())
            .await?;
        insert(
            self.tx,
            n,
            "an invite that conflicts with an existing one (same id or token, or a second pending invite for this email)",
            sqlx::query(
                "INSERT INTO canvas_invites (id, canvas_id, email, invited_by, token, status,
                                             created_at, accepted_at, nudged_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(i.id)
            .bind(self.head.id)
            .bind(&email)
            .bind(by)
            .bind(&token)
            .bind(&i.status)
            .bind(i.created_at)
            .bind(i.accepted_at)
            .bind(i.nudged_at),
        )
        .await?;
        self.counts.invites += 1;
        Ok(())
    }

    async fn link(&mut self, n: u64, l: Link) -> Result<()> {
        if self.head.kind == "personal" {
            return Err(bad_line(n, "a Personal canvas has no share links"));
        }
        if l.role != "editor" {
            return Err(bad_line(n, "share link role must be editor"));
        }
        if l.uses < 0 {
            return Err(bad_line(n, "share link uses must not be negative"));
        }
        let digest = hex::decode(token_hex(n, &l.token_sha256_hex)?)
            .map_err(|_| bad_line(n, "token_sha256_hex must be 64 hex digits"))?;
        let by = self
            .subjects
            .resolve_opt(n, l.created_by.as_deref())
            .await?;
        insert(
            self.tx,
            n,
            "a share link whose id or token is already used",
            sqlx::query(
                "INSERT INTO canvas_share_links (id, canvas_id, token_sha256, role,
                                                 created_by, created_at, uses)
                 VALUES ($1, $2, $3, 'editor', $4, $5, $6)",
            )
            .bind(l.id)
            .bind(self.head.id)
            .bind(&digest)
            .bind(by)
            .bind(l.created_at)
            .bind(l.uses),
        )
        .await?;
        self.counts.links += 1;
        Ok(())
    }

    async fn mention(&mut self, n: u64, m: Mention) -> Result<()> {
        let len = m.message_id.chars().count();
        if len == 0 || len > MAX_MESSAGE_ID_CHARS {
            return Err(bad_line(
                n,
                format!("message_id must be 1-{MAX_MESSAGE_ID_CHARS} characters"),
            ));
        }
        if m.excerpt.chars().count() > MAX_EXCERPT_CHARS {
            return Err(bad_line(
                n,
                format!("the excerpt is longer than {MAX_EXCERPT_CHARS} characters"),
            ));
        }
        let from = self.subjects.resolve(n, &m.from).await?;
        let to = self.subjects.resolve(n, &m.to).await?;
        let mut aad = Vec::with_capacity(EXCERPT_AAD_PREFIX.len() + 16);
        aad.extend_from_slice(EXCERPT_AAD_PREFIX);
        aad.extend_from_slice(m.id.as_bytes());
        let sealed = Crypto::seal(self.key, &aad, m.excerpt.as_bytes());
        insert(
            self.tx,
            n,
            "a mention whose id is already used, or a second mention of the same message for the same person",
            sqlx::query(
                "INSERT INTO canvas_mentions (id, canvas_id, message_id, from_user, to_user,
                                              excerpt_sealed, created_at, read_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(m.id)
            .bind(self.head.id)
            .bind(&m.message_id)
            .bind(from)
            .bind(to)
            .bind(&sealed)
            .bind(m.created_at)
            .bind(m.read_at),
        )
        .await?;
        self.counts.mentions += 1;
        Ok(())
    }
}

/// The fields of an `invite` line.
struct Invite {
    id: Uuid,
    email: String,
    invited_by: Option<String>,
    status: String,
    created_at: OffsetDateTime,
    accepted_at: Option<OffsetDateTime>,
    nudged_at: Option<OffsetDateTime>,
    token_sha256_hex: String,
}

/// The fields of a `link` line.
struct Link {
    id: Uuid,
    token_sha256_hex: String,
    role: String,
    created_by: Option<String>,
    created_at: OffsetDateTime,
    uses: i64,
}

/// The fields of a `mention` line.
struct Mention {
    id: Uuid,
    message_id: String,
    from: String,
    to: String,
    excerpt: String,
    created_at: OffsetDateTime,
    read_at: Option<OffsetDateTime>,
}

/// Runs one row insert; a unique violation is the line's fault (`422 bad_line`).
async fn insert(
    tx: &mut PgConnection,
    line: u64,
    what: &str,
    query: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
) -> Result<()> {
    query.execute(&mut *tx).await.map_err(|e| {
        if is_unique_violation(&e) {
            bad_line(line, what)
        } else {
            e.into()
        }
    })?;
    Ok(())
}

/// Decodes and applies one `update` line off the async runtime (an update can be up to
/// `limits.import_max_line_bytes`); returns the highest clock it carries per client. A decoder
/// panic (yrs has a few on malformed input) is the line's fault too: `422 bad_update`, never
/// a dropped connection.
async fn apply_update(doc: &Doc, line: u64, b64: String) -> Result<StateVector> {
    let doc = doc.clone();
    tokio::task::spawn_blocking(move || apply_update_now(&doc, line, &b64))
        .await
        .unwrap_or_else(|_| {
            Err(unprocessable(
                "bad_update",
                line,
                "the update could not be decoded (the Yjs decoder failed on it)",
            ))
        })
}

fn apply_update_now(doc: &Doc, line: u64, b64: &str) -> Result<StateVector> {
    let bytes = ids::b64_decode_any(b64)
        .map_err(|_| unprocessable("bad_update", line, "the update is not valid base64"))?;
    let update = Update::decode_v1(&bytes).map_err(|e| {
        unprocessable(
            "bad_update",
            line,
            format!("the update is not a lib0 v1 Yjs update ({e})"),
        )
    })?;
    // Not `Update::state_vector()`: that stops at the first gap, which is what we look for.
    let mut upper = StateVector::default();
    for (client, ranges) in update.insertions(true).iter() {
        if let Some(end) = ranges.iter().map(|r| r.end).max() {
            upper.set_max(*client, end);
        }
    }
    doc.transact_mut()
        .apply_update(update)
        .map_err(|e| unprocessable("bad_update", line, format!("the update was rejected ({e})")))?;
    Ok(upper)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{Rng as _, SeedableRng as _};

    fn chunked(
        body: &[u8],
        rng: &mut StdRng,
    ) -> futures::stream::Iter<std::vec::IntoIter<std::result::Result<bytes::Bytes, String>>> {
        let mut chunks = Vec::new();
        let mut rest = body;
        while !rest.is_empty() {
            let n = rng.gen_range(1..=rest.len().min(7));
            chunks.push(Ok(bytes::Bytes::copy_from_slice(&rest[..n])));
            rest = &rest[n..];
        }
        futures::stream::iter(chunks)
    }

    async fn all_lines<S>(lines: &mut Lines<S>) -> Result<Vec<(u64, Vec<u8>)>>
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, String>> + Unpin,
    {
        let mut out = Vec::new();
        while let Some(l) = lines.next().await? {
            out.push((lines.line, l.to_vec()));
        }
        Ok(out)
    }

    #[tokio::test]
    async fn lines_split_anywhere_the_same() {
        for seed in 0..200_u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut body = Vec::new();
            let mut expected = Vec::new();
            let n = rng.gen_range(0..12);
            for i in 0..n {
                let len = rng.gen_range(0..20);
                let line: Vec<u8> = (0..len).map(|_| rng.gen_range(b'a'..=b'z')).collect();
                body.extend_from_slice(&line);
                let last = i + 1 == n;
                let newline = !(last && rng.gen_bool(0.3));
                if newline {
                    body.extend_from_slice(if rng.gen_bool(0.5) { b"\r\n" } else { b"\n" });
                }
                // An empty last line without a newline is no line at all.
                if newline || !line.is_empty() {
                    expected.push((i + 1, line));
                }
            }
            let mut lines = Lines::new(chunked(&body, &mut rng), 1 << 20, 64);
            let got = all_lines(&mut lines).await.unwrap();
            let expected: Vec<(u64, Vec<u8>)> = expected;
            assert_eq!(
                got,
                expected,
                "seed {seed}: {:?}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    #[tokio::test]
    async fn line_and_body_limits() {
        let mut rng = StdRng::seed_from_u64(7);
        // Exactly at the line limit is fine (with or without CR).
        let body = b"0123456789\n0123456789\r\nabc".to_vec();
        let mut lines = Lines::new(chunked(&body, &mut rng), 1000, 10);
        assert_eq!(all_lines(&mut lines).await.unwrap().len(), 3);
        // One byte over: 413 naming the line, even if no newline ever comes.
        for body in [b"ok\n01234567890\n".to_vec(), b"ok\n01234567890".to_vec()] {
            let mut lines = Lines::new(chunked(&body, &mut rng), 1000, 10);
            let e = all_lines(&mut lines).await.unwrap_err();
            assert_eq!(e.status(), StatusCode::PAYLOAD_TOO_LARGE);
            assert!(e.message().starts_with("line 2 "), "{}", e.message());
        }
        let body = vec![b'\n'; 101];
        let mut lines = Lines::new(chunked(&body, &mut rng), 100, 10);
        let e = all_lines(&mut lines).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::PAYLOAD_TOO_LARGE);
        // A broken body is a 400.
        let stream = futures::stream::iter(vec![
            Ok(bytes::Bytes::from_static(b"a\nb")),
            Err("reset".to_owned()),
        ]);
        let mut lines = Lines::new(stream, 100, 10);
        assert!(lines.next().await.unwrap().is_some());
        let e = lines.next().await.unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
    }
}
