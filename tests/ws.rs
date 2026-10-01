//! y-websocket rooms: live sync, awareness relay, persistence across eviction, compaction,
//! REST ops fan-out, membership revocation.

#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{add_sticky, eventually, set_prop, TestApp};
use copper_cloud_core::events::Event;
use futures::SinkExt as _;
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(5);

fn canvas_uuid(id: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(id).unwrap()
}

#[tokio::test]
async fn two_clients_sync_both_ways_and_relay_awareness() {
    let app = TestApp::new().await;
    let a = app.user("alice").await;
    let b = app.user("bert").await;
    let id = app.canvas(&a, "Live").await;
    app.share(&a, &id, &b).await;

    let mut pa = app.peer(&a, &id).await;
    let mut pb = app.peer(&b, &id).await;

    // A → B
    pa.edit(|txn| add_sticky(txn, "s1", "hello", 0.0)).await;
    assert!(
        pb.pump_until(T, |p| p.shape_prop("s1", "text") == Some(json!("hello")))
            .await,
        "B sees A's sticky"
    );
    // B → A
    pb.edit(|txn| set_prop(txn, "s1", "text", "hello back"))
        .await;
    assert!(
        pa.pump_until(T, |p| p.shape_prop("s1", "text")
            == Some(json!("hello back")))
            .await,
        "A sees B's edit"
    );

    // Awareness: A's presence reaches B; leaving clears it.
    let a_client = pa.client_id();
    pa.set_awareness(
        1,
        r#"{"user":{"name":"alice","color":"pink"},"cursor":{"x":1,"y":2}}"#,
    )
    .await;
    assert!(
        pb.pump_until(T, |p| p
            .awareness
            .get(&a_client)
            .is_some_and(|j| j.contains("alice")))
            .await,
        "B sees A's awareness"
    );
    // A late joiner gets the current awareness in its greeting.
    let c = app.user("cleo").await;
    app.share(&a, &id, &c).await;
    let mut pc = app.peer(&c, &id).await;
    assert!(
        pc.pump_until(T, |p| p.awareness.contains_key(&a_client))
            .await
    );
    assert_eq!(pc.shape_prop("s1", "text"), Some(json!("hello back")));

    pa.close().await;
    assert!(
        pb.pump_until(T, |p| p
            .awareness
            .get(&a_client)
            .is_some_and(|j| j == "null"))
            .await,
        "A's awareness is removed when A disconnects"
    );
    assert!(copper_cloud_canvas::rooms_metrics().peers >= 2);
    pb.close().await;
    pc.close().await;

    // The server copy matches.
    let doc = app.state_doc(&a, &id).await;
    assert_eq!(
        common::shape_prop(&doc, "s1", "text"),
        Some(json!("hello back"))
    );
}

#[tokio::test]
async fn personal_canvas_room_over_ws() {
    let app = TestApp::new().await;
    let a = app.user("solo").await;
    let mut p1 = app.peer(&a, "personal").await;
    p1.edit(|txn| add_sticky(txn, "me", "private", 0.0)).await;
    let mut p2 = app.peer(&a, "personal").await;
    assert!(
        p2.pump_until(T, |p| p.shape_prop("me", "text") == Some(json!("private")))
            .await
    );
    p1.close().await;
    p2.close().await;
}

