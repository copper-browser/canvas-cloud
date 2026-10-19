//! `POST /v1/apps/self/import` (import format v1): a full canvas round trip (rows, sealed
//! mention excerpts, share-link tokens, the document), the digest contract, conflicts and
//! replacement (same id, the owner's Personal canvas), every validation error, limits,
//! atomicity, streamed bodies split anywhere, and randomized histories.

#![allow(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements
)]

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use canvas_cloud_apps::digest::document_digest;
use canvas_cloud_core::ids;
use common::{add_sticky, b64, eventually, ndjson, sticky_update, App, Caller, TestApp, T};
use rand::rngs::StdRng;
use rand::{Rng as _, SeedableRng as _};
use serde_json::{json, Value};
use uuid::Uuid;
use yrs::updates::decoder::Decode as _;
use yrs::{Doc, Map as _, ReadTxn as _, Transact as _, Update, WriteTxn as _};

const TS: &str = "2025-03-04T05:06:07Z";

fn canvas_line(id: Uuid, kind: &str, owner: &Caller) -> Value {
    json!({
        "type": "canvas", "version": 1, "id": id, "name": "Imported board", "kind": kind,
        "owner": owner.subject.as_deref().unwrap(),
        "created_at": "2024-01-02T03:04:05Z", "updated_at": TS,
    })
}

fn member(who: &Caller, role: &str) -> Value {
    json!({ "type": "member", "subject": who.subject.as_deref().unwrap(), "role": role,
            "added_at": TS })
}

fn update(bytes: &[u8]) -> Value {
    json!({ "type": "update", "b64": b64(bytes) })
}

fn end(updates: u64, members: u64, invites: u64, links: u64, mentions: u64) -> Value {
    json!({ "type": "end", "updates": updates, "members": members, "invites": invites,
            "links": links, "mentions": mentions })
}

/// A document built by two clients, delivered as its incremental updates (causal order).
fn two_client_history() -> (Doc, Vec<Vec<u8>>) {
    let a = Doc::with_client_id(11);
    let b = Doc::with_client_id(22);
    let mut out = Vec::new();
    let step = |d: &Doc, f: &dyn Fn(&mut yrs::TransactionMut)| {
        let before = d.transact().state_vector();
        {
            let mut t = d.transact_mut();
            f(&mut t);
        }
        d.transact().encode_state_as_update_v1(&before)
    };
    out.push(step(&a, &|t| {
        let meta = t.get_or_insert_map("meta");
        meta.insert(t, "name", "Imported board");
        add_sticky(t, "s1", "from a", 0.0);
    }));
    {
        let mut t = b.transact_mut();
        t.apply_update(Update::decode_v1(&out[0]).unwrap()).unwrap();
    }
    out.push(step(&b, &|t| add_sticky(t, "s2", "from b ✓", 50.0)));
    out.push(step(&a, &|t| add_sticky(t, "s3", "concurrent", 90.0)));
    out.push(step(&b, &|t| {
        let chat = t.get_or_insert_array("chat");
        yrs::Array::push_back(&chat, t, yrs::any!({"id": "m1", "text": "hi @ben"}));
    }));
    let full = Doc::new();
    {
        let mut t = full.transact_mut();
        for u in &out {
            t.apply_update(Update::decode_v1(u).unwrap()).unwrap();
        }
    }
    (full, out)
}

async fn setup(allow_import: bool) -> (TestApp, App, Caller, Caller) {
    let t = TestApp::new().await;
    let app = t.app(allow_import, false).await;
    let ann = t.provision(&app, "ann").await;
    let ben = t.provision(&app, "ben").await;
    (t, app, ann, ben)
}

fn err(v: &Value) -> &str {
    v["error"].as_str().unwrap_or("")
}

