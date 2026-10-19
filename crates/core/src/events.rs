//! In-process event fan-out: a per-user bus (canvas notifications for that user's SSE
//! streams) plus a global tap (every event, for linked apps' `/v1/apps/self/events`).
//!
//! * Per user: one `tokio::sync::broadcast` channel per user that currently has at least one
//!   subscriber. Publishing to a user nobody listens to is a cheap map lookup; empty channels
//!   are dropped on the next publish.
//! * Every published event gets a process-wide, strictly increasing `u64` id
//!   ([`Envelope::id`]) and goes to the global tap ([`Events::tap`]) and into a ring buffer of
//!   the last [`RING_CAPACITY`] envelopes, so an SSE client reconnecting with `Last-Event-ID`
//!   can resume ([`Events::since`]) or be told to resync when its id fell out of the buffer.
//!
//! Single-process only (one daemon per instance). Ids also increase across restarts: a process
//! numbers its events from the Unix time it started, in microseconds, so an id issued by an
//! earlier process is always below every id of the current one and resumes as a gap (resync)
//! instead of replaying unrelated events (unless an earlier process published more than a
//! million events a second). Ids stay below 2^53, so JavaScript reads them exactly.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::broadcast;
use uuid::Uuid;

/// Per-user channel capacity. A subscriber that falls this far behind gets `Lagged` and is
/// told to resync.
const CHANNEL_CAPACITY: usize = 256;
/// Global tap capacity (all users' events).
const TAP_CAPACITY: usize = 4096;
/// How many recent events are kept for `Last-Event-ID` resume.
pub const RING_CAPACITY: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    /// Something about a canvas changed (`kind` e.g. `created`, `renamed`, `deleted`,
    /// `invited` (new invite or a re-send reminder), `invite_revoked`, `invite_declined`,
    /// `member_added`, `member_removed`, `update`, `mention` (you were @mentioned in the
    /// canvas chat), `mention_read` (your mentions there were marked read, e.g. on another
    /// client)).
    Canvas { canvas_id: Uuid, kind: String },
}

impl Event {
    /// SSE `event:` name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Canvas { .. } => "canvas",
        }
    }
}

/// One published event, as seen by the global tap and the resume buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    /// Strictly increasing, across restarts too (starts at the process start time in Unix
    /// microseconds).
    pub id: u64,
    /// Whose event it is.
    pub user_id: Uuid,
    pub event: Event,
}

/// What [`Events::since`] found for a `Last-Event-ID`.
#[derive(Debug, PartialEq, Eq)]
pub enum Resume {
    /// Every envelope after the id is still buffered (possibly none), oldest first.
    Replay(Vec<Envelope>),
    /// Some events after the id were dropped from the buffer (or the id is from another
    /// process lifetime): the client must resync.
    Gap,
}

struct Inner {
    users: HashMap<Uuid, broadcast::Sender<Event>>,
    ring: VecDeque<Envelope>,
    next_id: u64,
}

#[derive(Clone)]
pub struct Events {
    inner: Arc<Mutex<Inner>>,
    tap: broadcast::Sender<Envelope>,
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("Events")
            .field("users", &inner.users.len())
            .field("last_id", &inner.next_id.saturating_sub(1))
            .finish_non_exhaustive()
    }
}

