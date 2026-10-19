//! Canvas chat (0.6.0): the client-owned top-level `chat` Y.Array is relayed live, persisted,
//! and reloaded untouched after the room is dropped from memory, after a fresh server state
//! (new pool + app, nothing cached), and across snapshot compaction; `/read` shows the last 50.

#![allow(clippy::too_many_lines)]

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{eventually, Peer, TestApp};
use copper_cloud_canvas::schema::{json_to_any, out_to_json};
use serde_json::{json, Value};
use yrs::{Array as _, Doc, ReadTxn as _, Transact as _, TransactionMut, WriteTxn as _};

const T: Duration = Duration::from_secs(5);

fn canvas_uuid(id: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(id).unwrap()
}

/// A message as the Copper page stores it (a plain object).
fn message(i: usize, author: &str, mentions: &[&str]) -> Value {
    json!({
        "id": format!("m{i}"),
        "authorId": format!("{author}-id"),
        "authorName": author,
        "text": format!("message {i} from {author}"),
        "mentions": mentions,
        "at": 1_790_000_000_000_i64 + i64::try_from(i).unwrap(),
    })
}

fn push(txn: &mut TransactionMut, msg: &Value) {
    let chat = txn.get_or_insert_array("chat");
    chat.push_back(txn, json_to_any(msg));
}

/// The `chat` array of `doc` as JSON (empty when absent).
fn chat_of(doc: &Doc) -> Vec<Value> {
    let txn = doc.transact();
    txn.get_array("chat").map_or_else(Vec::new, |a| {
        a.iter(&txn).map(|o| out_to_json(&txn, &o)).collect()
    })
}

async fn send(p: &mut Peer, msg: Value) {
    p.edit(|txn| push(txn, &msg)).await;
}

