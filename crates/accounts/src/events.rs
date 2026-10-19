//! `GET /v1/events` — the caller's `canvas` events as Server-Sent Events, with `id:` lines and
//! `Last-Event-ID` resume from core's ring buffer.
//!
//! Ids only increase, across restarts too: `base + n`, where `n` is core's per-process event id
//! ([`Envelope::id`], restarting at 1 with the process) and `base` the time this server started
//! in Unix microseconds. A later process's base is above every id an earlier one issued (unless
//! it published more than a million events a second), so an id from before a restart never
//! resumes into the new numbering; it gets a `resync` instead. Ids stay below 2^53, so
//! JavaScript reads them exactly.
//!
//! Frames:
//!
//! * `event: canvas` / `id: <u64>` / `data: {"type":"canvas","canvas_id":…,"kind":…}` — the
//!   same JSON as copper-cloud's `canvas` frames;
//! * `event: resync` / `data: {"reason":"gap"}` (the `Last-Event-ID` is no longer buffered,
//!   or unknown) or `{"reason":"lagged","skipped":n}` (this stream fell behind): refetch
//!   whatever you cache, then carry on;
//! * `event: ready` / `id: <u64>` / `data: {"user_id":…,"last_event_id":…}` — sent once,
//!   after any replay: everything up to that id has been delivered;
//! * `: keepalive` comments every [`KEEPALIVE`].
//!
//! The stream ends when the credential stops being valid (checked every
//! [`REVALIDATE_EVERY`]) or the server shuts down.

use std::convert::Infallible;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Extension;
use canvas_cloud_core::events::{Envelope, Events, Resume};
use canvas_cloud_core::extract::QueryParams;
use canvas_cloud_core::identity::{AuthUser, Via};
use canvas_cloud_core::state::SharedState;
use futures::Stream;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Receiver;
use uuid::Uuid;

use crate::SharedAccounts;

/// Keepalive comment interval.
pub const KEEPALIVE: Duration = Duration::from_secs(15);
/// How often an open stream re-checks that its credential is still valid.
pub const REVALIDATE_EVERY: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
pub(crate) struct EventsQuery {
    /// For clients that cannot set `Last-Event-ID` on the first connection.
    last_event_id: Option<String>,
}

/// This process's id base: the current Unix time in microseconds.
pub(crate) fn id_base_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX / 2))
}

/// The SSE id of core event `n`.
fn wire_id(base: u64, n: u64) -> u64 {
    base.saturating_add(n)
}

/// Core's event id for an SSE id, `None` for one from before this process (below its base).
fn local_id(base: u64, wire: u64) -> Option<u64> {
    wire.checked_sub(base)
}

/// Buffered envelopes after `after`, the id they reach, and a tap receiver positioned right
/// after them; `None` when `after` is no longer (or was never) buffered.
fn subscribe_after(
    events: &Events,
    after: u64,
) -> Option<(Vec<Envelope>, u64, Receiver<Envelope>)> {
    match events.resume(after) {
        (Resume::Replay(replay), rx) => {
            let reached = replay.last().map_or(after, |e| e.id);
            Some((replay, reached, rx))
        }
        (Resume::Gap, _) => None,
    }
}

/// Subscribe from "now".
fn subscribe_now(events: &Events) -> (Vec<Envelope>, u64, Receiver<Envelope>) {
    loop {
        // Only a burst of more than RING_CAPACITY events between the two calls misses.
        if let Some(s) = subscribe_after(events, events.last_id()) {
            return s;
        }
    }
}

fn canvas_frame(base: u64, env: &Envelope) -> SseEvent {
    SseEvent::default()
        .event(env.event.name())
        .id(wire_id(base, env.id).to_string())
        .data(serde_json::to_string(&env.event).unwrap_or_default())
}

/// Whether the stream's credential still works (fails open on database errors; the next
/// check or the client's reconnect settles it).
async fn credential_alive(db: &sqlx::PgPool, user: &AuthUser) -> bool {
    let q = match &user.via {
        Via::Session { session_id } => sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM sessions s JOIN users u ON u.id = s.user_id
                            WHERE s.id = $1 AND s.expires_at > now() AND NOT u.disabled)",
        )
        .bind(*session_id),
        Via::Token { token_id, .. } => sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM access_tokens t JOIN users u ON u.id = t.user_id
                            WHERE t.id = $1 AND t.revoked_at IS NULL
                              AND (t.expires_at IS NULL OR t.expires_at > now())
                              AND NOT u.disabled)",
        )
        .bind(*token_id),
        Via::App { app_id, subject } => sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM apps a
                            JOIN app_users au ON au.app_id = a.id
                            JOIN users u ON u.id = au.user_id
                            WHERE a.id = $1 AND a.revoked_at IS NULL AND au.subject = $2
                              AND NOT u.disabled)",
        )
        .bind(*app_id)
        .bind(subject.clone()),
    };
    q.fetch_one(db).await.unwrap_or(true)
}

struct StreamGauge;

impl StreamGauge {
    fn new() -> Self {
        metrics::gauge!("sse_subscribers", "stream" => "user").increment(1.0);
        Self
    }
}

