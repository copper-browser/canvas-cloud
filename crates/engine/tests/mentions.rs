//! Chat @mentions (0.6.0): `POST /canvases/{id}/mentions`, `GET /mentions`,
//! `POST /mentions/read` — authorization, target filtering, idempotency, read marking, SSE
//! events, sealing, rate limit.

#![allow(clippy::too_many_lines)]

mod common;

use axum::http::{Method, StatusCode};
use common::{TestApp, User};
use copper_cloud_canvas::mentions::{
    MAX_MENTIONS_LIMIT, MAX_MENTION_TARGETS, MENTION_POSTS_PER_MINUTE,
};
use copper_cloud_core::events::Event;
use serde_json::{json, Value};
use tokio::sync::broadcast::Receiver;
use uuid::Uuid;

/// Drains the mention-related `canvas` events queued for one subscriber: `(canvas_id, kind)`.
fn mention_events(rx: &mut Receiver<Event>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::Canvas { canvas_id, kind } = ev {
            if kind.starts_with("mention") {
                out.push((canvas_id.to_string(), kind));
            }
        }
    }
    out
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}

fn s(u: &User) -> String {
    u.id.to_string()
}

async fn mention(
    app: &TestApp,
    from: &User,
    canvas: &str,
    message_id: &str,
    to: &[&User],
    excerpt: &str,
) -> Value {
    let user_ids: Vec<String> = to.iter().map(|u| s(u)).collect();
    let (st, v) = app
        .post(
            from,
            &format!("/canvases/{canvas}/mentions"),
            json!({ "message_id": message_id, "user_ids": user_ids, "excerpt": excerpt }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

#[tokio::test]
async fn mentions_notify_members_only_once_and_list_newest_first() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let ben = app.user("ben").await;
    let cy = app.user("cy").await;
    let out = app.user("outsider").await;
    let id = app.canvas(&ann, "Team dinners").await;
    app.share(&ann, &id, &ben).await;
    app.share(&ann, &id, &cy).await;
    let mut ann_ev = app.state.events.subscribe(ann.id);
    let mut ben_ev = app.state.events.subscribe(ben.id);
    let mut cy_ev = app.state.events.subscribe(cy.id);
    let mut out_ev = app.state.events.subscribe(out.id);

    // Targets are filtered to current members other than the caller; duplicates collapse;
    // unknown ids are skipped. Order follows the request.
    let ghost = Uuid::now_v7().to_string();
    let path = format!("/canvases/{id}/mentions");
    let body = json!({
        "message_id": "msg-1",
        "user_ids": [s(&ben), s(&out), s(&ann), s(&ben), ghost, s(&cy)],
        "excerpt": "Dinner Tuesday?\n@ben @cy  who is in",
    });
    let (st, v) = app.post(&ann, &path, body.clone()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(ids(&v["notified"]), vec![s(&ben), s(&cy)]);
    assert_eq!(ids(&v["skipped"]), vec![s(&out), s(&ann), ghost.clone()]);
    let mentioned = vec![(id.clone(), "mention".to_owned())];
    assert_eq!(mention_events(&mut ben_ev), mentioned);
    assert_eq!(mention_events(&mut cy_ev), mentioned);
    assert_eq!(
        mention_events(&mut out_ev),
        vec![],
        "non-members hear nothing"
    );
    assert_eq!(
        mention_events(&mut ann_ev),
        vec![],
        "the sender is never notified"
    );

    // A retry answers the same and notifies nobody again (one row per recipient).
    let (st, again) = app.post(&ann, &path, body).await;
    assert_eq!(st, StatusCode::OK, "{again}");
    assert_eq!(again, v);
    assert_eq!(mention_events(&mut ben_ev), vec![]);
    assert_eq!(mention_events(&mut cy_ev), vec![]);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM canvas_mentions WHERE canvas_id = $1::uuid AND message_id = 'msg-1'",
    )
    .bind(&id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(rows, 2);

    // The recipient's list: canvas, sender, message id, cleaned excerpt, unread.
    let (st, list) = app.get(&ben, "/mentions").await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list["unread"], 1);
    let items = list["mentions"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{list}");
    let m = &items[0];
    assert_eq!(m["canvas"], json!({ "id": id, "name": "Team dinners" }));
    assert_eq!(
        m["from"],
        json!({ "id": s(&ann), "email": ann.email, "display_name": "ann" })
    );
    assert_eq!(m["message_id"], "msg-1");
    assert_eq!(m["excerpt"], "Dinner Tuesday? @ben @cy who is in");
    assert_eq!(m["read_at"], Value::Null);
    assert!(m["created_at"].as_str().unwrap().contains('T'));
    assert!(Uuid::parse_str(m["id"].as_str().unwrap()).is_ok());
    // Nobody else's mentions leak into a list.
    let (_, list) = app.get(&out, "/mentions").await;
    assert_eq!(list, json!({ "mentions": [], "unread": 0 }));
    let (_, list) = app.get(&ann, "/mentions").await;
    assert_eq!(list["mentions"], json!([]));

    // Newest first; a member may mention the owner back.
    mention(&app, &cy, &id, "msg-2", &[&ben, &ann], "second").await;
    let (_, list) = app.get(&ben, "/mentions").await;
    let order: Vec<&str> = list["mentions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["message_id"].as_str().unwrap())
        .collect();
    assert_eq!(order, vec!["msg-2", "msg-1"]);
    assert_eq!(list["unread"], 2);
    let (_, list) = app.get(&ann, "/mentions").await;
    assert_eq!(list["mentions"][0]["from"]["id"], s(&cy));
    assert_eq!(mention_events(&mut ann_ev), mentioned);

    // The same message id on another canvas is a different message.
    let other = app.canvas(&ann, "Other").await;
    app.share(&ann, &other, &ben).await;
    let v = mention(&app, &ann, &other, "msg-1", &[&ben], "elsewhere").await;
    assert_eq!(ids(&v["notified"]), vec![s(&ben)]);
    assert_eq!(
        mention_events(&mut ben_ev),
        vec![
            (id.clone(), "mention".to_owned()),
            (other.clone(), "mention".to_owned())
        ]
    );

    // Excerpts are sealed at rest, never plaintext.
    let blobs: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT excerpt_sealed FROM canvas_mentions WHERE canvas_id = $1::uuid")
            .bind(&id)
            .fetch_all(app.db())
            .await
            .unwrap();
    assert_ne!(blobs.len(), 0);
    for b in blobs {
        assert!(!b.windows(6).any(|w| w == b"Dinner" || w == b"second"));
    }

    // An empty or excerpt-less mention is fine; long excerpts are cut to 200 characters.
    let v = mention(&app, &ann, &id, "msg-3", &[], "").await;
    assert_eq!(v, json!({ "notified": [], "skipped": [] }));
    mention(&app, &ann, &id, "msg-4", &[&ben], &"é".repeat(500)).await;
    let (_, list) = app.get(&ben, "/mentions?limit=1").await;
    assert_eq!(list["mentions"][0]["message_id"], "msg-4");
    assert_eq!(
        list["mentions"][0]["excerpt"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        200
    );
}

#[tokio::test]
async fn mentions_are_scoped_to_members() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let ben = app.user("ben").await;
    let eve = app.user("eve").await;
    let id = app.canvas(&ann, "Private").await;
    app.share(&ann, &id, &ben).await;
    let path = format!("/canvases/{id}/mentions");
    let body = json!({ "message_id": "m", "user_ids": [s(&ben)], "excerpt": "hi" });

    // Non-members, unknown canvases and malformed ids are 404 — no existence leak.
    let (st, v) = app.post(&eve, &path, body.clone()).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["error"], "not_found");
    let ghost = Uuid::now_v7();
    let (st, _) = app
        .post(&ann, &format!("/canvases/{ghost}/mentions"), body.clone())
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = app
        .post(&ann, "/canvases/not-a-uuid/mentions", body.clone())
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // No session → 401.
    for (method, p) in [
        (Method::POST, path.as_str()),
        (Method::GET, "/mentions"),
        (Method::POST, "/mentions/read"),
    ] {
        let (st, v) = app.call(None, method, p, Some(json!({ "ids": [] }))).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{p}: {v}");
    }

    // The Personal canvas has no chat.
    let (st, v) = app.post(&ann, "/canvases/personal/mentions", body).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");

    // Validation.
    for (bad, why) in [
        (json!({ "user_ids": [s(&ben)] }), "message_id missing"),
        (
            json!({ "message_id": "  ", "user_ids": [] }),
            "empty message_id",
        ),
        (
            json!({ "message_id": "x".repeat(129), "user_ids": [] }),
            "long message_id",
        ),
        (
            json!({ "message_id": "m", "user_ids": ["nope"] }),
            "not a uuid",
        ),
        (
            json!({ "message_id": "m", "user_ids": (0..=MAX_MENTION_TARGETS).map(|_| Uuid::now_v7().to_string()).collect::<Vec<_>>() }),
            "too many targets",
        ),
    ] {
        let (st, v) = app.post(&ann, &path, bad).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{why}: {v}");
        assert_eq!(v["error"], "bad_request", "{why}");
    }

    // A member who left is skipped, and their old mentions disappear from their list (but come
    // back if they rejoin: the canvas is visible again).
    mention(&app, &ann, &id, "before", &[&ben], "before leaving").await;
    let (_, list) = app.get(&ben, "/mentions").await;
    assert_eq!(list["unread"], 1);
    let (st, _) = app
        .delete(&ben, &format!("/canvases/{id}/members/{}", ben.id))
        .await;
    assert_eq!(st, StatusCode::OK);
    let v = mention(&app, &ann, &id, "after", &[&ben], "after leaving").await;
    assert_eq!(v["notified"], json!([]));
    assert_eq!(ids(&v["skipped"]), vec![s(&ben)]);
    let (_, list) = app.get(&ben, "/mentions").await;
    assert_eq!(list, json!({ "mentions": [], "unread": 0 }));
    // ...and cannot be marked read from outside either.
    let (st, v) = app
        .post(&ben, "/mentions/read", json!({ "canvas_id": id }))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["updated"], 0);
    app.share(&ann, &id, &ben).await;
    let (_, list) = app.get(&ben, "/mentions").await;
    assert_eq!(list["unread"], 1);
    assert_eq!(list["mentions"][0]["message_id"], "before");

    // Deleting the canvas deletes its mentions.
    let (st, _) = app.delete(&ann, &format!("/canvases/{id}")).await;
    assert_eq!(st, StatusCode::OK);
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM canvas_mentions WHERE canvas_id = $1::uuid")
            .bind(&id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn marking_read_clears_badges_on_every_device() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let ben = app.user("ben").await;
    let cy = app.user("cy").await;
    let one = app.canvas(&ann, "One").await;
    let two = app.canvas(&ann, "Two").await;
    for c in [&one, &two] {
        app.share(&ann, c, &ben).await;
        app.share(&ann, c, &cy).await;
    }
    mention(&app, &ann, &one, "a", &[&ben, &cy], "first").await;
    mention(&app, &ann, &one, "b", &[&ben], "second").await;
    mention(&app, &ann, &two, "c", &[&ben], "third").await;
    let (_, list) = app.get(&ben, "/mentions").await;
    assert_eq!(list["unread"], 3);
    let id_of = |list: &Value, msg: &str| -> String {
        list["mentions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["message_id"] == msg)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let a_id = id_of(&list, "a");
    let mut ben_ev = app.state.events.subscribe(ben.id);
    let mut cy_ev = app.state.events.subscribe(cy.id);

    // By id: only the caller's own rows; one `mention_read` per canvas touched.
    let (st, v) = app
        .post(&cy, "/mentions/read", json!({ "ids": [a_id] }))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["updated"], 0, "Cy cannot mark Ben's mention");
    assert_eq!(mention_events(&mut cy_ev), vec![]);
    let (st, v) = app
        .post(&ben, "/mentions/read", json!({ "ids": [a_id] }))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "ok": true, "updated": 1 }));
    assert_eq!(
        mention_events(&mut ben_ev),
        vec![(one.clone(), "mention_read".to_owned())]
    );
    let (_, unread) = app.get(&ben, "/mentions?unread=1").await;
    assert_eq!(unread["unread"], 2);
    let msgs: Vec<&str> = unread["mentions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["message_id"].as_str().unwrap())
        .collect();
    assert_eq!(msgs, vec!["c", "b"]);
    // Cy's copy of the same message is untouched.
    let (_, cy_list) = app.get(&cy, "/mentions?unread=true").await;
    assert_eq!(cy_list["unread"], 1);

    // By canvas.
    let (st, v) = app
        .post(&ben, "/mentions/read", json!({ "canvas_id": two }))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["updated"], 1);
    assert_eq!(
        mention_events(&mut ben_ev),
        vec![(two.clone(), "mention_read".to_owned())]
    );
    // Again: nothing changes, no event.
    let (_, v) = app
        .post(&ben, "/mentions/read", json!({ "canvas_id": two }))
        .await;
    assert_eq!(v["updated"], 0);
    assert_eq!(mention_events(&mut ben_ev), vec![]);

    // The full list keeps read ones, with read_at set.
    let (_, all) = app.get(&ben, "/mentions?unread=0").await;
    assert_eq!(all["mentions"].as_array().unwrap().len(), 3);
    assert_eq!(all["unread"], 1);
    for m in all["mentions"].as_array().unwrap() {
        assert_eq!(m["read_at"].is_null(), m["message_id"] == "b", "{m}");
    }

    // Bad requests.
    for bad in [
        json!({}),
        json!({ "ids": ["nope"] }),
        json!({ "canvas_id": "personal" }),
    ] {
        let (st, v) = app.post(&ben, "/mentions/read", bad.clone()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
    for q in ["limit=0", "limit=-3", "unread=maybe", "limit=x"] {
        let (st, v) = app.get(&ben, &format!("/mentions?{q}")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{q}: {v}");
    }
    let (st, v) = app.get(&ben, "/mentions?limit=100000").await;
    assert_eq!(st, StatusCode::OK, "limit is capped, not refused: {v}");
    assert!(i64::try_from(v["mentions"].as_array().unwrap().len()).unwrap() <= MAX_MENTIONS_LIMIT);
}

#[tokio::test]
async fn posting_mentions_is_rate_limited_per_user() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let ben = app.user("ben").await;
    let id = app.canvas(&ann, "Busy").await;
    app.share(&ann, &id, &ben).await;
    let path = format!("/canvases/{id}/mentions");
    let mut limited = None;
    for i in 0..=MENTION_POSTS_PER_MINUTE {
        let (st, v) = app
            .post(
                &ann,
                &path,
                json!({ "message_id": format!("m{i}"), "user_ids": [s(&ben)], "excerpt": "hi" }),
            )
            .await;
        if st == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(v["error"], "rate_limited");
            limited = Some(i);
            break;
        }
        assert_eq!(st, StatusCode::OK, "{v}");
    }
    assert_eq!(limited, Some(MENTION_POSTS_PER_MINUTE), "burst then 429");
    // Per user: Ben is unaffected.
    let v = mention(&app, &ben, &id, "ben-1", &[&ann], "hi").await;
    assert_eq!(ids(&v["notified"]), vec![s(&ann)]);
}
