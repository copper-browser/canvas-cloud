//! In-memory canvas rooms: one `yrs::Doc` + awareness + broadcast channel per open canvas.
//!
//! Write path (WebSocket updates, REST ops, server-side meta edits): take the room's write
//! lock → apply in one `yrs` transaction → seal + INSERT the produced update → broadcast it →
//! compact every [`COMPACT_EVERY`] updates. The lock is held across the INSERT, so persisted
//! order == application order == broadcast order, and nobody ever observes an update that is
//! not durable. If persisting fails the in-memory doc is dropped (the room reloads from the
//! database on next use) and every peer is disconnected; y-websocket clients reconnect and
//! re-send whatever the server is missing during the sync handshake.
//!
//! Rooms with no peers and no REST activity for [`IDLE_EVICT`] are dropped from memory.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use copper_cloud_core::error::ApiError;
use copper_cloud_core::events::Event;
use copper_cloud_core::state::{AppState, SharedState};
use serde::Serialize;
use tokio::sync::{broadcast, RwLock, RwLockReadGuard, RwLockWriteGuard};
use uuid::Uuid;
use yrs::sync::{Awareness, AwarenessUpdate, Message, SyncMessage};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{ClientID, Doc, ReadTxn, StateVector, Transact, TransactionMut, Update};

use crate::store;
use crate::ws::close;

/// Rooms idle (no peers, no REST use) this long are evicted from memory.
pub const IDLE_EVICT: Duration = Duration::from_secs(60);
/// Compact the update log into a snapshot after this many updates.
pub const COMPACT_EVERY: usize = 200;
/// Broadcast frames buffered per room; a peer that falls further behind is disconnected.
pub const BROADCAST_CAPACITY: usize = 256;
/// `Event::Canvas { kind: "update" }` is published at most this often per room.
pub const EVENT_THROTTLE: Duration = Duration::from_secs(2);

/// Awareness client ids one connection may publish (y-websocket uses one per document).
pub const MAX_AWARENESS_CLIENTS_PER_PEER: usize = 16;

/// Origin id used for server-side writes (REST ops, renames); never equals a peer id.
pub(crate) const SERVER_ORIGIN: u64 = 0;

static NEXT_PEER: AtomicU64 = AtomicU64::new(1);

/// A fresh, process-unique peer id.
pub(crate) fn next_peer_id() -> u64 {
    NEXT_PEER.fetch_add(1, Ordering::Relaxed)
}

fn lock<T>(m: &StdMutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One frame fanned out to every peer of a room.
#[derive(Clone, Debug)]
pub(crate) enum Frame {
    /// An encoded y-websocket message. Peers skip document frames they
    /// originated; awareness frames are echoed back to their origin as well,
    /// like the reference y-websocket server does, so a lone client hears
    /// something at least every 15 s (its own presence renewal) and never
    /// trips the provider's 30 s silence timeout.
    Data {
        origin: u64,
        bytes: Bytes,
        echo: bool,
    },
    /// Disconnect `user`'s peers (or everybody when `None`) with a close code.
    Kick {
        user: Option<Uuid>,
        code: u16,
        reason: &'static str,
    },
}

/// The loaded document of a room.
pub(crate) struct DocState {
    doc: Doc,
    key: [u8; 32],
    /// Updates produced by committed transactions (filled by `observe_update_v1`).
    produced: Arc<StdMutex<Vec<Vec<u8>>>>,
    since_snapshot: usize,
    last_seq: i64,
}

impl DocState {
    async fn load(state: &AppState, canvas_id: Uuid) -> Result<Self, ApiError> {
        let stored = store::load(&state.db, &state.crypto, canvas_id).await?;
        let doc = Doc::new();
        let mut last_seq = stored.snapshot.as_ref().map_or(0, |s| s.0);
        {
            let mut txn = doc.transact_mut();
            let blobs = stored
                .snapshot
                .iter()
                .map(|(seq, b)| (*seq, b))
                .chain(stored.updates.iter().map(|(seq, b)| (*seq, b)));
            for (seq, blob) in blobs {
                last_seq = last_seq.max(seq);
                let applied = Update::decode_v1(blob)
                    .map_err(|e| e.to_string())
                    .and_then(|u| txn.apply_update(u).map_err(|e| e.to_string()));
                if let Err(error) = applied {
                    tracing::error!(%canvas_id, seq, error, "skipping undecodable stored canvas update");
                }
            }
        }
        let produced: Arc<StdMutex<Vec<Vec<u8>>>> = Arc::default();
        let sink = Arc::clone(&produced);
        doc.observe_update_v1("copper-cloud-persist", move |_, e| {
            lock(&sink).push(e.update.clone());
        })
        .map_err(|e| ApiError::internal(format!("observing canvas doc: {e}")))?;
        tracing::debug!(%canvas_id, updates = stored.updates.len(), "canvas room loaded");
        Ok(Self {
            doc,
            key: stored.key,
            produced,
            since_snapshot: stored.updates.len(),
            last_seq,
        })
    }

    /// The room's document (read-only use).
    pub(crate) fn doc(&self) -> &Doc {
        &self.doc
    }

    fn take_produced(&self) -> Option<Vec<u8>> {
        let mut produced = std::mem::take(&mut *lock(&self.produced));
        match produced.len() {
            0 => None,
            1 => produced.pop(),
            _ => match yrs::merge_updates_v1(&produced) {
                Ok(merged) => Some(merged),
                Err(e) => {
                    tracing::error!(error = %e, "merging canvas updates failed");
                    None
                }
            },
        }
    }
}

#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    trailing: bool,
}

