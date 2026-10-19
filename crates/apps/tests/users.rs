//! `/v1/apps/self/users*`: upsert (create, update, link by email under `trust_email`),
//! subject and body validation, batch, list, disconnect and delete (unlink vs. full delete),
//! and the app-key boundary.

#![allow(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements
)]

mod common;

use axum::http::StatusCode;
use common::{eventually, App, TestApp, T};
use serde_json::{json, Value};
use uuid::Uuid;

fn uid(v: &Value) -> Uuid {
    Uuid::parse_str(v["user_id"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn upsert_creates_then_updates() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let email = TestApp::email("Ann");
    let hint = Uuid::new_v4();
    let (s, v) = t
        .put(
            &app.me(),
            "/apps/self/users/ann-1",
            json!({ "email": email.to_uppercase(), "display_name": "  Ann  ", "id": hint }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["created"], true);
    assert_eq!(v["subject"], "ann-1");
    // Emails keep their case (they are compared case-insensitively).
    assert_eq!(v["email"], email.to_uppercase());
    assert_eq!(v["display_name"], "Ann");
    assert_eq!(v["disabled"], false);
    assert_eq!(uid(&v), hint, "a free id hint is used");
    let (pw, name): (Option<String>, String) =
        sqlx::query_as("SELECT password_hash, display_name FROM users WHERE id = $1")
            .bind(hint)
            .fetch_one(t.db())
            .await
            .unwrap();
    assert!(pw.is_none(), "app users have no password");
    assert_eq!(name, "Ann");

    // Same subject again: an update; absent fields are unchanged.
    let new_email = TestApp::email("ann2");
    let (s, v) = t
        .put(
            &app.me(),
            "/apps/self/users/ann-1",
            json!({ "email": new_email, "id": Uuid::new_v4() }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["created"], false);
    assert_eq!(uid(&v), hint, "the id hint only applies to a new user");
    assert_eq!(v["email"], new_email);
    assert_eq!(v["display_name"], "Ann");
    // `null` clears the name; disabled toggles.
    let (s, v) = t
        .put(
            &app.me(),
            "/apps/self/users/ann-1",
            json!({ "email": new_email, "display_name": null, "disabled": true }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["display_name"], Value::Null);
    assert_eq!(v["disabled"], true);

    // A new user without a name: no name (not the email local part).
    let (s, v) = t
        .put(
            &app.me(),
            "/apps/self/users/nameless",
            json!({ "email": TestApp::email("nameless"), "id": hint }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["display_name"], Value::Null);
    assert_ne!(uid(&v), hint, "a taken id hint falls back to a fresh id");

    // Audit: one create per new user.
    let creates: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_audit WHERE actor = $1 AND action = 'app.user.create'",
    )
    .bind(format!("app:{}", app.id))
    .fetch_one(t.db())
    .await
    .unwrap();
    assert_eq!(creates, 2);
}

#[tokio::test]
async fn subjects_and_bodies_are_validated() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let ok = json!({ "email": TestApp::email("v") });
    for bad in ["%20lead", "trail%20", "tab%09x", "%C3%A9", "x%7Fy"] {
        let (s, v) = t
            .put(&app.me(), &format!("/apps/self/users/{bad}"), ok.clone())
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
    let long = "a".repeat(201);
    let (s, _) = t
        .put(&app.me(), &format!("/apps/self/users/{long}"), ok.clone())
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let max = "a".repeat(200);
    let (s, v) = t
        .put(&app.me(), &format!("/apps/self/users/{max}"), ok.clone())
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Printable ASCII with punctuation works (it travels as X-Canvas-User).
    let (s, v) = t
        .put(
            &app.me(),
            "/apps/self/users/auth0%7Cabc%40x.y",
            json!({ "email": TestApp::email("p") }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["subject"], "auth0|abc@x.y");

    // Over-long names are cut to 200 characters (core's clean_name).
    let (s, v) = t
        .put(
            &app.me(),
            "/apps/self/users/long-name",
            json!({ "email": TestApp::email("x"), "display_name": "é".repeat(250) }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["display_name"].as_str().unwrap().chars().count(), 200);
    for body in [
        json!({}),
        json!({ "email": "not-an-email" }),
        json!({ "email": 5 }),
    ] {
        let (s, v) = t.put(&app.me(), "/apps/self/users/body", body).await;
        assert_eq!(s.as_u16() / 100, 4, "{v}");
        assert_ne!(s, StatusCode::OK);
    }
}

#[tokio::test]
async fn email_collisions_and_trust_email() {
    let t = TestApp::new().await;
    let human = t.human("hank").await;
    let strict = t.app(false, false).await;
    let trusting = t.app(false, true).await;

    // An existing account's email: refused unless the app is trusted with emails.
    let (s, v) = t
        .put(
            &strict.me(),
            "/apps/self/users/h",
            json!({ "email": human.email, "display_name": "H" }),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "email_taken");

    let (s, v) = t
        .put(
            &trusting.me(),
            "/apps/self/users/h",
            json!({ "email": human.email.to_uppercase(), "display_name": "Hank T." }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        uid(&v),
        human.user_id.unwrap(),
        "linked to the existing account"
    );
    assert_eq!(v["created"], false);
    assert_eq!(v["email"], human.email, "the account keeps its email");
    assert_eq!(v["display_name"], "Hank T.");

    // The same account cannot be linked twice to one app.
    let (s, v) = t
        .put(
            &trusting.me(),
            "/apps/self/users/h-again",
            json!({ "email": human.email }),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "email_taken");

    // Changing a linked user's email to someone else's: refused.
    let other = t.provision(&strict, "other").await;
    let (s, v) = t
        .put(
            &strict.me(),
            "/apps/self/users/other",
            json!({ "email": human.email }),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "email_taken");
    // Case-only change of its own email is fine.
    let (s, v) = t
        .put(
            &strict.me(),
            "/apps/self/users/other",
            json!({ "email": other.email.to_uppercase() }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn batch_reports_each_item_in_order() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let taken = t.human("taken").await;
    let users = vec![
        json!({ "subject": "b1", "email": TestApp::email("b1"), "display_name": "B1" }),
        json!({ "subject": "b2", "email": taken.email }),
        json!({ "subject": " bad", "email": TestApp::email("bad") }),
        json!({ "subject": "b3", "email": TestApp::email("b3") }),
        json!({ "subject": "b1", "email": TestApp::email("b1b") }),
        json!({ "email": TestApp::email("nosubject") }),
    ];
    let (s, v) = t
        .post(
            &app.me(),
            "/apps/self/users:batch",
            json!({ "users": users }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let r = v["results"].as_array().unwrap();
    assert_eq!(r.len(), 6, "{v}");
    assert_eq!(r[0]["ok"], true);
    assert_eq!(r[0]["created"], true);
    assert_eq!(r[1]["ok"], false);
    assert_eq!(r[1]["error"], "email_taken");
    assert_eq!(r[1]["subject"], "b2");
    assert_eq!(r[2]["ok"], false);
    assert_eq!(r[3]["ok"], true);
    assert_eq!(r[4]["ok"], true);
    assert_eq!(r[4]["created"], false, "second item for b1 updates it");
    assert_eq!(r[5]["ok"], false);
    let (_, list) = t.get(&app.me(), "/apps/self/users").await;
    assert_eq!(list["total"], 2, "{list}");

    let many: Vec<Value> = (0..1001)
        .map(|i| json!({ "subject": format!("m{i}"), "email": format!("m{i}@example.test") }))
        .collect();
    let (s, v) = t
        .post(
            &app.me(),
            "/apps/self/users:batch",
            json!({ "users": many }),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = t
        .post(&app.me(), "/apps/self/users:batch", json!({ "users": [] }))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["results"], json!([]));
}

#[tokio::test]
async fn list_pages_and_counts_canvases() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let mut callers = Vec::new();
    for i in 0..5 {
        callers.push(t.provision(&app, &format!("u{i}")).await);
    }
    t.canvas(&callers[1], "one").await;
    t.canvas(&callers[1], "two").await;
    let (s, v) = t.get(&app.me(), "/apps/self/users?limit=2&offset=1").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 5);
    assert_eq!(v["limit"], 2);
    assert_eq!(v["offset"], 1);
    let page = v["users"].as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["subject"], "u1", "ordered by link time");
    assert_eq!(page[0]["canvas_count"], 2);
    assert_eq!(page[1]["subject"], "u2");
    assert_eq!(page[1]["canvas_count"], 0);
    for key in [
        "user_id",
        "email",
        "display_name",
        "disabled",
        "linked_at",
        "created_at",
    ] {
        assert!(page[0].get(key).is_some(), "{key} in {}", page[0]);
    }
    let (s, v) = t.get(&app.me(), "/apps/self/users?limit=100000").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["limit"], 500, "clamped");
    // Another app sees none of them.
    let other = t.app(false, false).await;
    let (_, v) = t.get(&other.me(), "/apps/self/users").await;
    assert_eq!(v["total"], 0);
}

#[tokio::test]
async fn only_the_app_key_reaches_apps_self() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let ann = t.provision(&app, "ann").await;
    let human = t.human("h").await;
    for path in [
        "/apps/self/users",
        "/apps/self/stats",
        "/apps/self/canvases",
    ] {
        let (s, _) = t.get(&ann, path).await;
        assert_eq!(
            s,
            StatusCode::BAD_REQUEST,
            "{path} acting for a user (core rejects X-Canvas-User)"
        );
        let (s, _) = t.get(&human, path).await;
        assert_eq!(
            s,
            StatusCode::UNAUTHORIZED,
            "{path} with a session (only an app key)"
        );
        let (s, _) = t.call(None, axum::http::Method::GET, path, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{path} anonymous");
        let bogus = App {
            id: app.id,
            key: "cva_bogus".into(),
        };
        let (s, _) = t.get(&bogus.me(), path).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{path} bad key");
    }
}

#[tokio::test]
async fn disconnect_and_delete() {
    let t = TestApp::new().await;
    let app = t.app(false, true).await;
    let ann = t.provision(&app, "ann").await;
    let ben = t.provision(&app, "ben").await;
    let canvas = t.canvas(&ann, "Ann's board").await;
    t.share(&ann, &canvas, &ben).await;
    let ann_id = ann.user_id.unwrap();

    // Disconnect: live sockets close; the account stays.
    let mut peer = t.peer(&ann, &canvas).await;
    let (s, v) = t
        .post(&app.me(), "/apps/self/users/ann/disconnect", Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(peer.wait_closed(T).await.is_some());
    let (s, _) = t.get(&ann, &format!("/canvases/{canvas}")).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = t
        .post(&app.me(), "/apps/self/users/nobody/disconnect", Value::Null)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    // Ben's events.
    let mut ben_rx = t.state.events.subscribe(ben.user_id.unwrap());

    // Delete a pure app user: the account and its canvases go.
    let (s, v) = t.delete(&app.me(), "/apps/self/users/ann").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["deleted"], true, "{v}");
    assert_eq!(v["canvases_deleted"], 1, "{v}");
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE id = $1)")
        .bind(ann_id)
        .fetch_one(t.db())
        .await
        .unwrap();
    assert!(!exists);
    let (s, _) = t.get(&ben, &format!("/canvases/{canvas}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let cid = Uuid::parse_str(&canvas).unwrap();
    assert!(
        eventually(T, || {
            let got = common::drain_kinds(&mut ben_rx);
            async move { got.contains(&(cid, "deleted".to_owned())) }
        })
        .await,
        "ben is told the canvas is gone"
    );
    let (s, v) = t.delete(&app.me(), "/apps/self/users/ann").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    // A human who was linked (trust_email): unlinked only.
    let human = t.human("hum").await;
    let (s, v) = t
        .put(
            &app.me(),
            "/apps/self/users/hum",
            json!({ "email": human.email }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = t.delete(&app.me(), "/apps/self/users/hum").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["deleted"], false, "{v}");
    let (s, _) = t.get(&human, "/canvases").await;
    assert_eq!(s, StatusCode::OK, "the human keeps their account");
    let (_, list) = t.get(&app.me(), "/apps/self/users").await;
    assert_eq!(list["total"], 1, "only ben is left: {list}");

    // A user linked to two apps: deleting from one only unlinks.
    let app2 = t.app(false, true).await;
    let (s, _) = t
        .put(
            &app2.me(),
            "/apps/self/users/ben-too",
            json!({ "email": ben.email }),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = t.delete(&app.me(), "/apps/self/users/ben").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["deleted"], false, "{v}");
    let (s, v) = t.delete(&app2.me(), "/apps/self/users/ben-too").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["deleted"], true, "{v}");
}

#[tokio::test]
async fn disabling_a_user_closes_their_sockets() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let ann = t.provision(&app, "ann").await;
    let canvas = t.canvas(&ann, "b").await;
    let mut peer = t.peer(&ann, &canvas).await;
    let (s, v) = t
        .put(
            &app.me(),
            "/apps/self/users/ann",
            json!({ "email": ann.email, "disabled": true }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(peer.wait_closed(T).await.is_some());
    let (s, _) = t.get(&ann, &format!("/canvases/{canvas}")).await;
    assert_ne!(s, StatusCode::OK, "a disabled user cannot act");
}

#[tokio::test]
async fn concurrent_upserts_of_one_subject_create_one_user() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let email = TestApp::email("race");
    let mut handles = Vec::new();
    for _ in 0..8 {
        let router = t.router.clone();
        let key = app.key.clone();
        let email = email.clone();
        handles.push(tokio::spawn(async move {
            use tower::ServiceExt as _;
            let req = axum::http::Request::put("/v1/apps/self/users/race")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({ "email": email }).to_string(),
                ))
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), StatusCode::OK);
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE lower(email) = lower($1)")
        .bind(&email)
        .fetch_one(t.db())
        .await
        .unwrap();
    assert_eq!(n, 1);
}
