//! Password sign-in, `me`, logout, password change, personal access tokens (incl. scopes
//! enforced end to end), `/v1/events`, rate limits and the `/account` page — against a real
//! server on native Postgres.

#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use std::time::Duration;

use canvas_cloud_core::events::{Event, RING_CAPACITY};
use canvas_cloud_core::identity::Scope;
use common::{unique_email, TestApp, PASSWORD};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn login_is_uniform_and_enumeration_safe() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;

    // Success: {token, user}; the token works.
    let (s, v) = app.login(&ann.email.to_uppercase(), PASSWORD).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let token = v["token"].as_str().unwrap();
    assert!(token.starts_with("cvs_"));
    assert_eq!(v["user"]["id"], json!(ann.id));
    assert_eq!(v["user"]["email"], json!(ann.email));
    assert_eq!(v["user"]["display_name"], "ann");
    assert!(v["user"]["created_at"].is_string());
    assert_eq!(v["user"].as_object().unwrap().len(), 4, "{v}");
    let (s, me) = app.get("/v1/auth/me", token).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["user"]["id"], json!(ann.id));

    // Wrong password, unknown email, password-less (app-provisioned) user and an over-long
    // password all get the same 401.
    let app_user = unique_email("fed", "example.test");
    app.federated_user(&app_user).await;
    let long = "x".repeat(2000);
    let cases: [(&str, &str); 5] = [
        (&ann.email, "wrong password here"),
        ("nobody-at-all@example.test", PASSWORD),
        (&app_user, PASSWORD),
        (&app_user, ""),
        (&ann.email, &long),
    ];
    let mut bodies = Vec::new();
    for (email, pw) in cases {
        let (s, v) = app.login(email, pw).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{email}: {v}");
        assert_eq!(v["error"], "invalid_credentials", "{v}");
        bodies.push(v);
    }
    assert!(bodies.windows(2).all(|w| w[0] == w[1]), "{bodies:?}");

    // Disabled: only revealed with the right password.
    sqlx::query("UPDATE users SET disabled = true WHERE id = $1")
        .bind(ann.id)
        .execute(app.db())
        .await
        .unwrap();
    let (s, v) = app.login(&ann.email, "wrong password here").await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid_credentials")));
    let (s, v) = app.login(&ann.email, PASSWORD).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("account_disabled")));

    // Malformed bodies are 400s.
    let (s, _) = app.post("/v1/auth/login", None, json!({ "email": "a@b.co" })).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn me_reports_how_the_caller_authenticated() {
    let app = TestApp::new().await;
    let bea = app.user("bea").await;
    let (s, v) = app.get("/v1/auth/me", &bea.token).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["via"], json!({ "kind": "session" }));
    assert_eq!(v["user"]["email"], json!(bea.email));

    let read = app.pat(bea.id, Scope::Read).await;
    let (_, v) = app.get("/v1/auth/me", &read).await;
    assert_eq!(v["via"], json!({ "kind": "token", "scope": "read" }));
    let write = app.pat(bea.id, Scope::Write).await;
    let (_, v) = app.get("/v1/auth/me", &write).await;
    assert_eq!(v["via"], json!({ "kind": "token", "scope": "write" }));

    let (app_id, key) = app.app_for(bea.id, "subject-bea").await;
    let (s, v) = app
        .call_with(Method::GET, "/v1/auth/me", Some(&key), None, &[("x-canvas-user", "subject-bea")])
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["via"], json!({ "kind": "app", "app_id": app_id }));
    assert_eq!(v["user"]["id"], json!(bea.id));

    // PATCH display_name (any write-capable credential).
    let (s, v) = app
        .call(Method::PATCH, "/v1/auth/me", Some(&bea.token), Some(json!({ "display_name": "  Bea B. " })))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["user"]["display_name"], "Bea B.");
    assert_eq!(v["via"]["kind"], "session");
    let (s, v) = app
        .call(Method::PATCH, "/v1/auth/me", Some(&write), Some(json!({ "display_name": "Bea" })))
        .await;
    assert_eq!((s, v["user"]["display_name"].as_str()), (StatusCode::OK, Some("Bea")));
    for bad in [json!({ "display_name": "   " }), json!({}), json!({ "display_name": "x".repeat(201) })] {
        let (s, v) = app.call(Method::PATCH, "/v1/auth/me", Some(&bea.token), Some(bad)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    }
    let (s, v) = app
        .call(Method::PATCH, "/v1/auth/me", Some(&read), Some(json!({ "display_name": "nope" })))
        .await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("read_only_token")));

    let (s, v) = app.call(Method::GET, "/v1/auth/me", None, None).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("unauthorized")));
}

