//! `GET /v1/apps/self/events`: every canvas event of this app's users, as SSE.
//!
//! ```text
//! event: canvas
//! id: 42
//! data: {"subject":"…","user_id":"…","canvas_id":"…","kind":"created"}
//! ```
//!
//! Comment keepalives every 15 s. A reconnect with `Last-Event-ID: <id>` replays the events
//! after `<id>` still in core's resume buffer (the last 4096 of the whole instance); when
//! some were already dropped (or the id comes from before a restart), the stream starts with
//!
//! ```text
//! event: resync
//! id: <latest id>
//! data: {}
//! ```
//!
//! and the app should re-read whatever it caches. A stream that falls too far behind the
//! live tap gets a `resync` the same way. Streams end when the server shuts down or the app is
//! revoked.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use canvas_cloud_core::events::{Envelope, Event, Resume};
use canvas_cloud_core::identity::AppAuth;
use canvas_cloud_core::shutdown;
use canvas_cloud_core::state::SharedState;
use futures::Stream;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::common::Result;
use crate::directory::{self, Directory};

/// Interval of the `: keepalive` comments.
pub const KEEPALIVE: Duration = Duration::from_secs(15);
/// How often a stream asks for a directory refresh (which also notices a revoked app).
const HOUSEKEEPING: Duration = Duration::from_secs(30);

fn last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// The SSE frame of `env`, if it concerns one of this app's users.
fn frame(dir: &Directory, env: &Envelope) -> Option<SseEvent> {
    let subject = dir.subject(env.user_id)?;
    #[allow(unreachable_patterns)] // `Event` is non-exhaustive
    let data = match &env.event {
        Event::Canvas { canvas_id, kind } => json!({
            "subject": &*subject,
            "user_id": env.user_id,
            "canvas_id": canvas_id,
            "kind": kind,
        }),
        _ => return None,
    };
    Some(
        SseEvent::default()
            .event("canvas")
            .id(env.id.to_string())
            .data(data.to_string()),
    )
}

/// Resolves once the app is revoked (never, if the directory is gone).
async fn until_revoked(rx: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        let revoked = *rx.borrow_and_update();
        if revoked {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn resync(id: u64) -> SseEvent {
    SseEvent::default()
        .event("resync")
        .id(id.to_string())
        .data("{}")
}

pub async fn stream(
    State(state): State<SharedState>,
    app: AppAuth,
    headers: HeaderMap,
) -> Result<Response> {
    let dir = directory::open(&state.db, app.app_id).await?;
    let resume_from = last_event_id(&headers);
    let body = events(state, dir, resume_from);
    Ok(Sse::new(body)
        .keep_alive(KeepAlive::new().interval(KEEPALIVE).text("keepalive"))
        .into_response())
}

fn events(
    state: SharedState,
    dir: std::sync::Arc<Directory>,
    resume_from: Option<u64>,
) -> impl Stream<Item = std::result::Result<SseEvent, Infallible>> {
    async_stream::stream! {
        let mut stop = shutdown::subscribe();
        // Taken before subscribing: a resync's id must never skip an event this stream did
        // not deliver.
        let latest = state.events.last_id();
        let mut rx = match resume_from {
            Some(last) => {
                let (resume, rx) = state.events.resume(last);
                match resume {
                    Resume::Replay(envs) => {
                        for env in &envs {
                            if let Some(f) = frame(&dir, env) {
                                yield Ok(f);
                            }
                        }
                    }
                    Resume::Gap => yield Ok(resync(latest)),
                }
                rx
            }
            None => state.events.tap(),
        };
        let mut tick = tokio::time::interval(HOUSEKEEPING);
        tick.tick().await;
        let mut revoked = dir.on_revoked();
        loop {
            tokio::select! {
                () = shutdown::wait(&mut stop) => break,
                () = until_revoked(&mut revoked) => break,
                _ = tick.tick() => dir.refresh_if_stale(&state.db),
                msg = rx.recv() => match msg {
                    Ok(env) => {
                        if let Some(f) = frame(&dir, &env) {
                            yield Ok(f);
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        // Events were lost for this stream: tell the app to resync, keyed to
                        // what was issued so far, and carry on live from here.
                        rx = rx.resubscribe();
                        yield Ok(resync(state.events.last_id()));
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_event_id_parses() {
        let mut h = HeaderMap::new();
        assert_eq!(last_event_id(&h), None);
        h.insert("last-event-id", " 42 ".parse().unwrap());
        assert_eq!(last_event_id(&h), Some(42));
        h.insert("last-event-id", "x".parse().unwrap());
        assert_eq!(last_event_id(&h), None);
    }
}