/// One open canvas.
pub(crate) struct Room {
    pub id: Uuid,
    doc: RwLock<Option<DocState>>,
    awareness: StdMutex<Awareness>,
    tx: broadcast::Sender<Frame>,
    peers: AtomicUsize,
    last_active: StdMutex<Instant>,
    evict_scheduled: AtomicBool,
    throttle: StdMutex<Throttle>,
    closed: AtomicBool,
}

impl Room {
    fn new(id: Uuid) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            id,
            doc: RwLock::new(None),
            // Awareness only stores client states here; its doc is never used.
            awareness: StdMutex::new(Awareness::new(Doc::new())),
            tx,
            peers: AtomicUsize::new(0),
            last_active: StdMutex::new(Instant::now()),
            evict_scheduled: AtomicBool::new(false),
            throttle: StdMutex::default(),
            closed: AtomicBool::new(false),
        }
    }

    fn check_open(&self) -> Result<(), ApiError> {
        if self.closed.load(Ordering::Acquire) {
            Err(ApiError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Write guard over a loaded document (loads it on first use).
    async fn loaded_write(
        &self,
        state: &AppState,
    ) -> Result<RwLockWriteGuard<'_, Option<DocState>>, ApiError> {
        let mut g = self.doc.write().await;
        self.check_open()?;
        if g.is_none() {
            *g = Some(DocState::load(state, self.id).await?);
        }
        Ok(g)
    }

    /// Read guard over a loaded document (loads it on first use).
    pub(crate) async fn read(
        &self,
        state: &AppState,
    ) -> Result<RwLockReadGuard<'_, DocState>, ApiError> {
        self.touch();
        {
            let g = self.doc.read().await;
            self.check_open()?;
            if let Ok(mapped) = RwLockReadGuard::try_map(g, Option::as_ref) {
                return Ok(mapped);
            }
        }
        let g = self.loaded_write(state).await?.downgrade();
        RwLockReadGuard::try_map(g, Option::as_ref)
            .map_err(|_| ApiError::internal("canvas room failed to load"))
    }

    /// Makes sure the document is loaded (used before a WebSocket upgrade).
    pub(crate) async fn ensure_loaded(&self, state: &AppState) -> Result<(), ApiError> {
        self.read(state).await.map(drop)
    }

    /// Runs `f` in one write transaction, then persists and broadcasts what it produced.
    ///
    /// `origin` is the peer that caused the change (its own frame is not echoed back).
    pub(crate) async fn transact<R>(
        self: &Arc<Self>,
        state: &SharedState,
        origin: u64,
        f: impl FnOnce(&mut TransactionMut<'_>) -> R,
    ) -> Result<R, ApiError> {
        self.touch();
        let mut g = self.loaded_write(state).await?;
        let Some(st) = g.as_mut() else {
            return Err(ApiError::internal("canvas room not loaded"));
        };
        let out = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut txn = st.doc.transact_mut();
            f(&mut txn)
        }));
        let Ok(out) = out else {
            // The document may be half-applied: forget it and disconnect everyone.
            *g = None;
            drop(g);
            self.kick(None, close::SERVER_ERROR, "server error");
            return Err(ApiError::internal("canvas transaction panicked"));
        };
        let Some(update) = st.take_produced() else {
            return Ok(out);
        };
        match store::append(&state.db, &st.key, self.id, &update).await {
            Ok(seq) => {
                st.last_seq = seq;
                st.since_snapshot += 1;
            }
            Err(e) => {
                *g = None;
                drop(g);
                self.kick(None, close::SERVER_ERROR, "server error");
                return Err(e);
            }
        }
        let msg = Message::Sync(SyncMessage::Update(update)).encode_v1();
        let _ = self.tx.send(Frame::Data {
            origin,
            bytes: Bytes::from(msg),
            echo: false,
        });
        if st.since_snapshot >= COMPACT_EVERY {
            let snapshot = st
                .doc
                .transact()
                .encode_state_as_update_v1(&StateVector::default());
            match store::compact(&state.db, &st.key, self.id, st.last_seq, &snapshot).await {
                Ok(deleted) => {
                    tracing::debug!(canvas_id = %self.id, deleted, "canvas compacted");
                    st.since_snapshot = 0;
                }
                Err(e) => {
                    tracing::warn!(canvas_id = %self.id, error = %e, "canvas compaction failed");
                }
            }
        }
        drop(g);
        self.notify_update(state);
        Ok(out)
    }

    /// Applies a remote (client) lib0-v1 update.
    pub(crate) async fn apply_remote(
        self: &Arc<Self>,
        state: &SharedState,
        origin: u64,
        update: &[u8],
    ) -> Result<(), ApiError> {
        self.transact(state, origin, |txn| {
            let decoded = Update::decode_v1(update).map_err(|e| format!("invalid update: {e}"))?;
            txn.apply_update(decoded)
                .map_err(|e| format!("update rejected: {e}"))
        })
        .await?
        .map_err(ApiError::BadRequest)
    }

    /// The `SyncStep2` reply to a client's `SyncStep1`.
    pub(crate) async fn sync_step2(
        &self,
        state: &AppState,
        sv: &StateVector,
    ) -> Result<Vec<u8>, ApiError> {
        let g = self.read(state).await?;
        let update = g.doc.transact().encode_state_as_update_v1(sv);
        Ok(Message::Sync(SyncMessage::SyncStep2(update)).encode_v1())
    }

    /// Messages sent to a new peer: `SyncStep1` (our state vector) and, if anyone is present,
    /// the current awareness states.
    pub(crate) async fn greeting(&self, state: &AppState) -> Result<Vec<Bytes>, ApiError> {
        let sv = self.read(state).await?.doc.transact().state_vector();
        let mut out = vec![Bytes::from(
            Message::Sync(SyncMessage::SyncStep1(sv)).encode_v1(),
        )];
        if let Some(msg) = self.awareness_message() {
            out.push(Bytes::from(msg));
        }
        Ok(out)
    }

    /// The current awareness states as one message (`None` when empty).
    pub(crate) fn awareness_message(&self) -> Option<Vec<u8>> {
        let a = lock(&self.awareness);
        let update = a.update().ok()?;
        (!update.clients.is_empty()).then(|| Message::Awareness(update).encode_v1())
    }

    /// Applies an awareness update from peer `origin` and relays the changed states.
    pub(crate) fn apply_awareness(
        &self,
        origin: u64,
        update: AwarenessUpdate,
        controlled: &mut HashSet<ClientID>,
    ) -> Result<(), ApiError> {
        let new_ids = update
            .clients
            .keys()
            .filter(|id| !controlled.contains(id))
            .count();
        if controlled.len() + new_ids > MAX_AWARENESS_CLIENTS_PER_PEER {
            return Err(ApiError::bad_request(
                "too many awareness clients on one connection",
            ));
        }
        let msg = {
            let mut a = lock(&self.awareness);
            let Some(summary) = a
                .apply_update_summary(update)
                .map_err(|e| ApiError::bad_request(format!("invalid awareness update: {e}")))?
            else {
                return Ok(());
            };
            for id in summary.added.iter().chain(&summary.updated) {
                controlled.insert(*id);
            }
            for id in &summary.removed {
                controlled.remove(id);
            }
            let changed = summary.all_changes();
            match a.update_with_clients(changed) {
                Ok(u) => Message::Awareness(u).encode_v1(),
                Err(_) => return Ok(()),
            }
        };
        let _ = self.tx.send(Frame::Data {
            origin,
            bytes: Bytes::from(msg),
            echo: true,
        });
        Ok(())
    }

    /// Registers a peer and returns its broadcast receiver.
    pub(crate) fn join(&self) -> broadcast::Receiver<Frame> {
        self.peers.fetch_add(1, Ordering::AcqRel);
        metrics::gauge!("canvas_peers").increment(1.0);
        self.touch();
        self.tx.subscribe()
    }

    /// Unregisters a peer: clears the awareness states it controlled (telling the others) and
    /// schedules eviction once the room is idle.
    pub(crate) fn leave(self: &Arc<Self>, origin: u64, controlled: &HashSet<ClientID>) {
        if !controlled.is_empty() {
            let msg = {
                let mut a = lock(&self.awareness);
                for id in controlled {
                    a.remove_state(*id);
                }
                a.update_with_clients(controlled.iter().copied())
                    .ok()
                    .map(|u| Message::Awareness(u).encode_v1())
            };
            if let Some(msg) = msg {
                let _ = self.tx.send(Frame::Data {
                    origin,
                    bytes: Bytes::from(msg),
                    echo: false,
                });
            }
        }
        self.peers.fetch_sub(1, Ordering::AcqRel);
        metrics::gauge!("canvas_peers").decrement(1.0);
        self.touch();
    }

    /// Asks every peer of `user` (or every peer) to disconnect.
    pub(crate) fn kick(&self, user: Option<Uuid>, code: u16, reason: &'static str) {
        let _ = self.tx.send(Frame::Kick { user, code, reason });
    }

    pub(crate) fn peer_count(&self) -> usize {
        self.peers.load(Ordering::Acquire)
    }

    /// Records activity and makes sure an eviction check is pending.
    fn touch(&self) {
        *lock(&self.last_active) = Instant::now();
        if !self.evict_scheduled.swap(true, Ordering::AcqRel) {
            let id = self.id;
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(evict_loop(id));
            } else {
                self.evict_scheduled.store(false, Ordering::Release);
            }
        }
    }

    fn idle_for(&self) -> Duration {
        lock(&self.last_active).elapsed()
    }

    /// Publishes `Event::Canvas { kind: "update" }` to every member, at most once per
    /// [`EVENT_THROTTLE`] (leading edge + one trailing publish).
    fn notify_update(self: &Arc<Self>, state: &SharedState) {
        let now = Instant::now();
        let delay = {
            let mut t = lock(&self.throttle);
            match t.last {
                Some(last) if now.duration_since(last) < EVENT_THROTTLE => {
                    if t.trailing {
                        return;
                    }
                    t.trailing = true;
                    Some(EVENT_THROTTLE.saturating_sub(now.duration_since(last)))
                }
                _ => {
                    t.last = Some(now);
                    None
                }
            }
        };
        let state = Arc::clone(state);
        let weak: Weak<Room> = Arc::downgrade(self);
        let canvas_id = self.id;
        tokio::spawn(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
                if let Some(room) = weak.upgrade() {
                    let mut t = lock(&room.throttle);
                    t.trailing = false;
                    t.last = Some(Instant::now());
                }
            }
            if let Err(e) = publish_update(&state, canvas_id).await {
                tracing::warn!(%canvas_id, error = %e, "publishing canvas update event failed");
            }
        });
    }
}