#[tokio::test]
async fn logout_ends_sessions_only() {
    let app = TestApp::new().await;
    let cy = app.user("cy").await;
    let (_, second) = app.login(&cy.email, PASSWORD).await;
    let second = second["token"].as_str().unwrap().to_owned();
    let (_, third) = app.login(&cy.email, PASSWORD).await;
    let third = third["token"].as_str().unwrap().to_owned();

    let pat = app.pat(cy.id, Scope::Write).await;
    let (s, v) = app.call(Method::POST, "/v1/auth/logout", Some(&pat), None).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("session_required")));

    let (s, v) = app.call(Method::POST, "/v1/auth/logout", Some(&cy.token), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "ok": true, "revoked": 1 }));
    let (s, _) = app.get("/v1/auth/me", &cy.token).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = app.get("/v1/auth/me", &second).await;
    assert_eq!(s, StatusCode::OK);

    let (s, v) = app.post("/v1/auth/logout", Some(&second), json!({ "all": true })).await;
    assert_eq!((s, v["revoked"].as_u64()), (StatusCode::OK, Some(2)));
    for t in [&second, &third] {
        assert_eq!(app.get("/v1/auth/me", t).await.0, StatusCode::UNAUTHORIZED);
    }
    // Tokens survive a logout.
    assert_eq!(app.get("/v1/auth/me", &pat).await.0, StatusCode::OK);
}

#[tokio::test]
async fn password_change() {
    let app = TestApp::new().await;
    let di = app.user("di").await;
    let (_, other) = app.login(&di.email, PASSWORD).await;
    let other = other["token"].as_str().unwrap().to_owned();
    let pat = app.pat(di.id, Scope::Write).await;

    let (s, v) = app
        .post("/v1/auth/password", Some(&pat), json!({ "current": PASSWORD, "new": "another long password" }))
        .await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("session_required")));
    let (s, v) = app
        .post("/v1/auth/password", Some(&di.token), json!({ "current": "not my password", "new": "another long password" }))
        .await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("invalid_credentials")));
    for new in ["short", &"x".repeat(257)] {
        let (s, _) = app
            .post("/v1/auth/password", Some(&di.token), json!({ "current": PASSWORD, "new": new }))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
    let (s, v) = app
        .post("/v1/auth/password", Some(&di.token), json!({ "current": PASSWORD, "new": "another long password" }))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "ok": true, "revoked_sessions": 1 }));
    assert_eq!(app.get("/v1/auth/me", &di.token).await.0, StatusCode::OK, "this session stays");
    assert_eq!(app.get("/v1/auth/me", &other).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(app.get("/v1/auth/me", &pat).await.0, StatusCode::OK, "tokens stay");
    assert_eq!(app.login(&di.email, PASSWORD).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(app.login(&di.email, "another long password").await.0, StatusCode::OK);
    // `old` is accepted as an alias of `current`.
    let (s, _) = app
        .post("/v1/auth/password", Some(&di.token), json!({ "old": "another long password", "new": PASSWORD }))
        .await;
    assert_eq!(s, StatusCode::OK);

    // A password-less account (app / OIDC) with a session cannot set one itself.
    let fed = app.federated_user(&unique_email("fed", "example.test")).await;
    let session = canvas_cloud_core::identity::create_session(app.db(), fed, None)
        .await
        .unwrap();
    let (s, v) = app
        .post("/v1/auth/password", Some(&session), json!({ "current": "", "new": "another long password" }))
        .await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("password_not_set")));
}

