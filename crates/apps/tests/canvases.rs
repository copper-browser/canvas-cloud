//! `/v1/apps/self/canvases*` and `/v1/apps/self/stats`: listing (scope, filter, paging),
//! deleting (room closed, members told), the stored-document digest, and the counts.

#![allow(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements
)]

mod common;

use axum::http::StatusCode;
use base64::Engine as _;
use canvas_cloud_apps::digest::{document_digest, EMPTY_DOCUMENT_DIGEST};
use common::{add_sticky, eventually, TestApp, T};
use serde_json::{json, Value};
use uuid::Uuid;
use yrs::updates::decoder::Decode as _;
use yrs::{Doc, Transact as _, Update};

#[tokio::test]
async fn list_is_scoped_filtered_and_paged() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let other = t.app(false, false).await;
    let ann = t.provision(&app, "ann").await;
    let ben = t.provision(&app, "ben").await;
    let zed = t.provision(&other, "zed").await;
    let human = t.human("hum").await;
    let a1 = t.canvas(&ann, "a1").await;
    let a2 = t.canvas(&ann, "a2").await;
    t.share(&ann, &a2, &ben).await;
    let (s, link) = t
        .post(&ann, &format!("/canvases/{a2}/links"), json!({}))
        .await;
    assert!(s.is_success(), "{link}");
    let zed_canvas = t.canvas(&zed, "z").await;
    let human_canvas = t.canvas(&human, "h").await;
    t.share(&human, &human_canvas, &ann).await;
    // Ben's personal canvas (created on first use).
    let (s, v) = t.get(&ben, "/canvases").await;
    assert_eq!(s, StatusCode::OK, "{v}");

    let (s, v) = t.get(&app.me(), "/apps/self/canvases").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ids: Vec<&str> = v["canvases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&a1.as_str()) && ids.contains(&a2.as_str()),
        "{v}"
    );
    assert!(!ids.contains(&zed_canvas.as_str()), "another app's user");
    assert!(
        !ids.contains(&human_canvas.as_str()),
        "owned by a human (ann is a member)"
    );
    let total = v["total"].as_i64().unwrap();
    assert_eq!(total, i64::try_from(ids.len()).unwrap());
    // Newest first; owner and counts.
    let first = &v["canvases"][0];
    assert!(first.get("imported_at").is_some());
    let a2_row = v["canvases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == a2.as_str())
        .unwrap();
    assert_eq!(a2_row["kind"], "shared");
    assert_eq!(a2_row["owner"]["subject"], "ann");
    assert_eq!(a2_row["owner"]["email"], ann.email);
    assert_eq!(a2_row["member_count"], 2);
    assert_eq!(a2_row["share_link_count"], 1);
    assert_eq!(a2_row["imported_at"], Value::Null);

    let (_, shared) = t.get(&app.me(), "/apps/self/canvases?kind=shared").await;
    assert_eq!(shared["total"], 2, "{shared}");
    let (_, personal) = t.get(&app.me(), "/apps/self/canvases?kind=personal").await;
    assert!(personal["canvases"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["kind"] == "personal"));
    let (s, _) = t.get(&app.me(), "/apps/self/canvases?kind=nope").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, page) = t
        .get(&app.me(), "/apps/self/canvases?limit=1&offset=1")
        .await;
    assert_eq!(page["canvases"].as_array().unwrap().len(), 1);
    assert_eq!(page["canvases"][0]["id"], v["canvases"][1]["id"]);
}