/// Bumps `canvases.updated_at` and tells every member something changed.
async fn publish_update(state: &AppState, canvas_id: Uuid) -> Result<(), ApiError> {
    let members: Vec<Uuid> = sqlx::query_scalar(
        "WITH bump AS (UPDATE canvases SET updated_at = now() WHERE id = $1)
         SELECT user_id FROM canvas_members WHERE canvas_id = $1",
    )
    .bind(canvas_id)
    .fetch_all(&state.db)
    .await?;
    for user_id in members {
        state.events.publish(
            user_id,
            Event::Canvas {
                canvas_id,
                kind: "update".into(),
            },
        );
    }
    Ok(())
}

async fn evict_loop(id: Uuid) {
    let mut wait = IDLE_EVICT;
    loop {
        tokio::time::sleep(wait).await;
        match rooms().try_evict(id, false) {
            EvictOutcome::Evicted | EvictOutcome::Gone => return,
            EvictOutcome::Busy(remaining) => {
                wait = remaining.max(Duration::from_secs(1));
            }
        }
    }
}

enum EvictOutcome {
    Evicted,
    Gone,
    Busy(Duration),
}

/// Live room counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RoomsMetrics {
    /// Rooms in memory.
    pub rooms: usize,
    /// Connected WebSocket peers across all rooms.
    pub peers: usize,
}

