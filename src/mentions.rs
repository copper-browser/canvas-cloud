//! Chat @mentions on shared canvases (0.6.0).
//!
//! The chat lives in the canvas document ([`crate::chat`]); the server only learns who was
//! mentioned when the sender's Copper reports it:
//!
//! * `POST /canvases/{id}/mentions {message_id, user_ids, excerpt}` — one row per recipient
//!   that is a member of the canvas (the sender excluded), idempotent per
//!   `(canvas, message_id, recipient)`; each new recipient gets a `canvas` event, kind
//!   `mention`.
//! * `GET /mentions?unread=1&limit=n` — the caller's mentions on canvases they can still see,
//!   newest first.
//! * `POST /mentions/read {ids}|{canvas_id}` — marks them read; the caller's other devices get
//!   a `canvas` event, kind `mention_read`, per canvas touched.
//!
//! Excerpts are sealed under the canvas doc key (like the document itself), with AAD
//! [`EXCERPT_AAD_PREFIX`] ‖ the row id.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

use axum::extract::{Path, State};
use axum::Json;
use copper_cloud_core::auth::AuthUser;
use copper_cloud_core::crypto::Crypto;
use copper_cloud_core::error::ApiError;
use copper_cloud_core::extract::QueryParams;
use copper_cloud_core::ids;
use copper_cloud_core::state::SharedState;
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::rest::{access, publish, Body, UserRef};
use crate::store;

/// Longest excerpt stored (characters); longer ones are cut.
pub const MAX_EXCERPT_CHARS: usize = 200;
/// Longest chat message id (characters).
pub const MAX_MESSAGE_ID_CHARS: usize = 128;
/// Most `user_ids` in one `POST /canvases/{id}/mentions`.
pub const MAX_MENTION_TARGETS: usize = 100;
/// `GET /mentions` returns at most this many (default [`DEFAULT_MENTIONS_LIMIT`]).
pub const MAX_MENTIONS_LIMIT: i64 = 100;
/// `GET /mentions` page size when `limit` is not given.
pub const DEFAULT_MENTIONS_LIMIT: i64 = 50;
/// Most `ids` in one `POST /mentions/read`.
pub const MAX_READ_IDS: usize = 500;
/// `POST /canvases/{id}/mentions` calls per user per minute (GCRA: bursts of this many, then
/// one every `60 / n` s).
pub const MENTION_POSTS_PER_MINUTE: u32 = 60;
/// AAD prefix of a sealed excerpt (followed by the mention row id's 16 raw bytes).
pub const EXCERPT_AAD_PREFIX: &[u8] = b"copper-cloud/v1/mention:";

/// SSE `canvas` event kind sent to each newly mentioned member.
pub const KIND_MENTION: &str = "mention";
/// SSE `canvas` event kind sent to the caller after marking mentions read.
pub const KIND_MENTION_READ: &str = "mention_read";

// ---------------------------------------------------------------------------------------------
// Rate limit

struct PostLimiter {
    limiter: DefaultKeyedRateLimiter<Uuid>,
    calls: AtomicU64,
}

static POST_LIMITER: LazyLock<PostLimiter> = LazyLock::new(|| PostLimiter {
    limiter: RateLimiter::keyed(Quota::per_minute(
        NonZeroU32::new(MENTION_POSTS_PER_MINUTE).expect("non-zero"),
    )),
    calls: AtomicU64::new(0),
});

/// `true` if `user` may post mentions now (process-wide, per user).
fn allow_post(user: Uuid) -> bool {
    let l = &*POST_LIMITER;
    if l.calls.fetch_add(1, Ordering::Relaxed) % 4096 == 4095 {
        l.limiter.retain_recent();
        l.limiter.shrink_to_fit();
    }
    l.limiter.check_key(&user).is_ok()
}

// ---------------------------------------------------------------------------------------------
// Helpers

fn excerpt_aad(mention_id: Uuid) -> Vec<u8> {
    let mut aad = Vec::with_capacity(EXCERPT_AAD_PREFIX.len() + 16);
    aad.extend_from_slice(EXCERPT_AAD_PREFIX);
    aad.extend_from_slice(mention_id.as_bytes());
    aad
}

/// One line of at most [`MAX_EXCERPT_CHARS`]: control characters (newlines included) become
/// spaces, runs of whitespace collapse, ends are trimmed.
fn clean_excerpt(raw: &str) -> String {
    let spaced: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    spaced
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_EXCERPT_CHARS)
        .collect()
}

