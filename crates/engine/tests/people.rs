//! `GET /people` directory scoping: a linked app's users see the app's other users; everyone
//! else sees the people they share a canvas with.

mod common;

use axum::http::{Method, StatusCode};
use canvas_cloud_core::identity::create_app;
use common::{TestApp, User};
use serde_json::Value;
use uuid::Uuid;

/// A linked app with `users` provisioned as `(subject, user)`; returns its key.
async fn app_with_users(app: &TestApp, users: &[(&str, &User)]) -> (Uuid, String) {
    let (app_id, key) = create_app(app.db(), "people tests", false, true)
        .await
        .unwrap();
    for (subject, user) in users {
        sqlx::query("INSERT INTO app_users (app_id, subject, user_id) VALUES ($1, $2, $3)")
            .bind(app_id)
            .bind(subject)
            .bind(user.id)
            .execute(app.db())
            .await
            .unwrap();
    }
    (app_id, key)
}

/// The ids in the caller's directory (`extra` headers, e.g. `X-Canvas-User`).
async fn directory(app: &TestApp, caller: &User, headers: &[(&str, &str)]) -> Vec<Uuid> {
    let (s, v) = app
        .call_with(Some(caller), Method::GET, "/people?limit=50", None, headers)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["people"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| Uuid::parse_str(p["id"].as_str().unwrap()).unwrap())
        .collect()
}

#[tokio::test]
async fn sessions_see_people_they_share_a_canvas_with() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let ben = app.user("ben").await;
    let cy = app.user("cy").await;
    let id = app.canvas(&ann, "Shared").await;
    app.share(&ann, &id, &ben).await;

    let seen = directory(&app, &ann, &[]).await;
    assert!(seen.contains(&ben.id));
    assert!(!seen.contains(&cy.id), "cy shares nothing with ann");
    assert!(!seen.contains(&ann.id), "never the caller");
    let seen = directory(&app, &ben, &[]).await;
    assert_eq!(seen, vec![ann.id]);
    assert_eq!(directory(&app, &cy, &[]).await, Vec::<Uuid>::new());

    // Leaving the canvas leaves the directory too.
    let (s, v) = app
        .delete(&ben, &format!("/canvases/{id}/members/{}", ben.id))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(directory(&app, &ben, &[]).await, Vec::<Uuid>::new());
}

#[tokio::test]
async fn app_users_see_exactly_their_app() {
    let app = TestApp::new().await;
    let u1 = app.user("u1").await;
    let u2 = app.user("u2").await;
    let v1 = app.user("v1").await;
    let v2 = app.user("v2").await;
    let outsider = app.user("outsider").await;
    let (_, key1) = app_with_users(&app, &[("s-u1", &u1), ("s-u2", &u2)]).await;
    let (_, key2) = app_with_users(&app, &[("s-v1", &v1), ("s-v2", &v2)]).await;
    // u1 and v1 share a canvas: the app directory still does not cross apps.
    let id = app.canvas(&u1, "Cross").await;
    app.share(&u1, &id, &v1).await;
    let id2 = app.canvas(&u1, "Outside").await;
    app.share(&u1, &id2, &outsider).await;

    let as_u1 = u1.with_token(&key1);
    let seen = directory(&app, &as_u1, &[("x-canvas-user", "s-u1")]).await;
    assert_eq!(seen, vec![u2.id], "app 1 directory = app 1's other users");
    let acting_v1 = v1.with_token(&key2);
    let seen = directory(&app, &acting_v1, &[("x-canvas-user", "s-v1")]).await;
    assert_eq!(seen, vec![v2.id], "app 2 never sees app 1");

    // The same people signed in on their own: canvas sharers only.
    let seen = directory(&app, &u1, &[]).await;
    assert!(seen.contains(&v1.id) && seen.contains(&outsider.id));
    assert!(!seen.contains(&u2.id));

    // Filters and disabled users still apply inside an app directory.
    sqlx::query("UPDATE users SET disabled = true WHERE id = $1")
        .bind(u2.id)
        .execute(app.db())
        .await
        .unwrap();
    assert_eq!(
        directory(&app, &as_u1, &[("x-canvas-user", "s-u1")]).await,
        Vec::<Uuid>::new()
    );
    let (s, v): (StatusCode, Value) = app
        .call_with(
            Some(&acting_v1),
            Method::GET,
            &format!("/people?q={}", v2.email),
            None,
            &[("x-canvas-user", "s-v1")],
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["people"].as_array().unwrap().len(), 1);
}
