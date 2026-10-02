//! Canvases REST: list/create/rename/delete, Personal canvas, invites, membership scoping.

#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::TestApp;
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use yrs::{Map as _, Out, ReadTxn as _, Transact as _};

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn list_create_rename_delete() {
    let app = TestApp::new().await;
    let a = app.user("ann").await;

    let (s, list) = app.get(&a, "/canvases").await;
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 1);
    let personal = list[0].clone();
    assert_eq!(personal["kind"], "personal");
    assert_eq!(personal["name"], "Personal");
    assert_eq!(personal["role"], "owner");
    assert_eq!(personal["member_count"], 1);
    assert_eq!(personal["owner"]["id"], a.id.to_string());
    assert_eq!(personal["owner"]["email"], a.email);

    let (s, created) = app
        .post(&a, "/canvases", json!({ "name": "  Roadmap  " }))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    assert_eq!(created["name"], "Roadmap");
    assert_eq!(created["kind"], "shared");
    assert_eq!(created["role"], "owner");
    assert_eq!(created["member_count"], 1);
    assert!(created["created_at"].as_str().unwrap().contains('T'));
    let id = created["id"].as_str().unwrap().to_owned();

    let (s, _) = app.post(&a, "/canvases", json!({ "name": "   " })).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app.post(&a, "/canvases", json!({ "nom": "x" })).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app
        .post(&a, "/canvases", json!({ "name": "x".repeat(201) }))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (_, list) = app.get(&a, "/canvases").await;
    let listed = ids(&list);
    assert_eq!(listed.len(), 2);
    assert_eq!(
        listed[0],
        personal["id"].as_str().unwrap(),
        "personal first"
    );
    assert_eq!(listed[1], id);

    let (s, renamed) = app
        .patch(&a, &format!("/canvases/{id}"), json!({ "name": "Plan B" }))
        .await;
    assert_eq!(s, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["name"], "Plan B");
    let (s, one) = app.get(&a, &format!("/canvases/{id}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(one["name"], "Plan B");
    // The document's meta.name follows the rename.
    let doc = app.state_doc(&a, &id).await;
    {
        let txn = doc.transact();
        let meta = txn.get_map("meta").unwrap();
        assert!(
            matches!(meta.get(&txn, "name"), Some(Out::Any(yrs::Any::String(s))) if &*s == "Plan B")
        );
        assert!(meta.get(&txn, "createdBy").is_some());
    }

    let (s, v) = app.delete(&a, &format!("/canvases/{id}")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["ok"], true);
    let (s, _) = app.get(&a, &format!("/canvases/{id}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, list) = app.get(&a, "/canvases").await;
    assert_eq!(
        ids(&list),
        vec![personal["id"].as_str().unwrap().to_owned()]
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM canvas_updates WHERE canvas_id = $1::uuid")
            .bind(&id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(rows, 0, "updates cascade with the canvas");
}

#[tokio::test]
async fn personal_canvas_is_automatic_unique_and_unshareable() {
    let app = TestApp::new().await;
    let a = app.user("pat").await;
    let b = app.user("bea").await;

    // Concurrent first use creates exactly one Personal canvas.
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let state = app.state.clone();
        let uid = a.id;
        tasks.push(tokio::spawn(async move {
            copper_cloud_canvas::ensure_personal_canvas(&state, uid)
                .await
                .unwrap()
        }));
    }
    let mut got = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap());
    }
    got.dedup();
    assert_eq!(got.len(), 1, "{got:?}");
    let pid = got[0].to_string();

    let (s, one) = app.get(&a, "/canvases/personal").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(one["id"], pid.as_str());
    let (s, v) = app.get(&a, "/canvases/personal/state").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["state"].is_string());

    let p = format!("/canvases/{pid}");
    let (s, _) = app
        .post(&a, &format!("{p}/invites"), json!({ "email": b.email }))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app.delete(&a, &p).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app.patch(&a, &p, json!({ "name": "Mine" })).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app.delete(&a, &format!("{p}/members/{}", a.id)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, members) = app.get(&a, &format!("{p}/members")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(members.as_array().unwrap().len(), 1);

    // Another user's `personal` is their own.
    let (_, theirs) = app.get(&b, "/canvases/personal").await;
    assert_ne!(theirs["id"], pid.as_str());
    let (s, _) = app.get(&b, &p).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invite_accept_decline_and_leave() {
    let app = TestApp::new().await;
    let a = app.user("owner").await;
    let b = app.user("bob").await;
    let c = app.user("cat").await;
    let id = app.canvas(&a, "Team").await;
    let base = format!("/canvases/{id}");

    // Invite by email (case-insensitive), idempotent while pending.
    let (s, inv) = app
        .post(
            &a,
            &format!("{base}/invites"),
            json!({ "email": b.email.to_uppercase() }),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{inv}");
    assert_eq!(inv["email"], b.email);
    assert_eq!(inv["status"], "pending");
    assert_eq!(inv["canvas_name"], "Team");
    assert_eq!(inv["invited_by"]["id"], a.id.to_string());
    assert!(inv.get("token").is_none(), "tokens are never exposed");
    let (s, again) = app
        .post(&a, &format!("{base}/invites"), json!({ "email": b.email }))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(again["id"], inv["id"]);
    let (s, _) = app
        .post(&a, &format!("{base}/invites"), json!({ "email": "nope" }))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app
        .post(&a, &format!("{base}/invites"), json!({ "email": a.email }))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // An email with no account yet is still a valid invite.
    let (s, _) = app
        .post(
            &a,
            &format!("{base}/invites"),
            json!({ "email": "later@example.test" }),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, pending) = app.get(&a, &format!("{base}/invites")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(pending.as_array().unwrap().len(), 2);

    // Before accepting, B cannot see the canvas.
    let (s, _) = app.get(&b, &base).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, mine) = app.get(&b, "/invites").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(mine.as_array().unwrap().len(), 1);
    assert_eq!(mine[0]["canvas_id"], id.as_str());
    assert_eq!(mine[0]["canvas_name"], "Team");
    assert_eq!(mine[0]["invited_by"]["email"], a.email);
    // Someone else cannot accept B's invite.
    let inv_id = inv["id"].as_str().unwrap();
    let (s, _) = app
        .post(&c, &format!("/invites/{inv_id}/accept"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, joined) = app
        .post(&b, &format!("/invites/{inv_id}/accept"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK, "{joined}");
    assert_eq!(joined["id"], id.as_str());
    assert_eq!(joined["role"], "editor");
    assert_eq!(joined["member_count"], 2);
    let (s, _) = app
        .post(&b, &format!("/invites/{inv_id}/accept"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "an invite is single-use");
    let (_, mine) = app.get(&b, "/invites").await;
    assert_eq!(mine.as_array().unwrap().len(), 0);
    let (_, blist) = app.get(&b, "/canvases").await;
    assert!(ids(&blist).contains(&id));

    let (s, members) = app.get(&b, &format!("{base}/members")).await;
    assert_eq!(s, StatusCode::OK);
    let members = members.as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["role"], "owner");
    assert_eq!(members[1]["user_id"], b.id.to_string());
    assert_eq!(members[1]["role"], "editor");

    // Already a member → 409.
    let (s, _) = app
        .post(&a, &format!("{base}/invites"), json!({ "email": b.email }))
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Editors may invite; C declines.
    let (s, cinv) = app
        .post(&b, &format!("{base}/invites"), json!({ "email": c.email }))
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let cinv_id = cinv["id"].as_str().unwrap();
    let (s, v) = app
        .post(&c, &format!("/invites/{cinv_id}/decline"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, cmine) = app.get(&c, "/invites").await;
    assert_eq!(cmine.as_array().unwrap().len(), 0);
    let (s, _) = app
        .post(&c, &format!("/invites/{cinv_id}/accept"), Value::Null)
        .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "declined invites cannot be accepted"
    );
    let (s, _) = app.get(&c, &base).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Editors cannot rename, delete or remove others.
    let (s, _) = app.patch(&b, &base, json!({ "name": "Mine" })).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = app.delete(&b, &base).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = app.delete(&b, &format!("{base}/members/{}", a.id)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // The owner cannot leave.
    let (s, _) = app.delete(&a, &format!("{base}/members/{}", a.id)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // B leaves; then has no access.
    let (s, v) = app.delete(&b, &format!("{base}/members/{}", b.id)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = app.get(&b, &base).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app.delete(&a, &format!("{base}/members/{}", b.id)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "already gone");

    // Re-invite + accept, then the owner removes B.
    app.share(&a, &id, &b).await;
    let (s, _) = app.delete(&a, &format!("{base}/members/{}", b.id)).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = app.get(&b, &format!("{base}/state")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn non_members_get_404_everywhere() {
    let app = TestApp::new().await;
    let a = app.user("own").await;
    let x = app.user("intruder").await;
    let id = app.canvas(&a, "Secret").await;
    let base = format!("/canvases/{id}");
    let missing = uuid::Uuid::now_v7();

    for path in [
        base.clone(),
        format!("{base}/members"),
        format!("{base}/invites"),
        format!("{base}/state"),
        format!("{base}/read"),
        format!("/canvases/{missing}"),
        "/canvases/not-a-uuid".to_owned(),
    ] {
        let (s, v) = app.get(&x, &path).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "GET {path}: {v}");
    }
    let (s, _) = app.patch(&x, &base, json!({ "name": "pwned" })).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app.delete(&x, &base).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app.delete(&x, &format!("{base}/members/{}", a.id)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app
        .post(&x, &format!("{base}/invites"), json!({ "email": x.email }))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app
        .post(
            &x,
            &format!("{base}/ops"),
            json!({ "ops": [{ "op": "add", "shape": { "type": "sticky" } }] }),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app
        .post(&x, &format!("/invites/{missing}/accept"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // WebSocket upgrade is refused with 404 too.
    let err = app.ws_raw(&id, &x.token).await.unwrap_err();
    assert!(err.to_string().contains("404"), "{err}");

    // The canvas is untouched.
    let (_, one) = app.get(&a, &base).await;
    assert_eq!(one["name"], "Secret");

    // No session → 401; no instance key → 401.
    let (s, v) = app.call(None, Method::GET, "/canvases", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    let err = app.ws_raw(&id, "x".repeat(43).as_str()).await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
}

#[tokio::test]
async fn read_and_state_endpoints() {
    let app = TestApp::new().await;
    let a = app.user("reader").await;
    let id = app.canvas(&a, "Notes").await;
    let base = format!("/canvases/{id}");
    let long = "y".repeat(700);
    let (s, r) = app
        .post(
            &a,
            &format!("{base}/ops"),
            json!({ "ops": [
                { "op": "add", "shape": { "id": "s1", "type": "sticky", "text": long, "x": 0, "y": 0 } },
                { "op": "add", "shape": { "id": "l1", "type": "link", "url": "https://example.com", "title": "Ex", "x": 400, "y": 0 } },
                { "op": "connect", "from": "s1", "to": "l1", "label": "ref" }
            ]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["applied"], 3);

    let (s, read) = app.get(&a, &format!("{base}/read")).await;
    assert_eq!(s, StatusCode::OK, "{read}");
    assert_eq!(read["canvas"]["id"], id.as_str());
    assert_eq!(read["canvas"]["name"], "Notes");
    assert_eq!(read["canvas"]["kind"], "shared");
    assert_eq!(read["selection"], json!([]));
    let shapes = read["shapes"].as_array().unwrap();
    assert_eq!(shapes.len(), 3);
    let s1 = shapes.iter().find(|s| s["id"] == "s1").unwrap();
    assert_eq!(s1["text"].as_str().unwrap().chars().count(), 501);
    assert_eq!(s1["by"], "reader");
    let arrow = shapes.iter().find(|s| s["type"] == "arrow").unwrap();
    assert_eq!(arrow["from"], json!({ "ref": "s1" }));
    assert_eq!(arrow["label"], "ref");
    let (_, full) = app.get(&a, &format!("{base}/read?full=true")).await;
    let s1 = full["shapes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "s1")
        .unwrap()
        .clone();
    assert_eq!(s1["text"].as_str().unwrap().len(), 700);

    // Full state, then a diff against it is (nearly) empty.
    let doc = app.state_doc(&a, &id).await;
    assert_eq!(common::shape_count(&doc), 3);
    let sv = common::sv_b64(&doc);
    let (s, diff) = app
        .get(&a, &format!("{base}/state?sv={}", urlencode(&sv)))
        .await;
    assert_eq!(s, StatusCode::OK);
    let (_, fullstate) = app.get(&a, &format!("{base}/state")).await;
    assert!(diff["state"].as_str().unwrap().len() < fullstate["state"].as_str().unwrap().len());
    let (s, _) = app.get(&a, &format!("{base}/state?sv=!!!")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // A plain request to the socket path is a JSON 400, not a crash.
    let (s, v) = app.get(&a, &format!("{base}/ws")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "bad_request");
    // read filters
    let (_, only) = app
        .get(&a, &format!("{base}/read?types=link,arrow&ids=l1,shape:s1"))
        .await;
    assert_eq!(only["shapes"].as_array().unwrap().len(), 1, "{only}");
    assert_eq!(only["shapes"][0]["id"], "l1");
    assert_eq!(only["count"], 3);
}

#[tokio::test]
async fn share_links_people_and_landing() {
    let app = TestApp::new().await;
    let owner = app.user("owner").await;
    let editor = app.user("editor").await;
    let guest = app.user("guest").await;
    let disabled = app.user("disabled").await;
    let id = app.canvas(&owner, "Shared").await;
    app.share(&owner, &id, &editor).await;
    let base = format!("/canvases/{id}");
    let (s, _) = app.post(&guest, &format!("{base}/links"), json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, created) = app.post(&owner, &format!("{base}/links"), json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let token = created["token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 43);
    assert_eq!(created["role"], "editor");
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT token_sha256 FROM canvas_share_links WHERE id = $1::uuid")
            .bind(created["id"].as_str().unwrap())
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(
        stored.as_slice(),
        &copper_cloud_core::ids::sha256(token.as_bytes())
    );
    assert_ne!(stored.as_slice(), token.as_bytes());

    let (s, list) = app.get(&owner, &format!("{base}/links")).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(list["links"].as_array().unwrap().len(), 1);
    assert!(list["links"][0].get("token").is_none());
    let link_id = created["id"].as_str().unwrap();
    let (s, _) = app.get(&editor, &format!("{base}/links")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = app
        .delete(&editor, &format!("{base}/links/{link_id}"))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, by_editor) = app
        .post(&editor, &format!("{base}/links"), json!({"role":"editor"}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{by_editor}");
    let (s, _) = app
        .post(&owner, &format!("{base}/links"), json!({"role":"owner"}))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, preview) = app.get(&guest, &format!("/canvas-links/{token}")).await;
    assert_eq!(s, StatusCode::OK, "{preview}");
    assert_eq!(preview["member"], false);
    assert_eq!(preview["owner"]["id"], owner.id.to_string());
    let (s, _) = app
        .post(
            &owner,
            &format!("{base}/invites"),
            json!({"email":guest.email}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, joined) = app
        .post(&guest, &format!("/canvas-links/{token}/join"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK, "{joined}");
    assert_eq!(joined["role"], "editor");
    let (s, invites) = app.get(&guest, "/invites").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(invites.as_array().unwrap().len(), 0);
    // Retry is idempotent, and joining as the owner never downgrades them.
    let (s, _) = app
        .post(&guest, &format!("/canvas-links/{token}/join"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, owner_joined) = app
        .post(&owner, &format!("/canvas-links/{token}/join"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(owner_joined["role"], "owner");
    let uses: i64 = sqlx::query_scalar("SELECT uses FROM canvas_share_links WHERE id = $1::uuid")
        .bind(link_id)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(uses, 3);

    let (s, _) = app.delete(&owner, &format!("{base}/links/{link_id}")).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, err) = app.get(&guest, &format!("/canvas-links/{token}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["error"], "link_not_found");
    let (s, _) = app
        .post(&guest, &format!("/canvas-links/{token}/join"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, second) = app.post(&owner, &format!("{base}/links"), json!({})).await;
    assert_eq!(s, StatusCode::CREATED);
    let second_token = second["token"].as_str().unwrap();
    let (s, _) = app.delete(&owner, &format!("{base}/links")).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = app
        .get(&owner, &format!("/canvas-links/{second_token}"))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, third) = app.post(&owner, &format!("{base}/links"), json!({})).await;
    assert_eq!(s, StatusCode::CREATED);
    let third_token = third["token"].as_str().unwrap();
    let (s, people) = app
        .get(&guest, &format!("/people?q={}", editor.email))
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = app.call(None, Method::GET, "/people", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(people["people"].as_array().unwrap().len(), 1);
    assert_eq!(people["people"][0]["display_name"], "editor");
    sqlx::query("UPDATE users SET disabled = true WHERE id = $1")
        .bind(disabled.id)
        .execute(app.db())
        .await
        .unwrap();
    let (s, people) = app
        .get(&guest, &format!("/people?q={}", disabled.email))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(people["people"].as_array().unwrap().len(), 0);

    let (s, _) = app.delete(&owner, &base).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = app
        .get(&guest, &format!("/canvas-links/{third_token}"))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/join/{token}"))
        .header("host", "cloud.example:443")
        .body(Body::empty())
        .unwrap();
    let response = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        response.headers()["content-security-policy"],
        "default-src 'none'; style-src 'unsafe-inline'"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("copper://canvas/join/"));
    assert!(html.contains("cloud=cloud.example:443"));

    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/join/{token}"))
        .header("host", "[::1]")
        .body(Body::empty())
        .unwrap();
    let response = app.router.clone().oneshot(req).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("cloud=[::1]"));

    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/join/{token}"))
        .header("host", "<bad>")
        .body(Body::empty())
        .unwrap();
    let response = app.router.clone().oneshot(req).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(!html.contains("copper://"));
    assert!(!html.contains("<bad>"));
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

#[tokio::test]
async fn bodies_are_bounded() {
    let app = TestApp::new().await;
    let a = app.user("big").await;
    let id = app.canvas(&a, "Big").await;
    let huge = "z".repeat(copper_cloud_canvas::MAX_OPS_BODY_BYTES + 10);
    let (s, v) = app
        .post(
            &a,
            &format!("/canvases/{id}/ops"),
            json!({ "ops": [{ "op": "add", "shape": { "type": "sticky", "text": huge } }] }),
        )
        .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    let (s, v) = app
        .post(
            &a,
            "/canvases",
            json!({ "name": "z".repeat(copper_cloud_canvas::MAX_BODY_BYTES) }),
        )
        .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    // A 2 MiB-class image fits in one ops call.
    let img = format!("data:image/png;base64,{}", "A".repeat(1_500_000));
    let (s, v) = app
        .post(
            &a,
            &format!("/canvases/{id}/ops"),
            json!({ "ops": [{ "op": "add", "shape": { "type": "image", "src": img, "x": 0, "y": 0 } }] }),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["applied"], 1, "{v}");
    // Envelope problems are reported like the page does.
    let (s, v) = app
        .call(
            Some(&a),
            Method::POST,
            &format!("/canvases/{id}/ops"),
            Some(json!("not an object")),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["errors"][0]["index"], -1);
    let (s, v) = app
        .call(
            Some(&a),
            Method::POST,
            &format!("/canvases/{id}/ops"),
            Some(json!({ "ops": 3 })),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["errors"][0]["error"], "`ops` must be an array");
    // Malformed JSON is a 400.
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!("/v1/canvases/{id}/ops"))
        .header("x-copper-instance", &app.instance_key)
        .header("authorization", format!("Bearer {}", a.token))
        .body(axum::body::Body::from("{nope"))
        .unwrap();
    let resp = tower::ServiceExt::oneshot(app.router.clone(), req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // A bare array of ops works too.
    let (s, v) = app
        .call(
            Some(&a),
            Method::POST,
            &format!("/canvases/{id}/ops"),
            Some(json!([{ "op": "add", "shape": { "type": "text", "text": "bare" } }])),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["applied"], 1, "{v}");
}