/// The process-wide room registry.
pub(crate) struct Rooms {
    map: StdMutex<HashMap<Uuid, Arc<Room>>>,
}

static ROOMS: LazyLock<Rooms> = LazyLock::new(|| Rooms {
    map: StdMutex::new(HashMap::new()),
});

pub(crate) fn rooms() -> &'static Rooms {
    &ROOMS
}

impl Rooms {
    /// The room for `id`, created (unloaded) if needed.
    pub(crate) fn get(&self, id: Uuid) -> Arc<Room> {
        let mut map = lock(&self.map);
        let room = map.entry(id).or_insert_with(|| {
            metrics::gauge!("canvas_rooms").increment(1.0);
            Arc::new(Room::new(id))
        });
        Arc::clone(room)
    }

    /// The room for `id` if it is in memory.
    pub(crate) fn peek(&self, id: Uuid) -> Option<Arc<Room>> {
        lock(&self.map).get(&id).cloned()
    }

    /// Evicts `id` if nobody uses it. Only the registry may hold a reference: every user of a
    /// room holds an `Arc`, so `strong_count == 1` under the registry lock means no handler
    /// or peer can be mid-write.
    fn try_evict(&self, id: Uuid, force: bool) -> EvictOutcome {
        let mut map = lock(&self.map);
        let Some(room) = map.get(&id) else {
            return EvictOutcome::Gone;
        };
        let idle = room.idle_for();
        let unused = Arc::strong_count(room) == 1 && room.peer_count() == 0;
        if unused && (force || idle >= IDLE_EVICT) {
            map.remove(&id);
            metrics::gauge!("canvas_rooms").decrement(1.0);
            tracing::debug!(canvas_id = %id, "canvas room evicted");
            return EvictOutcome::Evicted;
        }
        if !unused {
            return EvictOutcome::Busy(IDLE_EVICT);
        }
        EvictOutcome::Busy(IDLE_EVICT.saturating_sub(idle))
    }