impl Events {
    /// A bus whose first event id is the current Unix time in microseconds.
    pub fn new() -> Self {
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| u64::try_from(d.as_micros()).unwrap_or(1 << 52));
        Self::starting_at(now_us)
    }

    /// A bus whose first event id is `first_id` (at least 1; tests use 1).
    pub fn starting_at(first_id: u64) -> Self {
        let (tap, _) = broadcast::channel(TAP_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                users: HashMap::new(),
                ring: VecDeque::with_capacity(RING_CAPACITY),
                next_id: first_id.max(1),
            })),
            tap,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Deliver `ev` to every live subscriber of `user_id` and to the global tap. Never
    /// blocks. Returns the event's id.
    pub fn publish(&self, user_id: Uuid, ev: Event) -> u64 {
        let mut inner = self.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        if let Some(tx) = inner.users.get(&user_id) {
            if tx.send(ev.clone()).is_err() {
                // No receivers left.
                inner.users.remove(&user_id);
            }
        }
        let env = Envelope {
            id,
            user_id,
            event: ev,
        };
        if inner.ring.len() == RING_CAPACITY {
            inner.ring.pop_front();
        }
        inner.ring.push_back(env.clone());
        // Sent under the lock so tap order == id order. No receivers is fine.
        let _ = self.tap.send(env);
        id
    }

    /// Subscribe to `user_id`'s events.
    pub fn subscribe(&self, user_id: Uuid) -> broadcast::Receiver<Event> {
        let mut inner = self.lock();
        if let Some(tx) = inner.users.get(&user_id) {
            return tx.subscribe();
        }
        let (tx, rx) = broadcast::channel(CHANNEL_CAPACITY);
        inner.users.insert(user_id, tx);
        rx
    }

    /// Subscribe to every event of every user (the global tap). A receiver that lags gets
    /// `RecvError::Lagged` and should fall back to [`Events::since`] or resync.
    pub fn tap(&self) -> broadcast::Receiver<Envelope> {
        self.tap.subscribe()
    }

    /// Atomically: the buffered envelopes after `last_id` plus a tap receiver positioned right
    /// after them, so a resuming stream neither misses nor duplicates an event.
    pub fn resume(&self, last_id: u64) -> (Resume, broadcast::Receiver<Envelope>) {
        let inner = self.lock();
        let rx = self.tap.subscribe();
        (Self::since_locked(&inner, last_id), rx)
    }

    /// The buffered envelopes with `id > last_id`, or [`Resume::Gap`] when some of them are no
    /// longer buffered (or `last_id` was never issued by this process).
    pub fn since(&self, last_id: u64) -> Resume {
        Self::since_locked(&self.lock(), last_id)
    }

    fn since_locked(inner: &Inner, last_id: u64) -> Resume {
        let last_issued = inner.next_id - 1;
        if last_id > last_issued {
            return Resume::Gap;
        }
        if last_id == last_issued {
            return Resume::Replay(Vec::new());
        }
        match inner.ring.front() {
            Some(first) if first.id <= last_id + 1 => Resume::Replay(
                inner
                    .ring
                    .iter()
                    .filter(|e| e.id > last_id)
                    .cloned()
                    .collect(),
            ),
            _ => Resume::Gap,
        }
    }

    /// The id of the most recently published event (the first id minus one = none yet).
    pub fn last_id(&self) -> u64 {
        self.lock().next_id - 1
    }

    /// Number of live subscribers for `user_id` (diagnostics/tests).
    pub fn subscriber_count(&self, user_id: Uuid) -> usize {
        self.lock()
            .users
            .get(&user_id)
            .map_or(0, broadcast::Sender::receiver_count)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_start_at_the_wall_clock_and_survive_restarts() {
        let earlier = Events::new();
        let last_of_earlier = earlier.publish(Uuid::now_v7(), canvas("created"));
        assert!(last_of_earlier > 1_700_000_000_000_000 && last_of_earlier < (1 << 53));
        std::thread::sleep(std::time::Duration::from_millis(2));
        let later = Events::new();
        let first_of_later = later.publish(Uuid::now_v7(), canvas("created"));
        assert!(first_of_later > last_of_earlier);
        assert_eq!(later.since(last_of_earlier), Resume::Gap);
    }

    use super::*;

    fn canvas(kind: &str) -> Event {
        Event::Canvas {
            canvas_id: Uuid::nil(),
            kind: kind.into(),
        }
    }

    #[tokio::test]
    async fn fanout_is_per_user() {
        let ev = Events::starting_at(1);
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let mut ra1 = ev.subscribe(a);
        let mut ra2 = ev.subscribe(a);
        let mut rb = ev.subscribe(b);
        ev.publish(a, canvas("update"));
        assert_eq!(ra1.recv().await.unwrap(), canvas("update"));
        assert_eq!(ra2.recv().await.unwrap(), canvas("update"));
        assert!(rb.try_recv().is_err());
        assert_eq!(ev.subscriber_count(a), 2);
    }

    #[test]
    fn publish_without_subscribers_cleans_up() {
        let ev = Events::starting_at(1);
        let u = Uuid::now_v7();
        ev.publish(u, canvas("a"));
        drop(ev.subscribe(u));
        ev.publish(u, canvas("b"));
        assert_eq!(ev.lock().users.len(), 0);
    }

    #[tokio::test]
    async fn tap_sees_every_user_with_increasing_ids() {
        let ev = Events::starting_at(1);
        let mut tap = ev.tap();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(ev.publish(a, canvas("created")), 1);
        assert_eq!(ev.publish(b, canvas("renamed")), 2);
        let one = tap.recv().await.unwrap();
        let two = tap.recv().await.unwrap();
        assert_eq!((one.id, one.user_id), (1, a));
        assert_eq!((two.id, two.user_id, two.event), (2, b, canvas("renamed")));
        assert_eq!(ev.last_id(), 2);
    }

    #[test]
    fn resume_replays_or_reports_a_gap() {
        let ev = Events::starting_at(1);
        let u = Uuid::now_v7();
        assert_eq!(ev.since(0), Resume::Replay(vec![]));
        assert_eq!(ev.since(5), Resume::Gap, "never issued");
        for _ in 0..10 {
            ev.publish(u, canvas("update"));
        }
        let Resume::Replay(r) = ev.since(7) else {
            panic!("expected replay")
        };
        assert_eq!(r.iter().map(|e| e.id).collect::<Vec<_>>(), vec![8, 9, 10]);
        assert_eq!(ev.since(10), Resume::Replay(vec![]));
        let Resume::Replay(all) = ev.since(0) else {
            panic!("expected replay")
        };
        assert_eq!(all.len(), 10);

        // Overflow the ring: ids 1..=10+RING_CAPACITY, the first 10 are gone.
        for _ in 0..RING_CAPACITY {
            ev.publish(u, canvas("update"));
        }
        assert_eq!(ev.since(5), Resume::Gap);
        assert_eq!(ev.since(9), Resume::Gap);
        let Resume::Replay(tail) = ev.since(10) else {
            panic!("id 11 is still buffered")
        };
        assert_eq!(tail.len(), RING_CAPACITY);
        assert_eq!(tail[0].id, 11);
    }

    #[tokio::test]
    async fn resume_then_tap_has_no_gap_or_duplicate() {
        let ev = Events::starting_at(1);
        let u = Uuid::now_v7();
        ev.publish(u, canvas("a"));
        ev.publish(u, canvas("b"));
        let (resume, mut rx) = ev.resume(1);
        ev.publish(u, canvas("c"));
        let Resume::Replay(r) = resume else { panic!() };
        assert_eq!(r.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(rx.recv().await.unwrap().id, 3);
    }

    #[test]
    fn serializes_with_type_tag() {
        let s = serde_json::to_string(&canvas("deleted")).unwrap();
        assert_eq!(
            s,
            r#"{"type":"canvas","canvas_id":"00000000-0000-0000-0000-000000000000","kind":"deleted"}"#
        );
        assert_eq!(canvas("x").name(), "canvas");
    }
}