fn message_id(raw: &str) -> Result<String, ApiError> {
    let id = raw.trim();
    if id.is_empty() {
        return Err(ApiError::bad_request("message_id must not be empty"));
    }
    if id.chars().count() > MAX_MESSAGE_ID_CHARS {
        return Err(ApiError::bad_request(format!(
            "message_id must be at most {MAX_MESSAGE_ID_CHARS} characters"
        )));
    }
    if id.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "message_id must not contain control characters",
        ));
    }
    Ok(id.to_owned())
}

// ---------------------------------------------------------------------------------------------
// POST /canvases/{id}/mentions

#[derive(Deserialize)]
pub(crate) struct MentionBody {
    message_id: String,
    #[serde(default)]
    user_ids: Vec<Uuid>,
    #[serde(default)]
    excerpt: String,
}

/// `POST /canvases/{id}/mentions` response.
#[derive(Debug, Clone, Serialize)]
pub struct MentionsSent {
    /// Members (other than the caller) who are now mentioned by this message — including on a
    /// retry, when nobody is notified again.
    pub notified: Vec<Uuid>,
    /// Everyone else from `user_ids`: the caller, non-members, unknown ids.
    pub skipped: Vec<Uuid>,
}

pub(crate) async fn post_mentions(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(raw): Path<String>,
    Body(body): Body<MentionBody>,
) -> Result<Json<MentionsSent>, ApiError> {
    let a = access(&state, user.user_id, &raw).await?;
    if a.kind == "personal" {
        return Err(ApiError::bad_request("the Personal canvas has no chat"));
    }
    let message_id = message_id(&body.message_id)?;
    if body.user_ids.len() > MAX_MENTION_TARGETS {
        return Err(ApiError::bad_request(format!(
            "at most {MAX_MENTION_TARGETS} user_ids per message"
        )));
    }
    if !allow_post(user.user_id) {
        metrics::counter!("canvas_mentions_rate_limited_total").increment(1);
        tracing::warn!(user_id = %user.user_id, canvas_id = %a.canvas_id, "mention rate limit exceeded");
        return Err(ApiError::RateLimited);
    }
    let excerpt = clean_excerpt(&body.excerpt);
    let mut seen = HashSet::new();
    let wanted: Vec<Uuid> = body
        .user_ids
        .into_iter()
        .filter(|u| seen.insert(*u))
        .collect();
    if wanted.is_empty() {
        return Ok(Json(MentionsSent {
            notified: Vec::new(),
            skipped: Vec::new(),
        }));
    }

    // Seal an excerpt for every candidate row up front (≤ 100, microseconds), so membership
    // filtering, the idempotent insert and the result are one statement.
    let key = store::doc_key(&state.db, &state.crypto, a.canvas_id).await?;
    let row_ids: Vec<Uuid> = wanted.iter().map(|_| ids::uuid_v7()).collect();
    let sealed: Vec<Vec<u8>> = row_ids
        .iter()
        .map(|id| Crypto::seal(&key, &excerpt_aad(*id), excerpt.as_bytes()))
        .collect();
    let rows: Vec<(Uuid, bool)> = sqlx::query_as(
        "WITH t AS (
             SELECT * FROM UNNEST($4::uuid[], $5::uuid[], $6::bytea[]) AS t (id, to_user, excerpt)
         ), ok AS (
             SELECT t.* FROM t
             JOIN canvas_members m ON m.canvas_id = $1 AND m.user_id = t.to_user
             WHERE t.to_user <> $3
         ), ins AS (
             INSERT INTO canvas_mentions (id, canvas_id, message_id, from_user, to_user, excerpt_sealed)
             SELECT id, $1, $2, $3, to_user, excerpt FROM ok
             ON CONFLICT (canvas_id, message_id, to_user) DO NOTHING
             RETURNING to_user
         )
         SELECT ok.to_user, (ins.to_user IS NOT NULL) AS fresh
         FROM ok LEFT JOIN ins ON ins.to_user = ok.to_user",
    )
    .bind(a.canvas_id)
    .bind(&message_id)
    .bind(user.user_id)
    .bind(&row_ids)
    .bind(&wanted)
    .bind(&sealed)
    .fetch_all(&state.db)
    .await?;

    let mentioned: HashSet<Uuid> = rows.iter().map(|r| r.0).collect();
    let fresh: Vec<Uuid> = rows.iter().filter(|r| r.1).map(|r| r.0).collect();
    let (notified, skipped): (Vec<Uuid>, Vec<Uuid>) =
        wanted.into_iter().partition(|u| mentioned.contains(u));
    publish(&state, &fresh, a.canvas_id, KIND_MENTION);
    metrics::counter!("canvas_mentions_total").increment(fresh.len() as u64);
    tracing::info!(
        user_id = %user.user_id,
        canvas_id = %a.canvas_id,
        notified = fresh.len(),
        mentioned = notified.len(),
        skipped = skipped.len(),
        "canvas chat mentions"
    );
    Ok(Json(MentionsSent { notified, skipped }))
}