#[tokio::test]
async fn delete_closes_the_room_and_tells_members() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let ann = t.provision(&app, "ann").await;
    let ben = t.provision(&app, "ben").await;
    let canvas = t.canvas(&ann, "doomed").await;
    t.share(&ann, &canvas, &ben).await;
    let mut peer = t.peer(&ben, &canvas).await;
    let mut ben_rx = t.state.events.subscribe(ben.user_id.unwrap());

    // Not this app's: 404 (and a bad id too).
    let other = t.app(false, false).await;
    let (s, _) = t
        .delete(&other.me(), &format!("/apps/self/canvases/{canvas}"))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = t.delete(&app.me(), "/apps/self/canvases/not-a-uuid").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, v) = t
        .delete(&app.me(), &format!("/apps/self/canvases/{canvas}"))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "ok": true, "id": canvas, "name": "doomed" }));
    assert_eq!(peer.wait_closed(T).await, Some(4404));
    let cid = Uuid::parse_str(&canvas).unwrap();
    assert!(
        eventually(T, || {
            let got = common::drain_kinds(&mut ben_rx);
            async move { got.contains(&(cid, "deleted".to_owned())) }
        })
        .await
    );
    let (s, _) = t.get(&ann, &format!("/canvases/{canvas}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = t
        .delete(&app.me(), &format!("/apps/self/canvases/{canvas}"))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_audit WHERE action = 'app.canvas.delete' AND target = $1",
    )
    .bind(&canvas)
    .fetch_one(t.db())
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

#[tokio::test]
async fn digest_matches_what_clients_see() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let ann = t.provision(&app, "ann").await;
    let canvas = t.canvas(&ann, "digest me").await;
    let path = format!("/apps/self/canvases/{canvas}/digest");

    let state_digest = |v: &Value| {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(v["state"].as_str().unwrap())
            .unwrap();
        let doc = Doc::new();
        doc.transact_mut()
            .apply_update(Update::decode_v1(&bytes).unwrap())
            .unwrap();
        document_digest(&doc)
    };

    let (s, v) = t.get(&app.me(), &path).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["canvas_id"], canvas);
    let d0 = v["digest"].as_str().unwrap().to_owned();
    assert_ne!(d0, EMPTY_DOCUMENT_DIGEST, "a new canvas has its meta");
    let (_, st) = t.get(&ann, &format!("/canvases/{canvas}/state")).await;
    assert_eq!(d0, state_digest(&st));

    // Live edits (update log) then a restart-free re-read.
    let mut peer = t.peer(&ann, &canvas).await;
    peer.edit(|txn| add_sticky(txn, "s1", "hello", 0.0)).await;
    peer.edit(|txn| add_sticky(txn, "s2", "world", 10.0)).await;
    let live = document_digest(&peer.doc);
    assert!(
        eventually(T, || {
            let t = &t;
            let me = app.me();
            let path = path.clone();
            let live = live.clone();
            async move { t.get(&me, &path).await.1["digest"] == live.as_str() }
        })
        .await,
        "the stored digest catches up with the live document"
    );
    peer.close().await;
    let (_, st) = t.get(&ann, &format!("/canvases/{canvas}/state")).await;
    assert_eq!(state_digest(&st), live);

    // Not this app's canvas, or unknown: 404.
    let human = t.human("h").await;
    let theirs = t.canvas(&human, "h").await;
    let (s, _) = t
        .get(&app.me(), &format!("/apps/self/canvases/{theirs}/digest"))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = t
        .get(
            &app.me(),
            &format!("/apps/self/canvases/{}/digest", Uuid::new_v4()),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stats_count_users_canvases_and_rooms() {
    let t = TestApp::new().await;
    let app = t.app(true, false).await;
    let (s, v) = t.get(&app.me(), "/apps/self/stats").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["users"], 0);
    assert_eq!(v["canvases"], 0);
    assert_eq!(v["allow_import"], true);
    assert_eq!(v["app"]["allow_import"], true);
    assert_eq!(v["app"]["trust_email"], false);
    assert_eq!(v["app"]["id"], app.id.to_string());
    assert_eq!(v["rooms"], json!({ "rooms": 0, "peers": 0 }));

    let ann = t.provision(&app, "ann").await;
    let ben = t.provision(&app, "ben").await;
    let canvas = t.canvas(&ann, "c").await;
    t.share(&ann, &canvas, &ben).await;
    let p1 = t.peer(&ann, &canvas).await;
    let p2 = t.peer(&ben, &canvas).await;
    // A room of someone else's canvas is not counted.
    let human = t.human("h").await;
    let theirs = t.canvas(&human, "h").await;
    let p3 = t.peer(&human, &theirs).await;
    let (s, v) = t.get(&app.me(), "/apps/self/stats").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["users"], 2);
    assert_eq!(v["canvases"], 1);
    assert_eq!(v["rooms"], json!({ "rooms": 1, "peers": 2 }), "{v}");
    p1.close().await;
    p2.close().await;
    p3.close().await;
}
