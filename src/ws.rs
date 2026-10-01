//! y-websocket peers: one read loop + one write task per connection.
//!
//! Wire format: every WebSocket *binary* message carries one or more y-protocol messages
//! (lib0 var-uint tag, then payload), exactly as `y-websocket` sends them:
//! `0` sync (`0` `SyncStep1` / `1` `SyncStep2` / `2` `Update`), `1` awareness, `2` auth,
//! `3` awareness query. Text frames are ignored.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as WsMessage, Utf8Bytes, WebSocket};
use bytes::Bytes;
use copper_cloud_core::error::ApiError;
use copper_cloud_core::state::SharedState;
use futures::stream::SplitSink;
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;
use yrs::encoding::read::Cursor;
use yrs::sync::{Message, MessageReader, SyncMessage};
use yrs::updates::decoder::DecoderV1;
use yrs::ClientID;

use crate::room::{next_peer_id, Frame, Room};

/// Largest accepted WebSocket message / frame. The page stores images as `data:` URLs of up
/// to 2 MiB, so one Yjs update can legitimately exceed 1 MiB; 8 MiB leaves room for a few.
pub const MAX_MESSAGE_BYTES: usize = 8 << 20;
/// Server → client ping interval.
pub const PING_EVERY: Duration = Duration::from_secs(30);
/// A peer that sends nothing (not even a pong) for this long is dropped.
pub const READ_TIMEOUT: Duration = Duration::from_secs(90);
/// A peer that does not accept a frame within this long is dropped.
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
/// Replies queued for one peer (`SyncStep2`, awareness query answers).
const DIRECT_CAPACITY: usize = 32;

/// Close codes sent by the server.
pub mod close {
    /// Normal closure.
    pub const NORMAL: u16 = 1000;
    /// Server shutting down / room closed.
    pub const GOING_AWAY: u16 = 1001;
    /// Undecodable y-protocol message or rejected update.
    pub const INVALID: u16 = 1007;
    /// Message or frame over [`super::MAX_MESSAGE_BYTES`].
    pub const TOO_BIG: u16 = 1009;
    /// Server-side failure; reconnect to resync.
    pub const SERVER_ERROR: u16 = 1011;
    /// The peer fell too far behind the room; reconnect to resync.
    pub const TOO_SLOW: u16 = 1013;
    /// The caller is no longer a member of the canvas (do not reconnect).
    pub const REVOKED: u16 = 4403;
    /// The canvas was deleted (do not reconnect).
    pub const DELETED: u16 = 4404;
}

enum Outgoing {
    Data(Bytes),
    Close(u16, &'static str),
}

/// Unregisters a peer from its room however the peer task ends (including a panic).
struct PeerGuard {
    room: Arc<Room>,
    peer: u64,
    controlled: HashSet<ClientID>,
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        self.room.leave(self.peer, &self.controlled);
    }
}

/// Serves one authenticated member on `room` until either side hangs up.
pub(crate) async fn serve_peer(
    state: SharedState,
    room: Arc<Room>,
    user_id: Uuid,
    socket: WebSocket,
) {
    let peer = next_peer_id();
    let bcast = room.join();
    let mut guard = PeerGuard {
        room: Arc::clone(&room),
        peer,
        controlled: HashSet::new(),
    };
    let (sink, mut stream) = socket.split();
    let (direct_tx, direct_rx) = mpsc::channel::<Outgoing>(DIRECT_CAPACITY);

    match room.greeting(&state).await {
        Ok(msgs) => {
            for m in msgs {
                let _ = direct_tx.try_send(Outgoing::Data(m));
            }
        }
        Err(e) => {
            tracing::warn!(canvas_id = %room.id, error = %e, "canvas greeting failed");
            let _ = direct_tx.try_send(Outgoing::Close(close::SERVER_ERROR, "load failed"));
        }
    }

    let mut writer = tokio::spawn(write_loop(sink, direct_rx, bcast, peer, user_id));
    let mut writer_done = false;
    loop {
        tokio::select! {
            _ = &mut writer => {
                writer_done = true;
                break;
            }
            next = tokio::time::timeout(READ_TIMEOUT, stream.next()) => {
                let msg = match next {
                    Ok(Some(Ok(msg))) => msg,
                    Ok(Some(Err(e))) => {
                        // tungstenite's capacity error: the frame/message is over the limit.
                        if e.to_string().contains("Space limit exceeded") {
                            let _ = direct_tx.try_send(Outgoing::Close(close::TOO_BIG, "message too big"));
                        }
                        break;
                    }
                    _ => break,
                };
                match msg {
                    WsMessage::Binary(bytes) => {
                        metrics::counter!("canvas_ws_bytes_in_total").increment(bytes.len() as u64);
                        if let Err(e) = handle_binary(&state, &room, peer, &bytes, &direct_tx, &mut guard.controlled).await {
                            let (code, reason) = match &e {
                                ApiError::BadRequest(_) => (close::INVALID, "invalid message"),
                                ApiError::NotFound => (close::DELETED, "canvas deleted"),
                                _ => (close::SERVER_ERROR, "server error"),
                            };
                            tracing::debug!(canvas_id = %room.id, peer, code, error = %e, "closing canvas peer");
                            let _ = direct_tx.send(Outgoing::Close(code, reason)).await;
                            break;
                        }
                    }
                    WsMessage::Close(_) => break,
                    WsMessage::Text(_) | WsMessage::Ping(_) | WsMessage::Pong(_) => {}
                }
            }
        }
    }
    drop(guard);
    drop(direct_tx);
    if !writer_done
        && tokio::time::timeout(Duration::from_secs(2), &mut writer)
            .await
            .is_err()
    {
        writer.abort();
    }
}