async fn canvas_exists(t: &TestApp, id: Uuid) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM canvases WHERE id = $1)")
        .bind(id)
        .fetch_one(t.db())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_shared_canvas_round_trips() {
    let (t, app, ann, ben) = setup(true).await;
    let (doc, updates) = two_client_history();
    let id = Uuid::new_v4();
    let link_token = ids::random_token();
    let invite_token_hex = ids::sha256_hex(ids::random_token().as_bytes());
    let mention_id = Uuid::new_v4();
    let outsider_email = TestApp::email("Later.Person");
    let mut lines = vec![
        canvas_line(id, "shared", &ann),
        member(&ann, "owner"),
        member(&ben, "editor"),
        json!({ "type": "invite", "id": Uuid::new_v4(), "email": outsider_email,
                "invited_by": "ann", "status": "pending", "created_at": TS,
                "accepted_at": null, "nudged_at": TS,
                "token_sha256_hex": invite_token_hex.to_uppercase() }),
        json!({ "type": "invite", "id": Uuid::new_v4(), "email": ben.email,
                "invited_by": null, "status": "accepted", "created_at": TS,
                "accepted_at": TS, "nudged_at": null,
                "token_sha256_hex": ids::sha256_hex(ids::random_token().as_bytes()) }),
        json!({ "type": "invite", "id": Uuid::new_v4(), "email": "gone@example.test",
                "invited_by": "ann", "status": "revoked", "created_at": TS,
                "accepted_at": null, "nudged_at": null,
                "token_sha256_hex": ids::sha256_hex(ids::random_token().as_bytes()) }),
        json!({ "type": "link", "id": Uuid::new_v4(),
                "token_sha256_hex": ids::sha256_hex(link_token.as_bytes()), "role": "editor",
                "created_by": "ann", "created_at": TS, "uses": 3, "future_field": [1, 2] }),
        json!({ "type": "mention", "id": mention_id, "message_id": "m1", "from": "ben",
                "to": "ann", "excerpt": "hi @ann — üñî", "created_at": TS, "read_at": null }),
    ];
    lines.extend(updates.iter().map(|u| update(u)));
    lines.push(end(4, 2, 3, 1, 1));
    let mut ann_rx = t.state.events.subscribe(ann.user_id.unwrap());

    let (s, v) = t.import(&app, None, ndjson(&lines)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let expected = document_digest(&doc);
    assert_eq!(
        v,
        json!({ "canvas_id": id, "status": "created", "updates_applied": 4, "members": 2,
                "invites": 3, "links": 1, "mentions": 1, "digest": expected })
    );

    // Stored as one compacted snapshot, no update log.
    let stored = t.stored_updates(id).await;
    assert_eq!(stored.len(), 1, "one snapshot");
    let reread = Doc::new();
    reread
        .transact_mut()
        .apply_update(Update::decode_v1(&stored[0]).unwrap())
        .unwrap();
    assert_eq!(document_digest(&reread), expected);
    let (s, d) = t
        .get(&app.me(), &format!("/apps/self/canvases/{id}/digest"))
        .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["digest"], expected);

    // Rows: canvas metadata and provenance, members, invites, link, mention.
    let (s, c) = t.get(&ben, &format!("/canvases/{id}")).await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["name"], "Imported board");
    let (created, updated, by): (time::OffsetDateTime, time::OffsetDateTime, Option<Uuid>) =
        sqlx::query_as(
            "SELECT created_at, updated_at, imported_by_app FROM canvases WHERE id = $1",
        )
        .bind(id)
        .fetch_one(t.db())
        .await
        .unwrap();
    assert_eq!(created.unix_timestamp(), 1_704_164_645);
    assert_eq!(updated.year(), 2025);
    assert_eq!(by, Some(app.id));
    let (s, members) = t.get(&ann, &format!("/canvases/{id}/members")).await;
    assert_eq!(s, StatusCode::OK, "{members}");
    assert_eq!(members.as_array().map_or(0, Vec::len), 2, "{members}");
    let token: String = sqlx::query_scalar(
        "SELECT token FROM canvas_invites WHERE canvas_id = $1 AND status = 'pending'",
    )
    .bind(id)
    .fetch_one(t.db())
    .await
    .unwrap();
    assert_eq!(token, invite_token_hex, "stored lower-case, as given");
    // The share link works with the original token.
    let human = t.human("visitor").await;
    let (s, p) = t.get(&human, &format!("/canvas-links/{link_token}")).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["canvas_id"], id.to_string());
    // The mention excerpt was re-sealed under the new doc key.
    let (s, m) = t.get(&ann, "/mentions").await;
    assert_eq!(s, StatusCode::OK, "{m}");
    assert_eq!(m["mentions"][0]["id"], mention_id.to_string(), "{m}");
    assert_eq!(m["mentions"][0]["excerpt"], "hi @ann — üñî");
    // Someone who signs up later with the invited email sees the pending invite.
    let later = t.human("later").await;
    sqlx::query("UPDATE users SET email = $2 WHERE id = $1")
        .bind(later.user_id.unwrap())
        .bind(outsider_email.to_uppercase())
        .execute(t.db())
        .await
        .unwrap();
    let (s, inv) = t.get(&later, "/invites").await;
    assert_eq!(s, StatusCode::OK, "{inv}");
    assert_eq!(inv.as_array().map_or(0, Vec::len), 1, "{inv}");

    // Live: a peer loads the imported document and can keep editing it.
    let mut peer = t.peer(&ben, &id.to_string()).await;
    assert_eq!(document_digest(&peer.doc), expected);
    peer.edit(|txn| add_sticky(txn, "s4", "after import", 0.0))
        .await;
    let live = document_digest(&peer.doc);
    let me = app.me();
    assert!(
        eventually(T, || {
            let (t, me, live) = (&t, me.clone(), live.clone());
            async move {
                t.get(&me, &format!("/apps/self/canvases/{id}/digest"))
                    .await
                    .1["digest"]
                    == live.as_str()
            }
        })
        .await
    );
    peer.close().await;

    // Members were told; it is listed with its import time; audited.
    let got = common::drain_kinds(&mut ann_rx);
    assert!(got.contains(&(id, "created".to_owned())), "{got:?}");
    let (_, list) = t.get(&app.me(), "/apps/self/canvases").await;
    let row = list["canvases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id.to_string())
        .unwrap()
        .clone();
    assert!(row["imported_at"].is_string(), "{row}");
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_audit WHERE action = 'app.canvas.import' AND target = $1",
    )
    .bind(id.to_string())
    .fetch_one(t.db())
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

#[tokio::test]
async fn import_requires_allow_import() {
    let (t, app, ann, _) = setup(false).await;
    let id = Uuid::new_v4();
    let body = ndjson(&[canvas_line(id, "shared", &ann), end(0, 0, 0, 0, 0)]);
    let (s, v) = t.import(&app, None, body).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(err(&v), "import_not_allowed");
    let (s, v) = t
        .import(&app, Some("sideways"), ndjson(&[end(0, 0, 0, 0, 0)]))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
}

