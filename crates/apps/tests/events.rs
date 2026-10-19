//! `GET /v1/apps/self/events`: only this app's users' canvas events, as SSE with ids;
//! `Last-Event-ID` replay without loss or duplication; `resync` on a gap; subjects of users
//! linked after the stream opened; keep-alives; the stream ends when the app is revoked.

#![allow(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements
)]

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use canvas_cloud_core::events::Event;
use common::{EventStream, Sse, TestApp, T};
use serde_json::{json, Value};
use uuid::Uuid;

fn data(ev: &Sse) -> Value {
    serde_json::from_str(&ev.data).unwrap()
}

fn is(ev: &Sse, subject: &str, kind: &str) -> bool {
    ev.event == "canvas" && {
        let d = data(ev);
        d["subject"] == subject && d["kind"] == kind
    }
}

#[tokio::test]
async fn streams_only_this_apps_users() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let other = t.app(false, false).await;
    let ann = t.provision(&app, "ann").await;
    let ben = t.provision(&app, "ben").await;
    let zed = t.provision(&other, "zed").await;
    let human = t.human("hum").await;
    let mut es = EventStream::open(&t, &app.key, None).await;

    // Activity of a user of another app, and of a human: invisible.
    t.canvas(&zed, "zed's").await;
    t.canvas(&human, "human's").await;
    // Ann creates and shares with Ben.
    let canvas = t.canvas(&ann, "Plans").await;
    let ev = es
        .until(T, |e| e.event == "canvas")
        .await
        .expect("ann's created event");
    let d = data(&ev);
    assert_eq!(d["subject"], "ann", "{d}");
    assert_eq!(d["kind"], "created");
    assert_eq!(d["canvas_id"], canvas);
    assert_eq!(d["user_id"], ann.user_id.unwrap().to_string());
    assert!(ev.id.as_deref().unwrap().parse::<u64>().is_ok());

    let (s, inv) = t
        .post(
            &ann,
            &format!("/canvases/{canvas}/invites"),
            json!({ "email": ben.email }),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{inv}");
    let ev = es.until(T, |e| e.event == "canvas").await.unwrap();
    assert!(is(&ev, "ben", "invited"), "{ev:?}");

    // A user linked after the stream opened is known immediately.
    let cat = t.provision(&app, "cat").await;
    t.canvas(&cat, "cat's").await;
    let ev = es.until(T, |e| e.event == "canvas").await.unwrap();
    assert!(is(&ev, "cat", "created"), "{ev:?}");

    // Nothing else arrived for zed or the human.
    let zed_id = zed.user_id.unwrap().to_string();
    let hum_id = human.user_id.unwrap().to_string();
    t.canvas(&ann, "flush").await;
    let ev = es.until(T, |e| e.event == "canvas").await.unwrap();
    let d = data(&ev);
    assert_ne!(d["user_id"], zed_id);
    assert_ne!(d["user_id"], hum_id);
    assert!(is(&ev, "ann", "created"), "{ev:?}");

    // An unlinked user's events stop.
    let (s, v) = t.delete(&app.me(), "/apps/self/users/cat").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    t.state.events.publish(
        cat.user_id.unwrap(),
        Event::Canvas {
            canvas_id: Uuid::nil(),
            kind: "update".into(),
        },
    );
    t.canvas(&ann, "flush 2").await;
    let ev = es.until(T, |e| e.event == "canvas").await.unwrap();
    assert!(
        is(&ev, "ann", "created"),
        "cat's event was not streamed: {ev:?}"
    );
}

#[tokio::test]
async fn last_event_id_resumes_without_loss_or_duplicates() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let ann = t.provision(&app, "ann").await;
    let ann_id = ann.user_id.unwrap();
    let mut es = EventStream::open(&t, &app.key, None).await;
    let publish = |i: u128| {
        t.state.events.publish(
            ann_id,
            Event::Canvas {
                canvas_id: Uuid::from_u128(i),
                kind: "update".into(),
            },
        )
    };
    for i in 1..=3 {
        publish(i);
    }
    let mut last = String::new();
    for i in 1..=3_u128 {
        let ev = es.next(T).await.unwrap();
        assert_eq!(data(&ev)["canvas_id"], Uuid::from_u128(i).to_string());
        last = ev.id.unwrap();
    }
    drop(es);
    // Missed while disconnected.
    for i in 4..=6 {
        publish(i);
    }
    let mut es = EventStream::open(&t, &app.key, Some(&last)).await;
    publish(7);
    for i in 4..=7_u128 {
        let ev = es.next(T).await.expect("replayed then live");
        assert_eq!(
            data(&ev)["canvas_id"],
            Uuid::from_u128(i).to_string(),
            "in order, once"
        );
    }
    assert!(
        es.next(Duration::from_millis(300)).await.is_none(),
        "no duplicates"
    );

    // Up to date: nothing replayed.
    let now = t.state.events.last_id().to_string();
    let mut es = EventStream::open(&t, &app.key, Some(&now)).await;
    assert!(es.next(Duration::from_millis(300)).await.is_none());

    // An id this process never issued (e.g. from before a restart): resync.
    let mut es = EventStream::open(&t, &app.key, Some("999999999")).await;
    let ev = es.next(T).await.unwrap();
    assert_eq!(ev.event, "resync", "{ev:?}");
    assert_eq!(ev.id.unwrap(), t.state.events.last_id().to_string());
    publish(8);
    let ev = es.next(T).await.unwrap();
    assert_eq!(data(&ev)["canvas_id"], Uuid::from_u128(8).to_string());
}

#[tokio::test]
async fn a_gap_beyond_the_buffer_resyncs() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let ann = t.provision(&app, "ann").await;
    for i in 0..(canvas_cloud_core::events::RING_CAPACITY as u128 + 10) {
        t.state.events.publish(
            ann.user_id.unwrap(),
            Event::Canvas {
                canvas_id: Uuid::from_u128(i),
                kind: "update".into(),
            },
        );
    }
    let mut es = EventStream::open(&t, &app.key, Some("1")).await;
    let ev = es.next(T).await.unwrap();
    assert_eq!(ev.event, "resync");
    assert_eq!(ev.data, "{}");
}

#[tokio::test]
async fn keepalives_flow() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let mut es = EventStream::open(&t, &app.key, None).await;
    // The first keep-alive comes after 15 s; wait a bit longer.
    let got = es.next(Duration::from_secs(17)).await;
    assert!(got.is_none(), "{got:?}");
    assert!(es.comments >= 1, "a `: keepalive` comment arrived");
}

#[tokio::test]
async fn revoking_the_app_ends_the_stream() {
    let t = TestApp::new().await;
    let app = t.app(false, false).await;
    let mut es = EventStream::open(&t, &app.key, None).await;
    sqlx::query("UPDATE apps SET revoked_at = now() WHERE id = $1")
        .bind(app.id)
        .execute(t.db())
        .await
        .unwrap();
    canvas_cloud_apps::directory::reload_now(t.db(), app.id)
        .await
        .unwrap();
    assert!(es.ended(T).await, "stream ended");
    let (s, _) = t.get(&app.me(), "/apps/self/users").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