#[tokio::test]
async fn persistence_survives_room_eviction() {
    let app = TestApp::new().await;
    let a = app.user("keeper").await;
    let id = app.canvas(&a, "Durable").await;
    let cid = canvas_uuid(&id);

    let mut p = app.peer(&a, &id).await;
    for i in 0..3 {
        let sid = format!("n{i}");
        p.edit(|txn| add_sticky(txn, &sid, &format!("note {i}"), f64::from(i) * 220.0))
            .await;
    }
    // Wait until the server has applied (and therefore persisted) all three.
    assert!(
        eventually(T, || async {
            common::shape_count(&app.state_doc(&a, &id).await) == 3
        })
        .await
    );
    p.close().await;

    assert!(
        eventually(T, || async { copper_cloud_canvas::evict_room_now(cid) }).await,
        "room evicts once the peer is gone"
    );
    assert!(!copper_cloud_canvas::room_in_memory(cid));

    // Reload from Postgres.
    let doc = app.state_doc(&a, &id).await;
    assert_eq!(common::shape_count(&doc), 3);
    assert_eq!(
        common::shape_prop(&doc, "n2", "text"),
        Some(json!("note 2"))
    );
    assert!(copper_cloud_canvas::room_in_memory(cid));

    // A new peer syncs the persisted content.
    let p2 = app.peer(&a, &id).await;
    assert_eq!(p2.shape_count(), 3);
    p2.close().await;

    // Stored blobs are sealed: no plaintext in the table.
    let blobs: Vec<Vec<u8>> =
        sqlx::query_scalar(r#"SELECT "update" FROM canvas_updates WHERE canvas_id = $1"#)
            .bind(cid)
            .fetch_all(app.db())
            .await
            .unwrap();
    assert!(!blobs.is_empty());
    for blob in blobs {
        assert!(
            !blob.windows(6).any(|w| w == b"note 1"),
            "update stored in plaintext"
        );
    }
}

#[tokio::test]
async fn compaction_after_200_updates() {
    let app = TestApp::new().await;
    let a = app.user("busy").await;
    let id = app.canvas(&a, "Busy").await;
    let cid = canvas_uuid(&id);

    let mut p = app.peer(&a, &id).await;
    let n = 250;
    for i in 0..n {
        let sid = format!("s{i}");
        p.edit(|txn| add_sticky(txn, &sid, "x", f64::from(i))).await;
    }
    assert!(
        eventually(Duration::from_secs(20), || async {
            let (_, read) = app.get(&a, &format!("/canvases/{id}/read")).await;
            read["shapes"].as_array().map_or(0, Vec::len) == n as usize
        })
        .await,
        "all updates applied"
    );

    let snapshot: Option<i64> =
        sqlx::query_scalar("SELECT seq FROM canvas_snapshots WHERE canvas_id = $1")
            .bind(cid)
            .fetch_optional(app.db())
            .await
            .unwrap();
    let snap_seq = snapshot.expect("a snapshot exists after > 200 updates");
    let (remaining, min_seq): (i64, Option<i64>) =
        sqlx::query_as("SELECT count(*), min(seq) FROM canvas_updates WHERE canvas_id = $1")
            .bind(cid)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(
        remaining < 200,
        "older updates were deleted ({remaining} left)"
    );
    assert!(
        min_seq.is_none_or(|m| m > snap_seq),
        "no update at or below the snapshot seq"
    );
    p.close().await;

    // Snapshot + tail reload to the same content.
    assert!(eventually(T, || async { copper_cloud_canvas::evict_room_now(cid) }).await);
    let doc = app.state_doc(&a, &id).await;
    assert_eq!(common::shape_count(&doc), n);
}

#[tokio::test]
async fn ops_endpoint_reaches_ws_peers() {
    let app = TestApp::new().await;
    let a = app.user("agent-owner").await;
    let id = app.canvas(&a, "Agents").await;
    let base = format!("/canvases/{id}");
    let mut events = app.state.events.subscribe(a.id);

    let mut p = app.peer(&a, &id).await;
    let (s, r) = app
        .post(
            &a,
            &format!("{base}/ops"),
            json!({
                "as": { "name": "Planner", "color": "purple" },
                "ops": [
                    { "op": "add", "shape": { "type": "sticky", "text": "idea" } },
                    { "op": "add", "shape": { "id": "t1", "type": "text", "text": "title", "x": 0, "y": -300 } },
                    { "op": "bogus" }
                ]
            }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["applied"], 2, "{r}");
    assert_eq!(r["errors"][0]["index"], 2);
    let sticky = r["ids"][0].as_str().unwrap().to_owned();
    assert_eq!(r["ids"][1], "t1");
    assert!(
        p.pump_until(T, |p| p.shape_prop(&sticky, "text") == Some(json!("idea"))
            && p.shape_prop("t1", "type") == Some(json!("text")))
            .await
    );
    assert_eq!(p.shape_prop(&sticky, "by"), Some(json!("Planner")));
    assert_eq!(p.shape_prop(&sticky, "color"), Some(json!("yellow")));

    // connect / update / move / resize, then delete.
    let (s, r) = app
        .post(
            &a,
            &format!("{base}/ops"),
            json!({ "ops": [
                { "op": "connect", "from": sticky, "to": "t1", "label": "explains" },
                { "op": "update", "id": sticky, "patch": { "text": "better idea", "color": "green" } },
                { "op": "move", "id": "t1", "dx": 10, "dy": 20 },
                { "op": "resize", "id": sticky, "w": 260, "h": 180 }
            ]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["applied"], 4, "{r}");
    let arrow = r["ids"][0].as_str().unwrap().to_owned();
    assert!(
        p.pump_until(T, |p| p.shape_prop(&sticky, "text")
            == Some(json!("better idea"))
            && p.shape_prop("t1", "y") == Some(json!(-280))
            && p.shape_prop(&sticky, "w") == Some(json!(260))
            && p.shape_prop(&arrow, "label") == Some(json!("explains")))
            .await
    );
    assert_eq!(p.shape_prop(&arrow, "from"), Some(json!({ "ref": sticky })));
    assert_eq!(
        p.shape_prop(&sticky, "by"),
        Some(json!("Planner")),
        "by is the creator"
    );
    assert_eq!(
        p.shape_prop(&arrow, "by"),
        Some(json!("agent-owner")),
        "without `as`, by = the caller's display name"
    );

    let (s, r) = app
        .post(
            &a,
            &format!("{base}/ops"),
            json!({ "ops": [{ "op": "delete", "id": "t1" }] }),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["applied"], 1);
    assert!(
        p.pump_until(T, |p| p.shape_prop("t1", "type").is_none()
            && p.shape_prop(&arrow, "type").is_none())
            .await,
        "delete cascades to the arrow"
    );

    // clear needs confirmation.
    let (_, r) = app
        .post(
            &a,
            &format!("{base}/ops"),
            json!({ "ops": [{ "op": "clear" }] }),
        )
        .await;
    assert_eq!(r["applied"], 0);
    assert_eq!(p.shape_count(), 1);
    let (_, r) = app
        .post(
            &a,
            &format!("{base}/ops"),
            json!({ "ops": [{ "op": "clear" }], "confirm": true }),
        )
        .await;
    assert_eq!(r["applied"], 1);
    assert!(p.pump_until(T, |p| p.shape_count() == 0).await);

    // Agent presence: `writing` with the cursor on the work, then `idle` after 1.5 s.
    let (_, read) = app.get(&a, &format!("{base}/read")).await;
    let agents = read["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "{read}");
    assert_eq!(agents[0]["id"], "agent:planner");
    assert_eq!(agents[0]["name"], "Planner");
    assert_eq!(agents[0]["color"], "purple");
    assert!(agents[0]["cursor"]["x"].is_number());
    assert!(
        eventually(Duration::from_secs(4), || async {
            let (_, read) = app.get(&a, &format!("{base}/read")).await;
            read["agents"][0]["status"] == "idle"
        })
        .await,
        "agent goes idle"
    );

    // Envelope errors come back as index -1.
    let (s, r) = app
        .post(&a, &format!("{base}/ops"), json!({ "ops": [], "as": 5 }))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["errors"][0]["index"], -1);
    let too_many: Vec<Value> = (0..501).map(|_| json!({ "op": "clear" })).collect();
    let (s, r) = app
        .post(&a, &format!("{base}/ops"), json!({ "ops": too_many }))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["applied"], 0);
    assert!(r["errors"][0]["error"]
        .as_str()
        .unwrap()
        .contains("at most 500"));

    // Members were told the canvas changed (throttled).
    let got = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            match events.recv().await {
                Ok(Event::Canvas { canvas_id, kind })
                    if canvas_id.to_string() == id && kind == "update" =>
                {
                    return true;
                }
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    })
    .await;
    assert_eq!(got, Ok(true));
    p.close().await;
}

#[tokio::test]
async fn revoked_and_deleted_canvases_close_sockets() {
    let app = TestApp::new().await;
    let a = app.user("boss").await;
    let b = app.user("temp").await;
    let id = app.canvas(&a, "Short-lived").await;
    app.share(&a, &id, &b).await;

    let mut pb = app.peer(&b, &id).await;
    let mut pa = app.peer(&a, &id).await;
    let (s, _) = app
        .delete(&a, &format!("/canvases/{id}/members/{}", b.id))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        pb.wait_closed(T).await,
        Some(copper_cloud_canvas::close_codes::REVOKED)
    );
    // The owner's socket stays up.
    pa.edit(|txn| add_sticky(txn, "still", "here", 0.0)).await;
    assert!(
        eventually(T, || async {
            common::shape_count(&app.state_doc(&a, &id).await) == 1
        })
        .await
    );

    let (s, _) = app.delete(&a, &format!("/canvases/{id}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        pa.wait_closed(T).await,
        Some(copper_cloud_canvas::close_codes::DELETED)
    );
    assert!(!copper_cloud_canvas::room_in_memory(canvas_uuid(&id)));
}

#[tokio::test]
async fn invalid_frames_close_the_peer() {
    let app = TestApp::new().await;
    let a = app.user("fuzzer").await;
    let id = app.canvas(&a, "Fuzz").await;
    let mut p = app.peer(&a, &id).await;
    // Sync message with an unknown sub-type.
    p.ws.send(tokio_tungstenite::tungstenite::Message::Binary(
        vec![0u8, 9, 1, 2].into(),
    ))
    .await
    .unwrap();
    assert_eq!(
        p.wait_closed(T).await,
        Some(copper_cloud_canvas::close_codes::INVALID)
    );
    // The room still works for others.
    let mut q = app.peer(&a, &id).await;
    q.edit(|txn| add_sticky(txn, "ok", "fine", 0.0)).await;
    assert!(
        eventually(T, || async {
            common::shape_count(&app.state_doc(&a, &id).await) == 1
        })
        .await
    );
    q.close().await;
}

#[tokio::test]
async fn oversized_messages_are_refused() {
    let app = TestApp::new().await;
    let a = app.user("huge").await;
    let id = app.canvas(&a, "Huge").await;
    let mut p = app.peer(&a, &id).await;
    let big = vec![0u8; copper_cloud_canvas::MAX_MESSAGE_BYTES + 1];
    let _ =
        p.ws.send(tokio_tungstenite::tungstenite::Message::Binary(big.into()))
            .await;
    let code = p.wait_closed(T).await;
    eprintln!("close code: {code:?}");
    assert_eq!(code, Some(1009));
}
