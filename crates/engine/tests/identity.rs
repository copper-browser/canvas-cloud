//! The engine on canvas-cloud identity: every credential kind reaches the same canvases with
//! the same user id; read-only tokens can read but not write or join rooms; app keys act for
//! provisioned users only. Plus the in-process service API (`canvas_cloud_engine::api`), the
//! importer storage path (`store::write_snapshot_tx`) and configured link schemes.

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use canvas_cloud_core::identity::{AuthUser, Scope, Via};
use canvas_cloud_core::ids;
use canvas_cloud_engine::api;
use canvas_cloud_engine::store;
use common::TestApp;
use serde_json::json;
use tokio_tungstenite::tungstenite::Error as WsError;
use uuid::Uuid;
use yrs::{Doc, Map as _, ReadTxn as _, Transact as _, WriteTxn as _};

const APP_USER: &str = "x-canvas-user";

#[tokio::test]
async fn personal_access_tokens_and_scopes() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let id = app.canvas(&ann, "Scoped").await;

    let write = ann.with_token(app.pat(&ann, Scope::Write).await);
    assert!(write.token.starts_with("cvt_"));
    let (s, list) = app.get(&write, "/canvases").await;
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 2, "{list}");
    let (s, v) = app
        .post(
            &write,
            &format!("/canvases/{id}/ops"),
            json!({ "ops": [{ "op": "add", "shape": { "type": "sticky", "text": "pat" } }] }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["applied"], 1);

    let read = ann.with_token(app.pat(&ann, Scope::Read).await);
    let (s, v) = app.get(&read, &format!("/canvases/{id}/read")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["shapes"].as_array().unwrap().len(), 1, "{v}");
    for (method, path, body) in [
        (
            Method::POST,
            "/canvases".to_owned(),
            Some(json!({ "name": "No" })),
        ),
        (
            Method::POST,
            format!("/canvases/{id}/ops"),
            Some(json!({ "ops": [] })),
        ),
        (
            Method::PATCH,
            format!("/canvases/{id}"),
            Some(json!({ "name": "No" })),
        ),
        (Method::DELETE, format!("/canvases/{id}"), None),
    ] {
        let (s, v) = app.call(Some(&read), method.clone(), &path, body).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {path}: {v}");
        assert_eq!(v["error"], "read_only_token", "{v}");
    }

    // A read-only token cannot join a room (it could write through it).
    match app.ws_raw(&id, &read.token).await {
        Err(WsError::Http(resp)) => assert_eq!(resp.status(), StatusCode::FORBIDDEN),
        Err(e) => panic!("unexpected handshake error: {e}"),
        Ok(_) => panic!("read-only token joined a room"),
    }
    // A write token can.
    let peer = app.peer(&write, &id).await;
    assert_eq!(peer.shape_count(), 1);
    peer.close().await;

    // Revoked or expired tokens are 401.
    sqlx::query("UPDATE access_tokens SET revoked_at = now() WHERE user_id = $1")
        .bind(ann.id)
        .execute(app.db())
        .await
        .unwrap();
    let (s, v) = app.get(&read, "/canvases").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["error"], "unauthorized");
}

#[tokio::test]
async fn app_keys_act_for_provisioned_users() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let id = app.canvas(&ann, "Linked").await;
    let key = app.app_key_for(&ann, "ann-in-app").await;
    let via_app = ann.with_token(key.clone());

    // With the subject header the app is Ann: same canvases, same user id on the wire.
    let (s, v) = app
        .call_with(
            Some(&via_app),
            Method::GET,
            &format!("/canvases/{id}/members"),
            None,
            &[(APP_USER, "ann-in-app")],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v[0]["user_id"], ann.id.to_string());
    let (s, v) = app
        .call_with(
            Some(&via_app),
            Method::POST,
            "/canvases",
            Some(json!({ "name": "Made by the app" })),
            &[(APP_USER, "ann-in-app")],
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["owner"]["id"], ann.id.to_string());

    // Unknown subject → 401 unknown_user; no subject → 401.
    let (s, v) = app
        .call_with(
            Some(&via_app),
            Method::GET,
            "/canvases",
            None,
            &[(APP_USER, "nobody")],
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["error"], "unknown_user");
    let (s, v) = app.get(&via_app, "/canvases").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");

    // A disabled user is refused whatever the credential.
    sqlx::query("UPDATE users SET disabled = true WHERE id = $1")
        .bind(ann.id)
        .execute(app.db())
        .await
        .unwrap();
    let (s, v) = app
        .call_with(
            Some(&via_app),
            Method::GET,
            "/canvases",
            None,
            &[(APP_USER, "ann-in-app")],
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["error"], "account_disabled");
    let (s, v) = app.get(&ann, "/canvases").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["error"], "account_disabled");
}