// ---------------------------------------------------------------------------------------------
// GET /mentions

#[derive(Debug, Default, Deserialize)]
pub(crate) struct MentionsQuery {
    unread: Option<String>,
    limit: Option<i64>,
}

/// `{id, name}` of the canvas a mention is on.
#[derive(Debug, Clone, Serialize)]
pub struct MentionCanvas {
    pub id: Uuid,
    pub name: String,
}

/// One `GET /mentions` item.
#[derive(Debug, Clone, Serialize)]
pub struct MentionView {
    pub id: Uuid,
    pub canvas: MentionCanvas,
    pub from: UserRef,
    pub message_id: String,
    pub excerpt: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub read_at: Option<OffsetDateTime>,
}

/// `GET /mentions` response.
#[derive(Debug, Clone, Serialize)]
pub struct MentionList {
    pub mentions: Vec<MentionView>,
    /// Every unread mention of the caller on canvases they can see (not capped by `limit`).
    pub unread: i64,
}

#[derive(sqlx::FromRow)]
struct MentionRow {
    id: Uuid,
    canvas_id: Uuid,
    canvas_name: String,
    doc_key_wrapped: Vec<u8>,
    from_id: Uuid,
    from_email: String,
    from_display_name: String,
    message_id: String,
    excerpt_sealed: Vec<u8>,
    created_at: OffsetDateTime,
    read_at: Option<OffsetDateTime>,
}

fn flag(raw: Option<&str>) -> Result<bool, ApiError> {
    match raw.map(str::trim) {
        None | Some("" | "0" | "false" | "no") => Ok(false),
        Some("1" | "true" | "yes") => Ok(true),
        Some(_) => Err(ApiError::bad_request("unread must be 1 or 0")),
    }
}

pub(crate) async fn list_mentions(
    State(state): State<SharedState>,
    user: AuthUser,
    QueryParams(q): QueryParams<MentionsQuery>,
) -> Result<Json<MentionList>, ApiError> {
    let unread_only = flag(q.unread.as_deref())?;
    let limit = q.limit.unwrap_or(DEFAULT_MENTIONS_LIMIT);
    if limit < 1 {
        return Err(ApiError::bad_request("limit must be at least 1"));
    }
    let limit = limit.min(MAX_MENTIONS_LIMIT);
    // Only canvases the caller is still a member of.
    let rows = sqlx::query_as::<_, MentionRow>(
        "SELECT mt.id, mt.canvas_id, c.name AS canvas_name, c.doc_key_wrapped,
                u.id AS from_id, u.email AS from_email, u.display_name AS from_display_name,
                mt.message_id, mt.excerpt_sealed, mt.created_at, mt.read_at
         FROM canvas_mentions mt
         JOIN canvas_members m ON m.canvas_id = mt.canvas_id AND m.user_id = mt.to_user
         JOIN canvases c ON c.id = mt.canvas_id
         JOIN users u ON u.id = mt.from_user
         WHERE mt.to_user = $1 AND (NOT $2 OR mt.read_at IS NULL)
         ORDER BY mt.created_at DESC, mt.id DESC
         LIMIT $3",
    )
    .bind(user.user_id)
    .bind(unread_only)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    let unread: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM canvas_mentions mt
         JOIN canvas_members m ON m.canvas_id = mt.canvas_id AND m.user_id = mt.to_user
         WHERE mt.to_user = $1 AND mt.read_at IS NULL",
    )
    .bind(user.user_id)
    .fetch_one(&state.db)
    .await?;

    let mut keys: HashMap<Uuid, Option<[u8; 32]>> = HashMap::new();
    let mut mentions = Vec::with_capacity(rows.len());
    for r in rows {
        let key = *keys.entry(r.canvas_id).or_insert_with(|| {
            state
                .crypto
                .unwrap_key(&r.doc_key_wrapped)
                .inspect_err(|e| tracing::warn!(canvas_id = %r.canvas_id, error = %e, "unwrapping canvas key for mentions failed"))
                .ok()
        });
        let opened = key.and_then(|k| Crypto::open(&k, &excerpt_aad(r.id), &r.excerpt_sealed).ok());
        let excerpt = if let Some(bytes) = opened {
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            tracing::warn!(mention_id = %r.id, "mention excerpt does not decrypt; served empty");
            String::new()
        };
        mentions.push(MentionView {
            id: r.id,
            canvas: MentionCanvas {
                id: r.canvas_id,
                name: r.canvas_name,
            },
            from: UserRef {
                id: r.from_id,
                email: r.from_email,
                display_name: r.from_display_name,
            },
            message_id: r.message_id,
            excerpt,
            created_at: r.created_at,
            read_at: r.read_at,
        });
    }
    Ok(Json(MentionList { mentions, unread }))
}

