//! Instance-admin helpers other crates build on: open-room listing and shutting a room while
//! its document is replaced.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use canvas_cloud_engine::admin;
use common::TestApp;
use tower::ServiceExt as _;
use uuid::Uuid;

#[tokio::test]
async fn shutting_a_room_kicks_peers_with_1001_and_holds_readers() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let id = app.canvas(&ann, "Board").await;
    let canvas_id = Uuid::parse_str(&id).unwrap();
    let mut peer = app.peer(&ann, &id).await;
    assert!(admin::open_rooms()
        .iter()
        .any(|(c, peers)| *c == canvas_id && *peers == 1));

    let router = app.router.clone();
    let token = ann.token.clone();
    let path = format!("/v1/canvases/{id}/read");
    // The shut returns the reader's handle on purpose: it must finish only after the shut.
    #[allow(clippy::async_yields_async)]
    let reader = admin::with_room_shut(canvas_id, async move {
        // A read arriving while the room is shut waits for the shut to end.
        let reader = tokio::spawn(async move {
            let req = Request::get(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !reader.is_finished(),
            "the read must wait for the shut room"
        );
        reader
    })
    .await;
    assert_eq!(reader.await.unwrap(), StatusCode::NOT_FOUND);
    assert_eq!(peer.wait_closed(Duration::from_secs(5)).await, Some(1001));
    assert!(!admin::open_rooms().iter().any(|(c, _)| *c == canvas_id));

    // The next use loads the canvas afresh.
    let (s, v) = app.get(&ann, &format!("/canvases/{id}/read")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let again = app.peer(&ann, &id).await;
    assert!(again.closed.is_none());
    again.close().await;
}

#[tokio::test]
async fn close_room_kicks_with_4404() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let id = app.canvas(&ann, "Board").await;
    let mut peer = app.peer(&ann, &id).await;
    admin::close_room(Uuid::parse_str(&id).unwrap());
    assert_eq!(peer.wait_closed(Duration::from_secs(5)).await, Some(4404));
}