#[tokio::test]
async fn chat_persists_across_eviction_and_restart() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let ben = app.user("ben").await;
    let id = app.canvas(&ann, "Team dinners").await;
    let cid = canvas_uuid(&id);
    app.share(&ann, &id, &ben).await;
    let ben_id = ben.id.to_string();
    let to_ben = [ben_id.as_str()];

    // Two live peers: messages typed by one appear at the other.
    let mut pa = app.peer(&ann, &id).await;
    let mut pb = app.peer(&ben, &id).await;
    send(&mut pa, message(1, "ann", &[])).await;
    send(&mut pa, message(2, "ann", &to_ben)).await;
    assert!(pb.pump_until(T, |p| chat_of(&p.doc).len() == 2).await);
    send(&mut pb, message(3, "ben", &[])).await;
    assert!(
        pb.pump_until(T, |p| chat_of(&p.doc).len() == 3).await,
        "ben sees all three"
    );
    assert!(pa.pump_until(T, |p| chat_of(&p.doc).len() == 3).await);
    let live = chat_of(&pa.doc);
    assert_eq!(live, chat_of(&pb.doc));
    assert_eq!(live[1], message(2, "ann", &to_ben));

    // A REST ops write (server-side transaction on `shapes`) leaves the chat alone.
    let (st, r) = app
        .post(
            &ann,
            &format!("/canvases/{id}/ops"),
            json!({ "ops": [{ "op": "add", "shape": { "id": "s1", "type": "sticky", "text": "RSVP" } }] }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert!(pb.pump_until(T, |p| p.shape_count() == 1).await);
    assert_eq!(chat_of(&pb.doc), live);

    // /read carries the chat (oldest first) next to the shapes.
    let (st, read) = app.get(&ann, &format!("/canvases/{id}/read")).await;
    assert_eq!(st, StatusCode::OK, "{read}");
    assert_eq!(read["count"], 1);
    let chat = read["chat"].as_array().unwrap();
    let got: Vec<&str> = chat.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(got, vec!["m1", "m2", "m3"]);
    assert_eq!(chat[1], message(2, "ann", &to_ben));

    // Drop the room from memory and reload it from Postgres.
    pa.close().await;
    pb.close().await;
    assert!(
        eventually(T, || async { copper_cloud_canvas::evict_room_now(cid) }).await,
        "room evicts once the peers are gone"
    );
    assert!(!copper_cloud_canvas::room_in_memory(cid));
    assert_eq!(chat_of(&app.state_doc(&ann, &id).await), live);
    assert!(eventually(T, || async { copper_cloud_canvas::evict_room_now(cid) }).await);

    // A restarted server (same database and master key; fresh pool, state, router and
    // listener; nothing in memory) serves the same chat over REST and over the socket.
    let app2 = app.restart().await;
    assert!(!copper_cloud_canvas::room_in_memory(cid));
    assert_eq!(chat_of(&app2.state_doc(&ben, &id).await), live);
    let mut pc = app2.peer(&ben, &id).await;
    assert_eq!(chat_of(&pc.doc), live, "reconnecting peer syncs the chat");
    assert_eq!(pc.shape_count(), 1);

    // The client caps the array by deleting the oldest when it appends; that persists too.
    pc.edit(|txn| {
        push(txn, &message(4, "ben", &[]));
        let chat = txn.get_or_insert_array("chat");
        chat.remove(txn, 0);
    })
    .await;
    // Edit a message in place (replace the element), as an edit/delete would.
    pc.edit(|txn| {
        let chat = txn.get_or_insert_array("chat");
        chat.remove(txn, 0);
        let mut edited = message(2, "ann", &[]);
        edited["text"] = json!("message 2, edited");
        edited["editedAt"] = json!(1_790_000_000_500_i64);
        chat.insert(txn, 0, json_to_any(&edited));
    })
    .await;
    let after = chat_of(&pc.doc);
    assert_eq!(
        after
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["m2", "m3", "m4"]
    );
    assert!(
        eventually(T, || async {
            chat_of(&app2.state_doc(&ann, &id).await) == after
        })
        .await,
        "server applied the cap and the edit"
    );
    pc.close().await;
    assert!(eventually(T, || async { copper_cloud_canvas::evict_room_now(cid) }).await);
    assert_eq!(chat_of(&app2.state_doc(&ann, &id).await), after);
    let (_, read) = app2.get(&ann, &format!("/canvases/{id}/read")).await;
    assert_eq!(read["chat"][0]["text"], "message 2, edited");
    assert_eq!(read["chat"][0]["editedAt"], 1_790_000_000_500_i64);

    // Chat bytes are sealed in the update log like the rest of the document.
    let blobs: Vec<Vec<u8>> =
        sqlx::query_scalar(r#"SELECT "update" FROM canvas_updates WHERE canvas_id = $1"#)
            .bind(cid)
            .fetch_all(app2.db())
            .await
            .unwrap();
    assert_ne!(blobs.len(), 0);
    for b in blobs {
        assert!(
            !b.windows(9).any(|w| w == b"message 3"),
            "chat stored in plaintext"
        );
    }
}

#[tokio::test]
async fn chat_survives_compaction_and_read_shows_the_last_50() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let id = app.canvas(&ann, "Chatty").await;
    let cid = canvas_uuid(&id);

    // No chat yet: `chat` is present and empty.
    let (st, read) = app.get(&ann, &format!("/canvases/{id}/read")).await;
    assert_eq!(st, StatusCode::OK, "{read}");
    assert_eq!(read["chat"], json!([]));

    // One update per message, past the compaction threshold (200).
    let n = 230;
    let mut p = app.peer(&ann, &id).await;
    for i in 0..n {
        send(&mut p, message(i, "ann", &[])).await;
    }
    // A deleted message and a long one at the end.
    let mut gone = message(n, "ann", &[]);
    gone["deleted"] = json!(true);
    gone["text"] = json!("");
    send(&mut p, gone).await;
    let mut long = message(n + 1, "ann", &[]);
    long["text"] = json!("z".repeat(900));
    send(&mut p, long).await;
    assert!(
        eventually(Duration::from_secs(20), || async {
            chat_of(&app.state_doc(&ann, &id).await).len() == n + 2
        })
        .await,
        "all messages applied"
    );
    p.close().await;
    let snapshot: Option<i64> =
        sqlx::query_scalar("SELECT seq FROM canvas_snapshots WHERE canvas_id = $1")
            .bind(cid)
            .fetch_optional(app.db())
            .await
            .unwrap();
    assert!(snapshot.is_some(), "a snapshot exists after > 200 updates");

    assert!(eventually(T, || async { copper_cloud_canvas::evict_room_now(cid) }).await);
    let reloaded = chat_of(&app.state_doc(&ann, &id).await);
    assert_eq!(reloaded.len(), n + 2);
    assert_eq!(reloaded[0], message(0, "ann", &[]));
    assert_eq!(reloaded[n - 1], message(n - 1, "ann", &[]));

    // /read: the last 50 non-deleted messages, oldest first, text cut unless `full`.
    let (_, read) = app.get(&ann, &format!("/canvases/{id}/read")).await;
    let chat = read["chat"].as_array().unwrap();
    assert_eq!(chat.len(), 50);
    assert_eq!(chat[0]["id"], format!("m{}", n + 1 - 50));
    assert_eq!(chat[48]["id"], format!("m{}", n - 1));
    assert_eq!(chat[49]["id"], format!("m{}", n + 1));
    assert!(
        chat.iter().all(|m| m["id"] != format!("m{n}")),
        "deleted hidden"
    );
    assert_eq!(chat[49]["text"].as_str().unwrap().chars().count(), 501);
    let (_, full) = app
        .get(&ann, &format!("/canvases/{id}/read?full=true"))
        .await;
    assert_eq!(full["chat"][49]["text"].as_str().unwrap().len(), 900);
    // Filters narrow shapes, not the chat.
    let (_, only) = app
        .get(&ann, &format!("/canvases/{id}/read?types=frame"))
        .await;
    assert_eq!(only["chat"].as_array().unwrap().len(), 50);
}