#[tokio::test]
async fn existing_ids_conflict_and_replace_only_own_imports() {
    let (t, app, ann, ben) = setup(true).await;
    let id = Uuid::new_v4();
    let first = sticky_update(3);
    let body = |u: &[u8], with_ben: bool| {
        let mut l = vec![canvas_line(id, "shared", &ann), member(&ann, "owner")];
        if with_ben {
            l.push(member(&ben, "editor"));
        }
        l.push(update(u));
        l.push(end(1, if with_ben { 2 } else { 1 }, 0, 0, 0));
        ndjson(&l)
    };
    let (s, v) = t.import(&app, Some("create"), body(&first, true)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let d1 = v["digest"].as_str().unwrap().to_owned();

    // Again: 409 with the stored digest (a migrator recognises its own earlier import).
    let (s, v) = t.import(&app, None, body(&first, true)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(err(&v), "canvas_exists");
    assert_eq!(v["digest"], d1);

    // Replace while peers are connected: they are told to reconnect (1001) and then see the
    // new document; ben (dropped from the members) is told the canvas is gone.
    let mut ann_peer = t.peer(&ann, &id.to_string()).await;
    let mut ben_peer = t.peer(&ben, &id.to_string()).await;
    let mut ben_rx = t.state.events.subscribe(ben.user_id.unwrap());
    let second = sticky_update(7);
    let (s, v) = t.import(&app, Some("replace"), body(&second, false)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "replaced");
    assert_ne!(v["digest"], d1);
    assert_eq!(ann_peer.wait_closed(T).await, Some(1001));
    assert_eq!(ben_peer.wait_closed(T).await, Some(1001));
    let peer = t.peer(&ann, &id.to_string()).await;
    assert_eq!(document_digest(&peer.doc), v["digest"].as_str().unwrap());
    peer.close().await;
    let (s, _) = t.get(&ben, &format!("/canvases/{id}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "ben is no longer a member");
    assert!(
        eventually(T, || {
            let got = common::drain_kinds(&mut ben_rx);
            async move { got.contains(&(id, "deleted".to_owned())) }
        })
        .await
    );
    let stored = t.stored_updates(id).await;
    assert_eq!(stored.len(), 1, "only the new snapshot, under the new key");

    // A canvas this app did not import cannot be replaced (nor created over).
    let theirs = Uuid::parse_str(&t.canvas(&ann, "made live").await).unwrap();
    let other = |mode| {
        let body = ndjson(&[canvas_line(theirs, "shared", &ann), end(0, 0, 0, 0, 0)]);
        t.import(&app, mode, body)
    };
    let (s, v) = other(Some("replace")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(err(&v), "not_replaceable");
    let (s, v) = other(None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(err(&v), "canvas_exists");
    assert!(
        v["digest"].is_string(),
        "owned by this app's user: digest given"
    );

    // Another app's import with the same id: canvas_exists without a digest.
    let app2 = t.app(true, false).await;
    let zed = t.provision(&app2, "zed").await;
    let body = ndjson(&[canvas_line(id, "shared", &zed), end(0, 0, 0, 0, 0)]);
    let (s, v) = t.import(&app2, None, body.clone()).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.get("digest").is_none(), "{v}");
    let (s, v) = t.import(&app2, Some("replace"), body).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(err(&v), "not_replaceable");
}

#[tokio::test]
async fn personal_canvases_replace_an_empty_one_and_guard_a_used_one() {
    let (t, app, ann, ben) = setup(true).await;
    // Ann's auto-created (empty) Personal canvas.
    let (s, v) = t.get(&ann, "/canvases").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let auto: Uuid =
        sqlx::query_scalar("SELECT id FROM canvases WHERE owner_id = $1 AND kind = 'personal'")
            .bind(ann.user_id.unwrap())
            .fetch_one(t.db())
            .await
            .unwrap();
    let id = Uuid::new_v4();
    let personal = |id: Uuid, who: &Caller, n: usize| {
        ndjson(&[
            canvas_line(id, "personal", who),
            member(who, "owner"),
            update(&sticky_update(n)),
            end(1, 1, 0, 0, 0),
        ])
    };
    let (s, v) = t.import(&app, None, personal(id, &ann, 2)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["status"], "created",
        "an unused Personal canvas is not a replacement"
    );
    assert!(
        !canvas_exists(&t, auto).await,
        "the empty auto-created one is gone"
    );

    // Ben has used his Personal canvas: create refuses, replace takes over.
    let (s, _) = t.get(&ben, "/canvases").await;
    assert_eq!(s, StatusCode::OK);
    let bens: Uuid =
        sqlx::query_scalar("SELECT id FROM canvases WHERE owner_id = $1 AND kind = 'personal'")
            .bind(ben.user_id.unwrap())
            .fetch_one(t.db())
            .await
            .unwrap();
    let mut peer = t.peer(&ben, &bens.to_string()).await;
    peer.edit(|txn| add_sticky(txn, "mine", "keep?", 0.0)).await;
    // Wait until the edit is stored (initial meta + the sticky).
    let deadline = tokio::time::Instant::now() + T;
    while t.stored_updates(bens).await.len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the edit was stored"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let new_id = Uuid::new_v4();
    let (s, v) = t.import(&app, None, personal(new_id, &ben, 1)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(err(&v), "personal_exists");
    assert_eq!(v["canvas_id"], bens.to_string());
    assert!(!canvas_exists(&t, new_id).await);
    let (s, v) = t
        .import(&app, Some("replace"), personal(new_id, &ben, 1))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "replaced");
    assert_eq!(
        peer.wait_closed(T).await,
        Some(4404),
        "the old Personal canvas is gone"
    );
    assert!(!canvas_exists(&t, bens).await);
    let (_, list) = t.get(&ben, "/canvases").await;
    assert!(
        list.to_string().contains(&new_id.to_string()),
        "the imported one is Ben's Personal canvas now: {list}"
    );

    // Re-importing that same Personal canvas: create → 409 with its digest, replace → done
    // (the same-id canvas *is* the owner's Personal canvas; no personal_exists).
    let (s, v) = t.import(&app, None, personal(new_id, &ben, 4)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(err(&v), "canvas_exists");
    assert!(v["digest"].is_string(), "{v}");
    let (s, v) = t
        .import(&app, Some("replace"), personal(new_id, &ben, 4))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "replaced");
    let personal_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM canvases WHERE owner_id = $1 AND kind = 'personal'",
    )
    .bind(ben.user_id.unwrap())
    .fetch_one(t.db())
    .await
    .unwrap();
    assert_eq!(personal_count, 1);

    // Personal canvases take no guests.
    let lone = Uuid::new_v4();
    for (extra, what) in [
        (member(&ann, "editor"), "member"),
        (
            json!({ "type": "link", "id": Uuid::new_v4(), "token_sha256_hex": ids::sha256_hex(b"x"),
                    "role": "editor", "created_by": null, "created_at": TS, "uses": 0 }),
            "link",
        ),
        (
            json!({ "type": "invite", "id": Uuid::new_v4(), "email": "x@example.test",
                    "invited_by": null, "status": "pending", "created_at": TS,
                    "accepted_at": null, "nudged_at": null,
                    "token_sha256_hex": ids::sha256_hex(b"y") }),
            "invite",
        ),
    ] {
        let body = ndjson(&[
            canvas_line(lone, "personal", &ben),
            member(&ben, "owner"),
            extra,
            end(0, 2, 0, 0, 0),
        ]);
        let (s, v) = t.import(&app, Some("replace"), body).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{what}: {v}");
        assert_eq!(err(&v), "bad_line", "{what}");
        assert_eq!(v["line"], 3, "{what}");
    }
    assert!(
        canvas_exists(&t, new_id).await,
        "failed imports changed nothing"
    );
}

/// Every way a body can be wrong: the error code, the line it names, and that nothing was
/// stored.
#[tokio::test]
async fn invalid_bodies_are_rejected_whole() {
    let (t, app, ann, ben) = setup(true).await;
    let (_, updates) = two_client_history();
    let id = Uuid::new_v4();
    let head = canvas_line(id, "shared", &ann);
    let owner = member(&ann, "owner");
    let ok_update = update(&updates[0]);
    let mut bad_version = head.clone();
    bad_version["version"] = json!(2);
    let mut bad_kind = head.clone();
    bad_kind["kind"] = json!("team");
    let mut long_name = head.clone();
    long_name["name"] = json!("n".repeat(201));
    let mut nil_id = head.clone();
    nil_id["id"] = json!(Uuid::nil());
    let mut ghost_owner = head.clone();
    ghost_owner["owner"] = json!("ghost");
    let mention = |excerpt: &str, message_id: &str| {
        json!({ "type": "mention", "id": Uuid::new_v4(), "message_id": message_id,
                "from": "ann", "to": "ben", "excerpt": excerpt, "created_at": TS,
                "read_at": null })
    };
    let cases: Vec<(&str, Vec<u8>, &str, u64)> = vec![
        ("empty body", Vec::new(), "bad_line", 1),
        ("blank lines only", b"\n \n".to_vec(), "bad_line", 2),
        ("not json", b"{nope\n".to_vec(), "bad_line", 1),
        (
            "unknown type",
            ndjson(&[json!({"type": "comment"})]),
            "bad_line",
            1,
        ),
        (
            "member first",
            ndjson(std::slice::from_ref(&owner)),
            "bad_line",
            1,
        ),
        (
            "version 2",
            ndjson(&[bad_version, end(0, 0, 0, 0, 0)]),
            "bad_line",
            1,
        ),
        (
            "kind",
            ndjson(&[bad_kind, end(0, 0, 0, 0, 0)]),
            "bad_line",
            1,
        ),
        (
            "name",
            ndjson(&[long_name, end(0, 0, 0, 0, 0)]),
            "bad_line",
            1,
        ),
        (
            "nil id",
            ndjson(&[nil_id, end(0, 0, 0, 0, 0)]),
            "bad_line",
            1,
        ),
        (
            "owner",
            ndjson(&[ghost_owner, end(0, 0, 0, 0, 0)]),
            "unknown_subject",
            1,
        ),
        (
            "no end",
            ndjson(&[head.clone(), owner.clone()]),
            "bad_line",
            3,
        ),
        (
            "two canvas lines",
            ndjson(&[head.clone(), head.clone(), end(0, 0, 0, 0, 0)]),
            "bad_line",
            2,
        ),
        (
            "member after update",
            ndjson(&[
                head.clone(),
                ok_update.clone(),
                owner.clone(),
                end(1, 1, 0, 0, 0),
            ]),
            "bad_line",
            3,
        ),
        (
            "owner role",
            ndjson(&[head.clone(), member(&ann, "editor"), end(0, 1, 0, 0, 0)]),
            "bad_line",
            2,
        ),
        (
            "editor role",
            ndjson(&[head.clone(), member(&ben, "owner"), end(0, 1, 0, 0, 0)]),
            "bad_line",
            2,
        ),
        (
            "duplicate member",
            ndjson(&[
                head.clone(),
                owner.clone(),
                owner.clone(),
                end(0, 2, 0, 0, 0),
            ]),
            "bad_line",
            3,
        ),
        (
            "unknown member",
            ndjson(&[
                head.clone(),
                json!({"type": "member", "subject": "ghost",
                     "role": "editor", "added_at": TS}),
                end(0, 1, 0, 0, 0),
            ]),
            "unknown_subject",
            2,
        ),
        (
            "invite status",
            ndjson(&[
                head.clone(),
                json!({"type": "invite", "id": Uuid::new_v4(),
                     "email": "a@b.test", "invited_by": null, "status": "lost",
                     "created_at": TS, "accepted_at": null, "nudged_at": null,
                     "token_sha256_hex": ids::sha256_hex(b"t")}),
                end(0, 0, 1, 0, 0),
            ]),
            "bad_line",
            2,
        ),
        (
            "invite token",
            ndjson(&[
                head.clone(),
                json!({"type": "invite", "id": Uuid::new_v4(),
                     "email": "a@b.test", "invited_by": null, "status": "pending",
                     "created_at": TS, "accepted_at": null, "nudged_at": null,
                     "token_sha256_hex": "abc"}),
                end(0, 0, 1, 0, 0),
            ]),
            "bad_line",
            2,
        ),
        (
            "link role",
            ndjson(&[
                head.clone(),
                json!({"type": "link", "id": Uuid::new_v4(),
                     "token_sha256_hex": ids::sha256_hex(b"l"), "role": "viewer",
                     "created_by": null, "created_at": TS, "uses": 0}),
                end(0, 0, 0, 1, 0),
            ]),
            "bad_line",
            2,
        ),
        (
            "link uses",
            ndjson(&[
                head.clone(),
                json!({"type": "link", "id": Uuid::new_v4(),
                     "token_sha256_hex": ids::sha256_hex(b"l"), "role": "editor",
                     "created_by": null, "created_at": TS, "uses": -1}),
                end(0, 0, 0, 1, 0),
            ]),
            "bad_line",
            2,
        ),
        (
            "long excerpt",
            ndjson(&[
                head.clone(),
                mention(&"e".repeat(201), "m"),
                end(0, 0, 0, 0, 1),
            ]),
            "bad_line",
            2,
        ),
        (
            "long message id",
            ndjson(&[
                head.clone(),
                mention("e", &"m".repeat(129)),
                end(0, 0, 0, 0, 1),
            ]),
            "bad_line",
            2,
        ),
        (
            "duplicate mention",
            ndjson(&[
                head.clone(),
                mention("e", "m"),
                mention("f", "m"),
                end(0, 0, 0, 0, 2),
            ]),
            "bad_line",
            3,
        ),
        (
            "bad base64",
            ndjson(&[
                head.clone(),
                json!({"type": "update", "b64": "!!!"}),
                end(1, 0, 0, 0, 0),
            ]),
            "bad_update",
            2,
        ),
        (
            "not an update",
            ndjson(&[
                head.clone(),
                update(&[0xff, 0xff, 0xff, 0xff, 0x0f]),
                end(1, 0, 0, 0, 0),
            ]),
            "bad_update",
            2,
        ),
        (
            "count mismatch",
            ndjson(&[
                head.clone(),
                owner.clone(),
                ok_update.clone(),
                end(2, 1, 0, 0, 0),
            ]),
            "count_mismatch",
            4,
        ),
        (
            "missing history",
            ndjson(&[head.clone(), update(&updates[2]), end(1, 0, 0, 0, 0)]),
            "incomplete_history",
            3,
        ),
        (
            // updates[3] continues client 22 after updates[1], which is left out.
            "gap in the middle",
            ndjson(&[
                head.clone(),
                update(&updates[0]),
                update(&updates[3]),
                end(2, 0, 0, 0, 0),
            ]),
            "incomplete_history",
            4,
        ),
        (
            "after end",
            ndjson(&[head.clone(), end(0, 0, 0, 0, 0), owner.clone()]),
            "bad_line",
            3,
        ),
    ];
    for (what, body, code, line) in cases {
        let (s, v) = t.import(&app, None, body).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{what}: {v}");
        assert_eq!(err(&v), code, "{what}: {v}");
        assert_eq!(v["line"], line, "{what}: {v}");
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .starts_with(&format!("line {line}: ")),
            "{what}: {v}"
        );
        assert!(!canvas_exists(&t, id).await, "{what}: nothing stored");
    }
    // Nothing leaked from the failed transactions either.
    let rows: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM canvas_members WHERE canvas_id = $1)
              + (SELECT count(*) FROM canvas_mentions WHERE canvas_id = $1)
              + (SELECT count(*) FROM canvas_invites WHERE canvas_id = $1)",
    )
    .bind(id)
    .fetch_one(t.db())
    .await
    .unwrap();
    assert_eq!(rows, 0);
    // Bad mode.
    let (s, v) = t
        .import(
            &app,
            Some("merge"),
            ndjson(&[canvas_line(id, "shared", &ann)]),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}

#[tokio::test]
async fn blank_lines_crlf_unknown_fields_and_a_missing_final_newline() {
    let (t, app, ann, _) = setup(true).await;
    let id = Uuid::new_v4();
    let u = sticky_update(2);
    let mut head = canvas_line(id, "shared", &ann);
    head["extra"] = json!({"ignored": true});
    let body = format!(
        "\r\n{}\r\n\n  \n{}\r\n{}\r\n{}",
        head,
        member(&ann, "owner"),
        update(&u),
        end(1, 1, 0, 0, 0)
    );
    let (s, v) = t.import(&app, None, body.into_bytes()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let doc = Doc::new();
    doc.transact_mut()
        .apply_update(Update::decode_v1(&u).unwrap())
        .unwrap();
    assert_eq!(v["digest"], document_digest(&doc));
    // The owner's member line is optional (added from the canvas line).
    let id2 = Uuid::new_v4();
    let body = ndjson(&[canvas_line(id2, "shared", &ann), end(0, 0, 0, 0, 0)]);
    let (s, v) = t.import(&app, None, body).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["members"], 0);
    assert_eq!(
        v["digest"],
        canvas_cloud_apps::digest::EMPTY_DOCUMENT_DIGEST
    );
    let (s, c) = t.get(&ann, &format!("/canvases/{id2}")).await;
    assert_eq!(s, StatusCode::OK, "the owner is a member: {c}");
}

#[tokio::test]
async fn limits_are_enforced_with_413() {
    let t = TestApp::with(|cfg| {
        cfg.limits.import_max_bytes = 64 * 1024;
        cfg.limits.import_max_line_bytes = 8 * 1024;
    })
    .await;
    let app = t.app(true, false).await;
    let ann = t.provision(&app, "ann").await;
    let id = Uuid::new_v4();
    // One line over the line limit.
    let big = vec![7_u8; 9 * 1024];
    let body = ndjson(&[
        canvas_line(id, "shared", &ann),
        json!({"type": "update", "b64": b64(&big)}),
        end(1, 0, 0, 0, 0),
    ]);
    let (s, v) = t.import(&app, None, body).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(v["line"], 2, "{v}");
    // A last line without a newline, over the limit.
    let mut body = ndjson(&[canvas_line(id, "shared", &ann)]);
    body.extend(std::iter::repeat_n(b' ', 9 * 1024));
    let (s, v) = t.import(&app, None, body).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    // Many small lines over the body limit.
    let mut lines = vec![canvas_line(id, "shared", &ann)];
    for _ in 0..100 {
        lines.push(json!({"type": "update", "b64": b64(&vec![1_u8; 1024])}));
    }
    let (s, v) = t.import(&app, None, ndjson(&lines)).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert!(!canvas_exists(&t, id).await);
    // A declared Content-Length over the limit is refused up front.
    let req = Request::post("/v1/apps/self/import")
        .header("authorization", format!("Bearer {}", app.key))
        .header("content-length", (64 * 1024 + 1).to_string())
        .body(Body::from(vec![b'\n'; 64 * 1024 + 1]))
        .unwrap();
    let (s, v) = t.send(req).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(err(&v), "payload_too_large");
}

/// A streamed body, split into random chunks, through a real socket.
async fn stream_import(t: &TestApp, app: &App, body: Vec<u8>, rng: &mut StdRng) -> (u16, Value) {
    let mut chunks: Vec<Result<bytes::Bytes, std::io::Error>> = Vec::new();
    let mut rest = &body[..];
    while !rest.is_empty() {
        let n = rng.gen_range(1..=rest.len().min(4096));
        chunks.push(Ok(bytes::Bytes::copy_from_slice(&rest[..n])));
        rest = &rest[n..];
    }
    let resp = reqwest::Client::new()
        .post(t.url("/apps/self/import"))
        .header("authorization", format!("Bearer {}", app.key))
        .header("content-type", "application/x-ndjson")
        .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Random multi-client histories (seeded): whatever the delivery chunking, the import
/// answers the digest of the document its updates build, and stores exactly that.
#[tokio::test]
async fn random_histories_import_exactly() {
    let (t, app, ann, ben) = setup(true).await;
    for seed in 0..12_u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let clients: Vec<Doc> = (1..=3).map(|c| Doc::with_client_id(100 + c)).collect();
        let mut history: Vec<Vec<u8>> = Vec::new();
        for _ in 0..rng.gen_range(1..40) {
            let who = rng.gen_range(0..clients.len());
            if rng.gen_bool(0.4) {
                let mut txn = clients[who].transact_mut();
                for u in &history {
                    txn.apply_update(Update::decode_v1(u).unwrap()).unwrap();
                }
            }
            let d = &clients[who];
            let before = d.transact().state_vector();
            {
                let mut txn = d.transact_mut();
                let key = format!("k{}", rng.gen_range(0..8));
                match rng.gen_range(0..3) {
                    0 => add_sticky(&mut txn, &key, &"x".repeat(rng.gen_range(0..300)), 1.5),
                    1 => {
                        let shapes = txn.get_or_insert_map("shapes");
                        shapes.remove(&mut txn, &key);
                    }
                    _ => {
                        let meta = txn.get_or_insert_map("meta");
                        meta.insert(&mut txn, key.as_str(), rng.gen::<f64>());
                    }
                }
            }
            history.push(d.transact().encode_state_as_update_v1(&before));
        }
        let full = Doc::new();
        {
            let mut txn = full.transact_mut();
            for u in &history {
                txn.apply_update(Update::decode_v1(u).unwrap()).unwrap();
            }
        }
        let expected = document_digest(&full);
        let id = Uuid::new_v4();
        let mut lines = vec![
            canvas_line(id, "shared", &ann),
            member(&ann, "owner"),
            member(&ben, "editor"),
        ];
        lines.extend(history.iter().map(|u| update(u)));
        lines.push(end(history.len() as u64, 2, 0, 0, 0));
        let (s, v) = stream_import(&t, &app, ndjson(&lines), &mut rng).await;
        assert_eq!(s, 200, "seed {seed}: {v}");
        assert_eq!(v["digest"], expected, "seed {seed}");
        assert_eq!(v["updates_applied"], history.len(), "seed {seed}");
        let stored = t.stored_updates(id).await;
        let back = Doc::new();
        back.transact_mut()
            .apply_update(Update::decode_v1(&stored[0]).unwrap())
            .unwrap();
        assert_eq!(document_digest(&back), expected, "seed {seed}");
    }
}

/// What a migrator does: export a live canvas's stored history (snapshot + log) and import
/// it elsewhere — the digests agree.
#[tokio::test]
async fn a_live_canvas_exported_and_imported_keeps_its_digest() {
    let (t, app, ann, ben) = setup(true).await;
    let src = t.canvas(&ann, "live").await;
    t.share(&ann, &src, &ben).await;
    let mut pa = t.peer(&ann, &src).await;
    let mut pb = t.peer(&ben, &src).await;
    for i in 0..10 {
        pa.edit(|txn| add_sticky(txn, &format!("a{i}"), "a", 0.0))
            .await;
        pb.edit(|txn| add_sticky(txn, &format!("b{i}"), "b", 1.0))
            .await;
    }
    let all = |p: &common::Peer| {
        let txn = p.doc.transact();
        txn.get_map("shapes").map_or(0, |m| m.len(&txn)) == 20
    };
    assert!(pa.pump_until(T, all).await);
    assert!(pb.pump_until(T, all).await);
    let target = document_digest(&pa.doc);
    assert_eq!(document_digest(&pb.doc), target);
    pa.close().await;
    pb.close().await;
    let src_id = Uuid::parse_str(&src).unwrap();
    let (_, d) = t
        .get(&app.me(), &format!("/apps/self/canvases/{src}/digest"))
        .await;
    assert_eq!(d["digest"], target);

    let history = t.stored_updates(src_id).await;
    let id = Uuid::new_v4();
    let mut lines = vec![
        canvas_line(id, "shared", &ann),
        member(&ann, "owner"),
        member(&ben, "editor"),
    ];
    lines.extend(history.iter().map(|u| update(u)));
    lines.push(end(history.len() as u64, 2, 0, 0, 0));
    let (s, v) = t.import(&app, None, ndjson(&lines)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["digest"], target);
}

/// A client that stops sending mid-body: `400`, nothing stored.
#[tokio::test]
async fn an_aborted_body_stores_nothing() {
    let (t, app, ann, _) = setup(true).await;
    let id = Uuid::new_v4();
    let head = ndjson(&[canvas_line(id, "shared", &ann), member(&ann, "owner")]);
    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![
        Ok(head.into()),
        Err(std::io::Error::other("client went away")),
    ];
    let req = Request::post("/v1/apps/self/import")
        .header("authorization", format!("Bearer {}", app.key))
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap();
    let (s, v) = t.send(req).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(!canvas_exists(&t, id).await);
    // The same canvas imports fine afterwards.
    let body = ndjson(&[canvas_line(id, "shared", &ann), end(0, 0, 0, 0, 0)]);
    let (s, v) = t.import(&app, None, body).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// Two imports of one id at once: exactly one wins, the other gets `409 canvas_exists`.
#[tokio::test]
async fn concurrent_imports_of_one_id() {
    let (t, app, ann, _) = setup(true).await;
    let id = Uuid::new_v4();
    let body = ndjson(&[
        canvas_line(id, "shared", &ann),
        update(&sticky_update(5)),
        end(1, 0, 0, 0, 0),
    ]);
    let (a, b) = tokio::join!(
        t.import(&app, None, body.clone()),
        t.import(&app, None, body.clone())
    );
    let mut statuses = [a.0, b.0];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "{a:?} {b:?}"
    );
    let loser = if a.0 == StatusCode::CONFLICT {
        &a.1
    } else {
        &b.1
    };
    assert_eq!(err(loser), "canvas_exists");
}

/// The brief's round trip: a real canvas built through the REST `/ops` API (shapes, a
/// checklist) and a chat typed by a live peer; its raw stored history exported from the
/// database and imported under a new id reads back identically (`/read?full=true`).
#[tokio::test]
async fn an_ops_built_canvas_reads_the_same_after_import() {
    let (t, app, ann, ben) = setup(true).await;
    let src = t.canvas(&ann, "Team dinners").await;
    t.share(&ann, &src, &ben).await;
    let (s, r) = t
        .post(
            &ann,
            &format!("/canvases/{src}/ops"),
            json!({ "ops": [
                { "op": "add", "shape": { "id": "s1", "type": "sticky", "text": "Where?" } },
                { "op": "add", "shape": { "id": "t1", "type": "text", "x": 300, "text": "Plan" } },
                { "op": "add", "shape": { "id": "c1", "type": "checklist", "title": "Tue",
                  "columns": ["Ann", "Ben"], "rows": ["Pizza", "Sushi"] } },
            ] }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, r) = t
        .post(
            &ben,
            &format!("/canvases/{src}/ops"),
            json!({ "ops": [{ "op": "update", "id": "s1", "props": { "text": "Where? Pizza!" } }] }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let mut peer = t.peer(&ben, &src).await;
    peer.edit(|txn| {
        let chat = txn.get_or_insert_array("chat");
        yrs::Array::push_back(
            &chat,
            txn,
            yrs::any!({"id": "m1", "authorId": "ben", "authorName": "Ben", "text": "pizza it is",
                       "mentions": [], "at": 1_790_000_000_000_i64}),
        );
    })
    .await;
    let src_id = Uuid::parse_str(&src).unwrap();
    let deadline = tokio::time::Instant::now() + T;
    loop {
        let (_, read) = t
            .get(&ann, &format!("/canvases/{src}/read?full=true"))
            .await;
        if read["chat"].as_array().is_some_and(|c| !c.is_empty()) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "chat stored");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    peer.close().await;
    let (_, original) = t
        .get(&ann, &format!("/canvases/{src}/read?full=true"))
        .await;

    let history = t.stored_updates(src_id).await;
    let id = Uuid::new_v4();
    let mut head = canvas_line(id, "shared", &ann);
    head["name"] = json!("Team dinners");
    let mut lines = vec![head, member(&ann, "owner"), member(&ben, "editor")];
    lines.extend(history.iter().map(|u| update(u)));
    lines.push(end(history.len() as u64, 2, 0, 0, 0));
    let (s, v) = t.import(&app, None, ndjson(&lines)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, digest) = t
        .get(&app.me(), &format!("/apps/self/canvases/{src}/digest"))
        .await;
    assert_eq!(v["digest"], digest["digest"]);

    let (s, imported) = t.get(&ben, &format!("/canvases/{id}/read?full=true")).await;
    assert_eq!(s, StatusCode::OK, "{imported}");
    let strip = |mut v: Value| {
        if let Some(m) = v.as_object_mut() {
            m.remove("id");
            m.remove("canvas_id");
            m.remove("canvas");
        }
        v
    };
    assert_eq!(strip(imported.clone()), strip(original.clone()));
    assert!(original.to_string().contains("Pizza!"), "{original}");
    assert!(original.to_string().contains("pizza it is"), "{original}");
}

/// An imported pending invite is accepted by whoever signs up with that email.
#[tokio::test]
async fn an_imported_invite_is_accepted_by_email() {
    let (t, app, ann, _) = setup(true).await;
    let guest = t.human("Guest").await;
    let id = Uuid::new_v4();
    let invite = Uuid::new_v4();
    let body = ndjson(&[
        canvas_line(id, "shared", &ann),
        member(&ann, "owner"),
        json!({ "type": "invite", "id": invite, "email": guest.email.to_uppercase(),
                "invited_by": "ann", "status": "pending", "created_at": TS,
                "accepted_at": null, "nudged_at": null,
                "token_sha256_hex": ids::sha256_hex(ids::random_token().as_bytes()) }),
        end(0, 1, 1, 0, 0),
    ]);
    let (s, v) = t.import(&app, None, body).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = t
        .post(&guest, &format!("/invites/{invite}/accept"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = t.get(&guest, &format!("/canvases/{id}")).await;
    assert_eq!(s, StatusCode::OK);
}

/// The example in `docs/import-format.md` imports as documented (keeps the doc honest).
#[tokio::test]
async fn the_documented_example_imports() {
    let doc = include_str!("../../../docs/import-format.md");
    let start = doc.find("<<'NDJSON'\n").expect("example block") + "<<'NDJSON'\n".len();
    // Through the newline that ends the last NDJSON line.
    let last_newline = start + doc[start..].find("\nNDJSON\n").unwrap();
    let body = &doc[start..=last_newline];
    let documented = doc
        .split("\"digest\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap();
    let t = TestApp::new().await;
    let app = t.app(true, false).await;
    t.provision(&app, "ann").await;
    t.provision(&app, "ben").await;
    // The example's fixed id: start from a clean slate in a reused test database.
    let id = Uuid::parse_str("0190f7a2-6c1d-7c3e-9a51-2b7d8e4f1a20").unwrap();
    sqlx::query("DELETE FROM canvases WHERE id = $1")
        .bind(id)
        .execute(t.db())
        .await
        .unwrap();
    let (s, v) = t
        .import(&app, Some("create"), body.as_bytes().to_vec())
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "created");
    assert_eq!(v["members"], 2);
    assert_eq!(v["digest"], documented);
}