/// Decodes every y-protocol message in one WebSocket frame.
fn decode_frame(bytes: &[u8]) -> Result<Vec<Message>, ApiError> {
    std::panic::catch_unwind(|| {
        let mut decoder = DecoderV1::new(Cursor::new(bytes));
        MessageReader::new(&mut decoder).collect::<Result<Vec<_>, _>>()
    })
    .map_err(|_| ApiError::bad_request("undecodable message"))?
    .map_err(|e| ApiError::bad_request(format!("undecodable message: {e}")))
}

async fn handle_binary(
    state: &SharedState,
    room: &Arc<Room>,
    peer: u64,
    bytes: &[u8],
    direct: &mpsc::Sender<Outgoing>,
    controlled: &mut HashSet<ClientID>,
) -> Result<(), ApiError> {
    for msg in decode_frame(bytes)? {
        match msg {
            Message::Sync(SyncMessage::SyncStep1(sv)) => {
                let reply = room.sync_step2(state, &sv).await?;
                send_direct(direct, reply).await?;
            }
            Message::Sync(SyncMessage::SyncStep2(update) | SyncMessage::Update(update)) => {
                room.apply_remote(state, peer, &update).await?;
            }
            Message::Awareness(update) => room.apply_awareness(peer, update, controlled)?,
            Message::AwarenessQuery => {
                if let Some(reply) = room.awareness_message() {
                    send_direct(direct, reply).await?;
                }
            }
            // Auth is decided at upgrade time; custom tags are ignored.
            Message::Auth(_) | Message::Custom(..) => {}
        }
    }
    Ok(())
}

async fn send_direct(direct: &mpsc::Sender<Outgoing>, msg: Vec<u8>) -> Result<(), ApiError> {
    direct
        .send(Outgoing::Data(Bytes::from(msg)))
        .await
        .map_err(|_| ApiError::internal("peer writer gone"))
}

async fn write_loop(
    mut sink: SplitSink<WebSocket, WsMessage>,
    mut direct: mpsc::Receiver<Outgoing>,
    mut bcast: broadcast::Receiver<Frame>,
    peer: u64,
    user: Uuid,
) {
    let mut shutdown = copper_cloud_core::shutdown::subscribe();
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let close_with = loop {
        let out = tokio::select! {
            d = direct.recv() => match d {
                Some(Outgoing::Data(b)) => WsMessage::Binary(b),
                Some(Outgoing::Close(code, reason)) => break Some((code, reason)),
                None => break Some((close::NORMAL, "")),
            },
            f = bcast.recv() => match f {
                Ok(Frame::Data { origin, bytes, echo }) => {
                    if origin == peer && !echo {
                        continue;
                    }
                    WsMessage::Binary(bytes)
                }
                Ok(Frame::Kick { user: target, code, reason }) => {
                    if target.is_none_or(|u| u == user) {
                        break Some((code, reason));
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => break Some((close::TOO_SLOW, "too slow")),
                Err(broadcast::error::RecvError::Closed) => break Some((close::GOING_AWAY, "")),
            },
            _ = ping.tick() => WsMessage::Ping(Bytes::new()),
            () = copper_cloud_core::shutdown::wait(&mut shutdown) => {
                break Some((close::GOING_AWAY, "server shutting down"));
            }
        };
        if let WsMessage::Binary(b) = &out {
            metrics::counter!("canvas_ws_bytes_out_total").increment(b.len() as u64);
        }
        match tokio::time::timeout(SEND_TIMEOUT, sink.send(out)).await {
            Ok(Ok(())) => {}
            _ => break None,
        }
    };
    if let Some((code, reason)) = close_with {
        let frame = CloseFrame {
            code,
            reason: Utf8Bytes::from_static(reason),
        };
        let _ = tokio::time::timeout(SEND_TIMEOUT, sink.send(WsMessage::Close(Some(frame)))).await;
    }
}