    /// Removes a room for good (canvas deleted) and disconnects its peers.
    pub(crate) fn close(&self, id: Uuid) {
        if let Some(room) = lock(&self.map).remove(&id) {
            room.closed.store(true, Ordering::Release);
            room.kick(None, close::DELETED, "canvas deleted");
            metrics::gauge!("canvas_rooms").decrement(1.0);
        }
    }

    /// Disconnects `user`'s peers from room `id` (membership revoked).
    pub(crate) fn kick_user(&self, id: Uuid, user: Uuid) {
        if let Some(room) = self.peek(id) {
            room.kick(Some(user), close::REVOKED, "access revoked");
        }
    }

    /// Disconnects `user`'s peers from every room (account disabled or deleted).
    pub(crate) fn kick_user_everywhere(&self, user: Uuid) {
        let all: Vec<Arc<Room>> = lock(&self.map).values().cloned().collect();
        for room in all {
            room.kick(Some(user), close::REVOKED, "access revoked");
        }
    }

    pub(crate) fn metrics(&self) -> RoomsMetrics {
        let map = lock(&self.map);
        RoomsMetrics {
            rooms: map.len(),
            peers: map.values().map(|r| r.peer_count()).sum(),
        }
    }
}

/// Current room/peer counts.
pub fn rooms_metrics() -> RoomsMetrics {
    rooms().metrics()
}

/// Graceful shutdown: tells every connected peer the server is going away (close 1001) so
/// clients reconnect promptly. Nothing needs flushing — every update is persisted before it
/// is broadcast.
pub fn close_all_rooms() {
    let all: Vec<Arc<Room>> = lock(&rooms().map).values().cloned().collect();
    for room in all {
        room.kick(None, close::GOING_AWAY, "server shutting down");
    }
}

/// Test hook: evicts the room for `canvas_id` now if no peer or request is using it.
/// Returns whether it was evicted (or was not in memory).
#[doc(hidden)]
pub fn evict_room_now(canvas_id: Uuid) -> bool {
    matches!(
        rooms().try_evict(canvas_id, true),
        EvictOutcome::Evicted | EvictOutcome::Gone
    )
}

/// Test hook: whether a room for `canvas_id` is in memory.
#[doc(hidden)]
pub fn room_in_memory(canvas_id: Uuid) -> bool {
    rooms().peek(canvas_id).is_some()
}