#[tokio::test]
async fn linked_app_user_signs_in_after_an_admin_sets_a_password() {
    let app = TestApp::new().await;
    let email = unique_email("linked", "example.test");
    let id = app.federated_user(&email).await;
    let (_, key) = app.app_for(id, "sub-linked").await;
    let hdr = [("x-canvas-user", "sub-linked")];
    let (s, canvas) = app
        .call_with(Method::POST, "/v1/canvases", Some(&key), Some(json!({ "name": "Plans" })), &hdr)
        .await;
    assert_eq!(s, StatusCode::CREATED, "{canvas}");
    assert_eq!(app.login(&email, PASSWORD).await.0, StatusCode::UNAUTHORIZED);

    // What `canvas-cloud admin users set-password` does.
    let hash = canvas_cloud_core::identity::hash_password(PASSWORD.into()).await.unwrap();
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(id)
        .bind(hash)
        .execute(app.db())
        .await
        .unwrap();
    let (s, v) = app.login(&email, PASSWORD).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["user"]["id"], json!(id), "same users row");
    let (_, list) = app.get("/v1/canvases", v["token"].as_str().unwrap()).await;
    assert!(
        list.as_array().unwrap().iter().any(|c| c["id"] == canvas["id"]),
        "{list}"
    );
}

#[tokio::test]
async fn personal_access_tokens() {
    let app = TestApp::new().await;
    let ed = app.user("ed").await;

    let (s, v) = app
        .post("/v1/tokens", Some(&ed.token), json!({ "name": " CI bot ", "scope": "read", "expires_in_days": 30 }))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let read = v["token"].as_str().unwrap().to_owned();
    assert!(read.starts_with("cvt_"));
    assert_eq!(v["name"], "CI bot");
    assert_eq!(v["scope"], "read");
    assert!(v["last_used_at"].is_null());
    let expires = time::OffsetDateTime::parse(
        v["expires_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let days = (expires - time::OffsetDateTime::now_utc()).whole_days();
    assert!((29..=30).contains(&days), "{days}");
    let read_id = v["id"].as_str().unwrap().to_owned();
    let (s, v) = app
        .post("/v1/tokens", Some(&ed.token), json!({ "name": "agent", "scope": "write" }))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert!(v["expires_at"].is_null());
    let write = v["token"].as_str().unwrap().to_owned();

    // Listed newest first, never with the secret.
    let (s, list) = app.get("/v1/tokens", &ed.token).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list["limit"], 50);
    let names: Vec<&str> = list["tokens"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["agent", "CI bot"]);
    assert!(list["tokens"][0].get("token").is_none());
    let keys: Vec<&String> = list["tokens"][0].as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 6, "{keys:?}");

    // Scope enforced end to end: read tokens GET only, no rooms; write tokens do anything.
    let (s, _) = app.get("/v1/canvases", &read).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = app.post("/v1/canvases", Some(&read), json!({ "name": "no" })).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("read_only_token")));
    let (s, canvas) = app.post("/v1/canvases", Some(&write), json!({ "name": "yes" })).await;
    assert_eq!(s, StatusCode::CREATED, "{canvas}");
    let id = canvas["id"].as_str().unwrap();
    let ws = |token: &str| {
        let url = format!("ws://{}/v1/canvases/{id}/ws?token={token}", app.addr);
        async move { tokio_tungstenite::connect_async(url.into_client_request().unwrap()).await }
    };
    match ws(&read).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        }
        other => panic!("read token joined a room: {:?}", other.map(|_| ())),
    }
    let (sock, _) = ws(&write).await.expect("write token joins the room");
    drop(sock);
    let (_, v) = app.get("/v1/tokens", &ed.token).await;
    assert!(v["tokens"][1]["last_used_at"].is_string(), "{v}");

    // Only a session manages tokens.
    for t in [&read, &write] {
        let (s, v) = app.get("/v1/tokens", t).await;
        assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("session_required")));
    }
    let (s, v) = app.post("/v1/tokens", Some(&write), json!({ "name": "x", "scope": "write" })).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("session_required")));
    let (s, v) = app.call(Method::DELETE, &format!("/v1/tokens/{read_id}"), Some(&write), None).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("session_required")));

    // Validation.
    for bad in [
        json!({ "name": "", "scope": "read" }),
        json!({ "scope": "read" }),
        json!({ "name": "x", "scope": "admin" }),
        json!({ "name": "x" }),
        json!({ "name": "x", "scope": "read", "expires_in_days": 0 }),
        json!({ "name": "x", "scope": "read", "expires_in_days": 3651 }),
        json!({ "name": "x".repeat(201), "scope": "read" }),
    ] {
        let (s, v) = app.post("/v1/tokens", Some(&ed.token), bad.clone()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }

    // Revoke: gone from the list and dead at once; unknown / someone else's → 404.
    let (s, v) = app.call(Method::DELETE, &format!("/v1/tokens/{read_id}"), Some(&ed.token), None).await;
    assert_eq!((s, v), (StatusCode::OK, json!({ "ok": true })));
    assert_eq!(app.get("/v1/canvases", &read).await.0, StatusCode::UNAUTHORIZED);
    let (_, v) = app.get("/v1/tokens", &ed.token).await;
    assert_eq!(v["tokens"].as_array().unwrap().len(), 1);
    let (s, _) = app.call(Method::DELETE, &format!("/v1/tokens/{read_id}"), Some(&ed.token), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app.call(Method::DELETE, "/v1/tokens/not-a-uuid", Some(&ed.token), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let fay = app.user("fay").await;
    let (_, v) = app.get("/v1/tokens", &ed.token).await;
    let write_id = v["tokens"][0]["id"].as_str().unwrap().to_owned();
    let (s, _) = app.call(Method::DELETE, &format!("/v1/tokens/{write_id}"), Some(&fay.token), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(app.get("/v1/canvases", &write).await.0, StatusCode::OK);

    // Expired tokens neither work nor count.
    sqlx::query("UPDATE access_tokens SET expires_at = now() - interval '1 minute' WHERE id = $1")
        .bind(Uuid::parse_str(&write_id).unwrap())
        .execute(app.db())
        .await
        .unwrap();
    assert_eq!(app.get("/v1/canvases", &write).await.0, StatusCode::UNAUTHORIZED);
    let (_, v) = app.get("/v1/tokens", &ed.token).await;
    assert_eq!(v["tokens"], json!([]));
}

#[tokio::test]
async fn token_limit() {
    let app = TestApp::new().await;
    let gus = app.user("gus").await;
    for i in 0..50 {
        let (s, v) = app
            .post("/v1/tokens", Some(&gus.token), json!({ "name": format!("t{i}"), "scope": "read" }))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{i}: {v}");
    }
    let (s, v) = app.post("/v1/tokens", Some(&gus.token), json!({ "name": "one more", "scope": "read" })).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("token_limit")));
    let (_, list) = app.get("/v1/tokens", &gus.token).await;
    let id = list["tokens"][0]["id"].as_str().unwrap().to_owned();
    let (s, _) = app.call(Method::DELETE, &format!("/v1/tokens/{id}"), Some(&gus.token), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = app.post("/v1/tokens", Some(&gus.token), json!({ "name": "one more", "scope": "read" })).await;
    assert_eq!(s, StatusCode::CREATED);
}

/// The SSE id of core event `n`, given a `ready` frame's id sent when core's last id was
/// `at` (ids are `base + n`).
fn wire_from(ready_id: &str, at: u64, n: u64) -> String {
    (ready_id.parse::<u64>().unwrap() - at + n).to_string()
}

fn canvas_event(kind: &str) -> Event {
    Event::Canvas {
        canvas_id: Uuid::nil(),
        kind: kind.into(),
    }
}

#[tokio::test]
async fn events_stream_live_with_ids_and_only_the_callers() {
    let app = TestApp::new().await;
    let hal = app.user("hal").await;
    let ivy = app.user("ivy").await;

    let mut sse = app.events(&hal.token, &[]).await;
    assert_eq!(sse.status, StatusCode::OK);
    assert!(sse.headers["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    assert_eq!(sse.headers["cache-control"], "no-cache");
    let ready = sse.next(WAIT).await.expect("ready");
    assert_eq!(ready.event, "ready");
    let ready_id = ready.id.clone().unwrap();
    let start: u64 = ready_id.parse().unwrap();
    assert!(start > 1_700_000_000_000_000 && start < 1 << 53, "microsecond base: {start}");
    assert_eq!(ready.json()["last_event_id"], json!(start));
    let at = app.state.events.last_id();
    let wire = |n: u64| wire_from(&ready_id, at, n);
    assert_eq!(ready.json()["user_id"], json!(hal.id));

    // Ivy's events never reach Hal; Hal's REST actions do, with increasing ids and the
    // copper-cloud `canvas` frame JSON.
    app.state.events.publish(ivy.id, canvas_event("update"));
    let (s, canvas) = app.post("/v1/canvases", Some(&hal.token), json!({ "name": "Board" })).await;
    assert_eq!(s, StatusCode::CREATED);
    let f = sse.next(WAIT).await.expect("canvas event");
    assert_eq!(f.event, "canvas");
    assert_eq!(
        f.json(),
        json!({ "type": "canvas", "canvas_id": canvas["id"], "kind": "created" })
    );
    let id: u64 = f.id.as_deref().unwrap().parse().unwrap();
    assert!(id > start + 1, "ivy's event took an id");
    assert!(!f.data.contains(' ') && !f.data.contains('\n'), "compact JSON: {}", f.data);

    // A read-only PAT via ?token= (EventSource cannot set headers), and an app acting for Hal.
    let read = app.pat(hal.id, Scope::Read).await;
    let mut by_query = common::Sse::open(
        app.http
            .get(app.url(&format!("/v1/events?token={read}")))
            .header("accept", "text/event-stream"),
    )
    .await;
    assert_eq!(by_query.status, StatusCode::OK);
    assert_eq!(by_query.next(WAIT).await.unwrap().event, "ready");
    let (_, key) = app.app_for(hal.id, "hal-subject").await;
    let mut by_app = app.events(&key, &[("x-canvas-user", "hal-subject")]).await;
    assert_eq!(by_app.status, StatusCode::OK);
    assert_eq!(by_app.next(WAIT).await.unwrap().event, "ready");
    let n = app.state.events.publish(hal.id, canvas_event("renamed"));
    for s in [&mut sse, &mut by_query, &mut by_app] {
        let f = s.until("canvas", WAIT).await.unwrap();
        assert_eq!(f.id, Some(wire(n)));
        assert_eq!(f.json()["kind"], "renamed");
    }

    // No credential, or ?token= without the event-stream Accept header → 401.
    let (s, _) = app.call(Method::GET, "/v1/events", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = app.call(Method::GET, &format!("/v1/events?token={read}"), None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn events_resume_from_last_event_id_or_resync() {
    let app = TestApp::new().await;
    let jo = app.user("jo").await;
    let kim = app.user("kim").await;
    let ev = &app.state.events;

    let first = app.events(&jo.token, &[]).await.next(WAIT).await.unwrap();
    let (r, at) = (first.id.unwrap(), ev.last_id());
    let wire = |n: u64| wire_from(&r, at, n);
    let a = ev.publish(jo.id, canvas_event("a"));
    ev.publish(kim.id, canvas_event("other"));
    let b = ev.publish(jo.id, canvas_event("b"));
    let c = ev.publish(jo.id, canvas_event("c"));

    // Resume after `a`: replays b, c (not Kim's), then ready at the newest id.
    let mut sse = app.events(&jo.token, &[("last-event-id", &wire(a))]).await;
    let f1 = sse.next(WAIT).await.unwrap();
    let f2 = sse.next(WAIT).await.unwrap();
    assert_eq!((f1.event.as_str(), f1.id.clone()), ("canvas", Some(wire(b))));
    assert_eq!(f1.json()["kind"], "b");
    assert_eq!(f2.id, Some(wire(c)));
    let ready = sse.next(WAIT).await.unwrap();
    assert_eq!((ready.event.as_str(), ready.id.clone()), ("ready", Some(wire(c))));
    let d = ev.publish(jo.id, canvas_event("d"));
    assert_eq!(sse.next(WAIT).await.unwrap().id, Some(wire(d)));
    let d = wire(d);

    // Up to date: just ready. Via the query parameter too.
    let mut sse = common::Sse::open(
        app.http
            .get(app.url(&format!("/v1/events?last_event_id={d}")))
            .bearer_auth(&jo.token),
    )
    .await;
    let f = sse.next(WAIT).await.unwrap();
    assert_eq!((f.event.as_str(), f.id), ("ready", Some(d.clone())));

    // An id this process never issued, one from before a restart (another epoch, even with a
    // core id that exists now), garbage, or one that fell out of the ring buffer → resync,
    // then ready.
    let base = r.parse::<u64>().unwrap() - at;
    let restarted = (base - 1).to_string();
    for bad in [wire(999_999_999), restarted, a.to_string(), "nope".to_owned()] {
        let mut sse = app.events(&jo.token, &[("last-event-id", &bad)]).await;
        let f = sse.next(WAIT).await.unwrap();
        assert_eq!(f.event, "resync", "{bad}");
        assert_eq!(f.json(), json!({ "reason": "gap" }));
        assert_eq!(sse.next(WAIT).await.unwrap().event, "ready");
    }
    for _ in 0..RING_CAPACITY {
        ev.publish(kim.id, canvas_event("flood"));
    }
    let mut sse = app.events(&jo.token, &[("last-event-id", &wire(a))]).await;
    assert_eq!(sse.next(WAIT).await.unwrap().event, "resync");
    let ready = sse.next(WAIT).await.unwrap();
    assert_eq!(ready.event, "ready");
    assert_eq!(ready.id, Some(wire(ev.last_id())));
}

#[tokio::test]
async fn events_end_when_the_credential_is_revoked() {
    // Revalidation runs every REVALIDATE_EVERY; here we only check the stream stays up while
    // the credential is valid and that a revoked one cannot reconnect.
    let app = TestApp::new().await;
    let lu = app.user("lu").await;
    let mut sse = app.events(&lu.token, &[]).await;
    assert_eq!(sse.next(WAIT).await.unwrap().event, "ready");
    let (s, _) = app.call(Method::POST, "/v1/auth/logout", Some(&lu.token), None).await;
    assert_eq!(s, StatusCode::OK);
    let again = app.events(&lu.token, &[]).await;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_routes_are_rate_limited_per_ip_and_per_email() {
    // Per IP (core): the 4th call from one address in a minute is refused.
    let app = TestApp::with(|c| c.limits.auth_per_minute = 3).await;
    let mo = app.user("mo").await;
    let mut statuses = Vec::new();
    for _ in 0..4 {
        statuses.push(app.login(&mo.email, PASSWORD).await.0);
    }
    assert_eq!(statuses[..3], [StatusCode::OK; 3]);
    assert_eq!(statuses[3], StatusCode::TOO_MANY_REQUESTS);

    // Per email: the same address from four different IPs (behind a trusted proxy) is limited
    // too, while another email from a fifth IP still gets through.
    let app = TestApp::with(|c| {
        c.limits.auth_per_minute = 3;
        c.trust_proxy = true;
    })
    .await;
    let ned = app.user("ned").await;
    let mut statuses = Vec::new();
    for i in 1..=4 {
        let ip = format!("203.0.113.{i}");
        let (s, v) = app
            .call_with(
                Method::POST,
                "/v1/auth/login",
                None,
                Some(json!({ "email": ned.email, "password": "wrong password!!" })),
                &[("x-forwarded-for", &ip)],
            )
            .await;
        statuses.push((s, v["error"].as_str().map(ToOwned::to_owned)));
    }
    assert!(statuses[..3].iter().all(|(s, _)| *s == StatusCode::UNAUTHORIZED), "{statuses:?}");
    assert_eq!(statuses[3], (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited".into())));
    let (s, _) = app
        .call_with(
            Method::POST,
            "/v1/auth/login",
            None,
            Some(json!({ "email": ned.email.to_uppercase(), "password": PASSWORD })),
            &[("x-forwarded-for", "203.0.113.9")],
        )
        .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "case-insensitive bucket");
    let other = app.user("oz").await;
    let (s, _) = app
        .call_with(
            Method::POST,
            "/v1/auth/login",
            None,
            Some(json!({ "email": other.email, "password": PASSWORD })),
            &[("x-forwarded-for", "203.0.113.5")],
        )
        .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn account_page_is_served_with_a_strict_csp() {
    let app = TestApp::new().await;
    for path in ["/account", "/account/"] {
        let resp = app.http.get(app.url(path)).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        let h = resp.headers().clone();
        assert_eq!(h["content-type"], "text/html; charset=utf-8");
        assert_eq!(h["content-security-policy"], canvas_cloud_accounts::CONTENT_SECURITY_POLICY);
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert_eq!(h["x-frame-options"], "DENY");
        assert_eq!(h["cache-control"], "no-cache");
        let body = resp.text().await.unwrap();
        assert!(body.starts_with("<!doctype html>"), "{path}");
        assert!(body.contains("src=\"/account/app.js\""));
        assert!(body.contains("href=\"/account/app.css\""));
        assert!(!body.contains("<script>"), "no inline script");
        assert!(!body.contains(" style="), "no inline style");
        assert!(!body.contains("http://") && !body.contains("https://"), "no external requests");
    }
    let csp = canvas_cloud_accounts::CONTENT_SECURITY_POLICY;
    for want in ["default-src 'none'", "script-src 'self'", "style-src 'self'", "connect-src 'self'", "frame-ancestors 'none'"] {
        assert!(csp.contains(want), "{want}");
    }
    assert!(!csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"));
    let js = app.http.get(app.url("/account/app.js")).send().await.unwrap();
    assert_eq!(js.status(), StatusCode::OK);
    assert_eq!(js.headers()["content-type"], "text/javascript; charset=utf-8");
    let js = js.text().await.unwrap();
    for api in ["/v1/auth/login", "/v1/auth/signup", "/v1/tokens", "/v1/canvases", "/v1/info", "sessionStorage"] {
        assert!(js.contains(api), "{api}");
    }
    assert!(!js.contains("http://") && !js.contains("https://"), "no external requests");
    let css = app.http.get(app.url("/account/app.css")).send().await.unwrap();
    assert_eq!(css.headers()["content-type"], "text/css; charset=utf-8");
    assert!(css.text().await.unwrap().contains("prefers-color-scheme"));
    // Other methods are not served.
    let r = app.http.post(app.url("/account")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn info_lists_the_accounts_features() {
    let app = TestApp::new().await;
    let resp = app.http.get(app.url("/v1/info")).send().await.unwrap();
    let v: Value = resp.json().await.unwrap();
    for f in canvas_cloud_accounts::FEATURES {
        assert!(v["features"].as_array().unwrap().contains(&json!(f)), "{f}: {v}");
    }
    assert_eq!(v["auth"]["password"], true);
}