// ---------------------------------------------------------------------------------------------
// POST /mentions/read

#[derive(Deserialize)]
pub(crate) struct ReadBody {
    #[serde(default)]
    ids: Option<Vec<Uuid>>,
    #[serde(default)]
    canvas_id: Option<Uuid>,
}

/// `POST /mentions/read` response.
#[derive(Debug, Clone, Serialize)]
pub struct MentionsRead {
    pub ok: bool,
    /// Mentions that went from unread to read.
    pub updated: usize,
}

pub(crate) async fn mark_read(
    State(state): State<SharedState>,
    user: AuthUser,
    Body(body): Body<ReadBody>,
) -> Result<Json<MentionsRead>, ApiError> {
    if body.ids.is_none() && body.canvas_id.is_none() {
        return Err(ApiError::bad_request("send ids or canvas_id"));
    }
    let ids = body.ids.unwrap_or_default();
    if ids.len() > MAX_READ_IDS {
        return Err(ApiError::bad_request(format!(
            "at most {MAX_READ_IDS} ids per call"
        )));
    }
    let touched: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE canvas_mentions mt SET read_at = now()
         WHERE mt.to_user = $1 AND mt.read_at IS NULL
           AND (mt.id = ANY($2) OR mt.canvas_id = $3)
           AND EXISTS (SELECT 1 FROM canvas_members m
                       WHERE m.canvas_id = mt.canvas_id AND m.user_id = $1)
         RETURNING mt.canvas_id",
    )
    .bind(user.user_id)
    .bind(&ids)
    .bind(body.canvas_id)
    .fetch_all(&state.db)
    .await?;
    let canvases: HashSet<Uuid> = touched.iter().copied().collect();
    for canvas_id in canvases {
        publish(&state, &[user.user_id], canvas_id, KIND_MENTION_READ);
    }
    Ok(Json(MentionsRead {
        ok: true,
        updated: touched.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts_are_one_trimmed_line() {
        assert_eq!(
            clean_excerpt("  hi\n\n@Ann  see\tthis \u{7}"),
            "hi @Ann see this"
        );
        assert_eq!(clean_excerpt(&"é".repeat(300)).chars().count(), 200);
        assert_eq!(clean_excerpt(""), "");
    }

    #[test]
    fn message_ids() {
        assert_eq!(message_id(" m1 ").unwrap(), "m1");
        assert!(message_id("  ").is_err());
        assert!(message_id(&"x".repeat(129)).is_err());
        assert!(message_id("a\u{0}b").is_err());
    }

    #[test]
    fn unread_flag() {
        assert!(!flag(None).unwrap());
        assert!(flag(Some("1")).unwrap() && flag(Some("true")).unwrap());
        assert!(!flag(Some("0")).unwrap());
        assert!(flag(Some("maybe")).is_err());
    }

    #[test]
    fn aad_binds_the_row() {
        let key = Crypto::new_key();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let sealed = Crypto::seal(&key, &excerpt_aad(a), b"hello");
        assert_eq!(
            Crypto::open(&key, &excerpt_aad(a), &sealed).unwrap(),
            b"hello"
        );
        assert!(Crypto::open(&key, &excerpt_aad(b), &sealed).is_err());
    }
}
