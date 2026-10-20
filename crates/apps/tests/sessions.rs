//! `POST /v1/apps/self/users/{subject}/sessions`: a `trust_email` app signs one of its users
//! in (an ordinary `cvs_…` session), and every way that is refused.

#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use canvas_cloud_core::identity::credential_digest;
use common::{App, Caller, TestApp};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tower::ServiceExt as _;
use uuid::Uuid;

/// `POST /v1/apps/self/users/{subject}/sessions` as `who` (with `X-Canvas-User` when it has
/// a subject), with an optional raw body. Returns status, headers and JSON.
async fn mint_raw(
    t: &TestApp,
    who: &Caller,
    subject: &str,
    body: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(format!("/v1/apps/self/users/{subject}/sessions"))
        .header("authorization", format!("Bearer {}", who.token))
        .header("user-agent", "sessions-tests/1");
    if let Some(s) = &who.subject {
        req = req.header("x-canvas-user", s);
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_owned())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let resp = t.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, v)
}

async fn mint(t: &TestApp, app: &App, subject: &str) -> (StatusCode, Value) {
    let (s, _, v) = mint_raw(t, &app.me(), subject, Some("{}")).await;
    (s, v)
}

fn session(token: &str) -> Caller {
    Caller {
        token: token.to_owned(),
        subject: None,
        user_id: None,
        email: String::new(),
    }
}

#[tokio::test]
async fn a_trusting_app_signs_its_user_in() {
    let t = TestApp::new().await;
    let app = t.app(false, true).await;
    let ann = t.provision(&app, "ann").await;
    let ann_id = ann.user_id.unwrap();

    let (s, h, v) = mint_raw(&t, &app.me(), "ann", Some("{}")).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(h["cache-control"], "no-store");
    let token = v["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("cvs_"), "{v}");
    assert_eq!(v["user_id"], ann_id.to_string());
    let expires = OffsetDateTime::parse(v["expires_at"].as_str().unwrap(), &Rfc3339).unwrap();
    let hours = (expires - OffsetDateTime::now_utc()).whole_hours();
    assert!((29 * 24..=30 * 24).contains(&hours), "{v}");
    assert_eq!(v.as_object().unwrap().len(), 3, "{v}");

    // An ordinary session row: same table and expiry as login / OIDC.
    let (uid, ua, live): (Uuid, Option<String>, bool) = sqlx::query_as(
        "SELECT user_id, user_agent,
                expires_at BETWEEN now() + interval '29 days' AND now() + interval '30 days'
         FROM sessions WHERE token_sha256 = $1",
    )
    .bind(&credential_digest(&token)[..])
    .fetch_one(t.db())
    .await
    .unwrap();
    assert_eq!(
        (uid, ua.as_deref(), live),
        (ann_id, Some("sessions-tests/1"), true)
    );

    // The token is the person's own credential: no app key, no X-Canvas-User.
    let me = session(&token);
    let (s, list) = t.get(&me, "/canvases").await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let (s, own) = t.get(&ann, "/canvases").await;
    assert_eq!((s, &list), (StatusCode::OK, &own), "the same user");

    // Body optional; each call is a new session.
    let (s, _, v2) = mint_raw(&t, &app.me(), "ann", None).await;
    assert_eq!(s, StatusCode::CREATED, "{v2}");
    assert_ne!(v2["token"], v["token"]);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id = $1")
        .bind(ann_id)
        .fetch_one(t.db())
        .await
        .unwrap();
    assert_eq!(n, 2);

    // Audited (never the token).
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT actor, detail FROM admin_audit WHERE action = 'app.user.session' AND target = $1",
    )
    .bind(ann_id.to_string())
    .fetch_all(t.db())
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, format!("app:{}", app.id));
    assert_eq!(rows[0].1, json!({ "subject": "ann" }));

    // Subjects with spaces work percent-encoded, like on the other user routes.
    let (s, v) = t
        .put(
            &app.me(),
            "/apps/self/users/ann%20b",
            json!({ "email": TestApp::email("annb") }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, minted) = mint(&t, &app, "ann%20b").await;
    assert_eq!(s, StatusCode::CREATED, "{minted}");
    assert_eq!(minted["user_id"], v["user_id"]);

    let (s, info) = t.call(None, Method::GET, "/info", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        info["features"]
            .as_array()
            .unwrap()
            .contains(&json!("app_sessions")),
        "{info}"
    );
    assert_eq!(info["limits"]["app_sessions_per_minute"], 60);
}

#[tokio::test]
async fn refusals() {
    let t = TestApp::new().await;
    let app = t.app(false, true).await;
    let ann = t.provision(&app, "ann").await;

    // Only the app as itself: X-Canvas-User → 400; a person's session → 401.
    let (s, _, v) = mint_raw(&t, &app.acting("ann"), "ann", Some("{}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let human = t.human("hum").await;
    let (s, _, v) = mint_raw(&t, &human, "ann", Some("{}")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    let (s, v) = mint(&t, &app, "ann").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, _, v) = mint_raw(&t, &session(v["token"].as_str().unwrap()), "ann", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");

    // An app that does not trust email may not sign people in.
    let wary = t.app(false, false).await;
    t.provision(&wary, "ann").await;
    let (s, v) = mint(&t, &wary, "ann").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("trust_email_required")),
        "{v}"
    );

    // Unknown subject — including another app's user.
    let (s, v) = mint(&t, &app, "nobody").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("unknown_subject")),
        "{v}"
    );
    let other = t.app(false, true).await;
    t.provision(&other, "zed").await;
    let (s, v) = mint(&t, &app, "zed").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("unknown_subject"))
    );

    // Disabled.
    let (s, v) = t
        .put(
            &app.me(),
            "/apps/self/users/ann",
            json!({ "email": ann.email, "disabled": true }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = mint(&t, &app, "ann").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("account_disabled")),
        "{v}"
    );

    // A body that is not JSON.
    t.provision(&app, "ben").await;
    let (s, _, v) = mint_raw(&t, &app.me(), "ben", Some("nope")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // A revoked app key.
    canvas_cloud_core::identity::revoke_app(t.db(), app.id)
        .await
        .unwrap();
    let (s, v) = mint(&t, &app, "ben").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
}

#[tokio::test]
async fn rate_limited_per_app() {
    let t = TestApp::with(|c| c.limits.app_sessions_per_minute = 2).await;
    let app = t.app(false, true).await;
    let other = t.app(false, true).await;
    t.provision(&app, "ann").await;
    t.provision(&other, "ben").await;
    for _ in 0..2 {
        let (s, v) = mint(&t, &app, "ann").await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    let (s, v) = mint(&t, &app, "ann").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited")),
        "{v}"
    );
    // Another app has its own budget.
    let (s, v) = mint(&t, &other, "ben").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (_, info) = t.call(None, Method::GET, "/info", None).await;
    assert_eq!(info["limits"]["app_sessions_per_minute"], 2);
}