impl Drop for StreamGauge {
    fn drop(&mut self) {
        metrics::gauge!("sse_subscribers", "stream" => "user").decrement(1.0);
    }
}

/// Where a client asked to resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResumeFrom {
    /// No `Last-Event-ID`: start now.
    Now,
    /// After this event id.
    After(u64),
    /// An unparsable `Last-Event-ID`: resync, then start now.
    Invalid,
}

fn requested_resume(headers: &HeaderMap, query: Option<&str>) -> ResumeFrom {
    let raw = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .or(query)
        .map(str::trim)
        .unwrap_or_default();
    if raw.is_empty() {
        return ResumeFrom::Now;
    }
    raw.parse::<u64>()
        .map_or(ResumeFrom::Invalid, ResumeFrom::After)
}

fn event_stream(
    state: SharedState,
    base: u64,
    user: AuthUser,
    resume: ResumeFrom,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    let events = &state.events;
    let resumed = match resume {
        ResumeFrom::Now => Some(subscribe_now(events)),
        ResumeFrom::After(wire) => {
            local_id(base, wire).and_then(|after| subscribe_after(events, after))
        }
        ResumeFrom::Invalid => None,
    };
    let gap = resumed.is_none();
    let (replay, reached, mut rx) = resumed.unwrap_or_else(|| subscribe_now(events));
    let reached = wire_id(base, reached);
    let user_id: Uuid = user.user_id;
    let mut shutdown = canvas_cloud_core::shutdown::subscribe();
    async_stream::stream! {
        let _gauge = StreamGauge::new();
        if gap {
            yield Ok(SseEvent::default().event("resync").data(json!({ "reason": "gap" }).to_string()));
        }
        for env in replay.iter().filter(|e| e.user_id == user_id) {
            yield Ok(canvas_frame(base, env));
        }
        yield Ok(SseEvent::default()
            .event("ready")
            .id(reached.to_string())
            .data(json!({ "user_id": user_id, "last_event_id": reached }).to_string()));
        let mut revalidate = tokio::time::interval(REVALIDATE_EVERY);
        revalidate.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        revalidate.tick().await;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(env) => {
                        if env.user_id == user_id {
                            yield Ok(canvas_frame(base, &env));
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        yield Ok(SseEvent::default().event("resync").data(
                            json!({ "reason": "lagged", "skipped": skipped }).to_string(),
                        ));
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = revalidate.tick() => {
                    if !credential_alive(&state.db, &user).await {
                        break;
                    }
                }
                () = canvas_cloud_core::shutdown::wait(&mut shutdown) => break,
            }
        }
    }
}

/// `GET /v1/events` (any credential; `?token=` accepted with `Accept: text/event-stream`).
pub(crate) async fn stream(
    State(state): State<SharedState>,
    Extension(shared): Extension<SharedAccounts>,
    user: AuthUser,
    headers: HeaderMap,
    QueryParams(q): QueryParams<EventsQuery>,
) -> impl IntoResponse {
    let resume = requested_resume(&headers, q.last_event_id.as_deref());
    let sse = Sse::new(event_stream(state, shared.events_base, user, resume))
        .keep_alive(KeepAlive::new().interval(KEEPALIVE).text("keepalive"));
    (
        [
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (
                HeaderName::from_static("x-accel-buffering"),
                HeaderValue::from_static("no"),
            ),
        ],
        sse,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_cloud_core::events::Event;

    #[test]
    fn resume_point_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(requested_resume(&h, None), ResumeFrom::Now);
        assert_eq!(requested_resume(&h, Some("7")), ResumeFrom::After(7));
        assert_eq!(requested_resume(&h, Some(" ")), ResumeFrom::Now);
        h.insert("last-event-id", HeaderValue::from_static("12"));
        assert_eq!(
            requested_resume(&h, Some("7")),
            ResumeFrom::After(12),
            "header wins"
        );
        h.insert("last-event-id", HeaderValue::from_static("nope"));
        assert_eq!(requested_resume(&h, None), ResumeFrom::Invalid);
    }

    #[test]
    fn wire_ids() {
        let base = id_base_now();
        assert!(base > 1_700_000_000_000_000 && base < (1 << 53), "{base}");
        assert_eq!(wire_id(base, 7), base + 7);
        assert_eq!(local_id(base, base + 7), Some(7));
        assert_eq!(local_id(base, base), Some(0));
        assert_eq!(local_id(base, base - 1), None, "an earlier process");
        assert_eq!(local_id(base, 7), None, "a bare core id");
        assert!(id_base_now() >= base);
    }

    #[test]
    fn subscribe_positions() {
        let ev = Events::new();
        let u = Uuid::now_v7();
        let canvas = Event::Canvas {
            canvas_id: Uuid::nil(),
            kind: "update".into(),
        };
        for _ in 0..3 {
            ev.publish(u, canvas.clone());
        }
        let (replay, reached, _rx) = subscribe_now(&ev);
        assert_eq!(replay, Vec::<Envelope>::new());
        assert_eq!(reached, 3);
        let (replay, reached, _rx) = subscribe_after(&ev, 1).unwrap();
        assert_eq!(replay.iter().map(|e| e.id).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(reached, 3);
        assert!(subscribe_after(&ev, 9).is_none());
    }
}