fn auth_user(user: &common::User) -> AuthUser {
    AuthUser {
        user_id: user.id,
        email: user.email.clone(),
        display_name: None,
        via: Via::Session {
            session_id: Uuid::nil(),
        },
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn service_api_matches_rest() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let ben = app.user("ben").await;
    let (a, b) = (auth_user(&ann), auth_user(&ben));
    let state = &app.state;

    let canvas = api::create_canvas(state, &a, "Service").await.unwrap();
    let id = canvas.id.to_string();
    let (s, rest_view) = app.get(&ann, &format!("/canvases/{id}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(serde_json::to_value(&canvas).unwrap(), rest_view);

    let sent = api::create_invite(state, &a, &id, &ben.email)
        .await
        .unwrap();
    assert!(sent.created);
    let again = api::create_invite(state, &a, &id, &ben.email)
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(api::my_invites(state, &b).await.unwrap().len(), 1);
    api::accept_invite(state, &b, &sent.invite.id.to_string())
        .await
        .unwrap();
    assert_eq!(api::list_members(state, &a, &id).await.unwrap().len(), 2);

    let request = api::OpsRequest::from_json(json!({
        "as": { "id": "agent:svc", "name": "Svc" },
        "near": { "x": 500, "y": 500 },
        "ops": [{ "op": "add", "shape": { "id": "s1", "type": "sticky", "text": "hi" } }],
    }))
    .unwrap();
    let result = api::apply_ops(state, &b, &id, request).await.unwrap();
    assert_eq!(result.applied, 1, "{result:?}");
    assert!(api::OpsRequest::from_json(json!(3)).is_err());

    let read = api::read_canvas(
        state,
        &a,
        &id,
        &api::ReadOptions {
            full: true,
            ids: Some(vec!["shape:s1".into()]),
            types: None,
        },
    )
    .await
    .unwrap();
    let read = serde_json::to_value(&read).unwrap();
    assert_eq!(read["shapes"].as_array().unwrap().len(), 1, "{read}");
    assert_eq!(read["shapes"][0]["by"], "Svc", "{read}");
    let (_, rest_read) = app
        .get(&ann, &format!("/canvases/{id}/read?full=true&ids=shape:s1"))
        .await;
    assert_eq!(read["shapes"], rest_read["shapes"]);

    let link = api::create_share_link(state, &a, &id, None).await.unwrap();
    let preview = api::preview_share_link(state, &b, &link.token)
        .await
        .unwrap();
    assert!(preview.member);
    assert_eq!(
        api::list_share_links(state, &a, &id).await.unwrap().len(),
        1
    );
    assert!(matches!(
        api::list_share_links(state, &b, &id).await,
        Err(canvas_cloud_core::error::ApiError::Forbidden)
    ));

    let sent = api::post_mentions(
        state,
        &a,
        &id,
        api::MentionBody {
            message_id: "m1".into(),
            user_ids: vec![ben.id],
            excerpt: "@ben look".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(sent.notified, vec![ben.id]);
    let list = api::list_mentions(state, &b, true, None).await.unwrap();
    assert_eq!(list.unread, 1);
    assert_eq!(list.mentions[0].excerpt, "@ben look");
    let marked = api::mark_read(
        state,
        &b,
        api::MarkReadBody {
            ids: None,
            canvas_id: Some(canvas.id),
        },
    )
    .await
    .unwrap();
    assert_eq!(marked.updated, 1);

    let people = api::list_people(state, &a, Some(&ben.email), None)
        .await
        .unwrap();
    assert!(people.iter().any(|p| p.id == ben.id));

    // Not a member: 404, exactly like REST.
    let carl = app.user("carl").await;
    assert!(matches!(
        api::get_canvas(state, &auth_user(&carl), &id).await,
        Err(canvas_cloud_core::error::ApiError::NotFound)
    ));
    api::delete_canvas(state, &a, &id).await.unwrap();
    let (s, _) = app.get(&ann, &format!("/canvases/{id}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn importer_writes_one_snapshot_in_its_own_transaction() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;

    // The document an importer would have built.
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        common::add_sticky(&mut txn, "imp1", "imported", 0.0);
        let meta = txn.get_or_insert_map("meta");
        meta.insert(&mut txn, "name", "Imported");
    }
    let state_bytes = doc
        .transact()
        .encode_state_as_update_v1(&yrs::StateVector::default());

    let canvas_id = ids::uuid_v7();
    let key = store::new_doc_key();
    let mut tx = app.db().begin().await.unwrap();
    sqlx::query(
        "INSERT INTO canvases (id, owner_id, name, kind, doc_key_wrapped)
         VALUES ($1, $2, 'Imported', 'shared', $3)",
    )
    .bind(canvas_id)
    .bind(ann.id)
    .bind(store::wrap(&app.state.crypto, &key))
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO canvas_members (canvas_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(canvas_id)
        .bind(ann.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    store::write_snapshot_tx(&mut tx, canvas_id, &state_bytes, &key)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT state FROM canvas_snapshots WHERE canvas_id = $1")
            .bind(canvas_id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(store::open(&key, canvas_id, &sealed).unwrap(), state_bytes);
    assert!(store::open(&key, Uuid::nil(), &sealed).is_err());

    let (s, read) = app.get(&ann, &format!("/canvases/{canvas_id}/read")).await;
    assert_eq!(s, StatusCode::OK, "{read}");
    assert_eq!(read["shapes"][0]["id"], "imp1", "{read}");

    // Live edits follow the snapshot and survive a cold reload.
    let (s, v) = app
        .post(
            &ann,
            &format!("/canvases/{canvas_id}/ops"),
            json!({ "ops": [{ "op": "add", "shape": { "id": "live1", "type": "text", "text": "after" } }] }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(
        common::eventually(Duration::from_secs(5), || async {
            canvas_cloud_engine::evict_room_now(canvas_id)
        })
        .await,
        "room evicts"
    );
    assert!(!canvas_cloud_engine::room_in_memory(canvas_id));
    let reloaded = app.state_doc(&ann, &canvas_id.to_string()).await;
    assert_eq!(common::shape_count(&reloaded), 2);
}

#[tokio::test]
async fn extra_url_schemes_come_from_config() {
    let link =
        |url: &str| json!({ "ops": [{ "op": "add", "shape": { "type": "link", "url": url } }] });
    let plain = TestApp::new().await;
    let ann = plain.user("ann").await;
    let id = plain.canvas(&ann, "Links").await;
    let (_, v) = plain
        .post(
            &ann,
            &format!("/canvases/{id}/ops"),
            link("myapp://canvas/1"),
        )
        .await;
    assert_eq!(v["applied"], 0, "{v}");
    assert!(
        v["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("unsupported link scheme myapp:"),
        "{v}"
    );

    let custom = TestApp::with(|cfg| cfg.ops.extra_url_schemes = vec!["myapp".into()]).await;
    let ben = custom.user("ben").await;
    let id = custom.canvas(&ben, "Links").await;
    let (_, v) = custom
        .post(
            &ben,
            &format!("/canvases/{id}/ops"),
            link("myapp://canvas/1"),
        )
        .await;
    assert_eq!(v["applied"], 1, "{v}");
    let (_, read) = custom
        .get(&ben, &format!("/canvases/{id}/read?full=true"))
        .await;
    assert_eq!(read["shapes"][0]["url"], "myapp://canvas/1", "{read}");
}
