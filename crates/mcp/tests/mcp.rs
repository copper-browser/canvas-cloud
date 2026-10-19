//! `POST /mcp`, `/llms.txt` and `/v1/openapi.json` over real HTTP against native PostgreSQL.

#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use std::time::Duration;

use canvas_cloud_core::identity::Scope;
use common::TestApp;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

const ALL_TOOLS: [&str; 11] = [
    "whoami",
    "list_canvases",
    "create_canvas",
    "rename_canvas",
    "read_canvas",
    "apply_ops",
    "list_members",
    "invite",
    "create_share_link",
    "list_mentions",
    "mark_mentions_read",
];

fn names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn lifecycle_over_http_with_a_personal_access_token() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let pat = app.pat(&ann, Scope::Write).await;
    let mut mcp = app.mcp(&pat);
    // `initialize` comes before the client knows the version: no header.
    mcp.headers.clear();

    let init = mcp
        .request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "tests", "version": "1" },
            }),
        )
        .await;
    let r = &init["result"];
    assert_eq!(r["protocolVersion"], "2025-06-18");
    assert_eq!(r["serverInfo"]["name"], "canvas-cloud");
    assert_eq!(r["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        r["capabilities"],
        json!({ "tools": { "listChanged": false } })
    );
    let lines = r["instructions"].as_str().unwrap().lines().count();
    assert!((3..=6).contains(&lines), "{lines}");

    mcp.headers = vec![("mcp-protocol-version".into(), "2025-06-18".into())];
    // The initialized notification: 202, no body.
    let (status, body) = mcp
        .send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;
    assert_eq!((status, body), (StatusCode::ACCEPTED, Value::Null));

    let pong = mcp.request("ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));

    let list = mcp.request("tools/list", json!({})).await;
    let tools = &list["result"]["tools"];
    assert_eq!(names(tools), ALL_TOOLS);
    for t in tools.as_array().unwrap() {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert_eq!(t["outputSchema"]["type"], "object", "{t}");
        assert!(t["description"].as_str().unwrap().len() > 60, "{t}");
    }
    let apply = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "apply_ops")
        .unwrap();
    let d = apply["description"].as_str().unwrap();
    for needle in [
        "sticky",
        "checklist",
        "connect",
        "\"op\":\"clear\"",
        "confirm",
        "near",
        "shape:<id>",
    ] {
        assert!(d.contains(needle), "apply_ops description lacks {needle}");
    }
    assert_eq!(apply["annotations"]["destructiveHint"], true);

    let me = mcp.ok("whoami", json!({})).await;
    assert_eq!(me["user"]["id"], ann.id.to_string());
    assert_eq!(me["user"]["email"], ann.email);
    assert_eq!(me["via"], json!({ "kind": "token", "scope": "write" }));
    assert_eq!(me["read_only"], false);
    assert_eq!(me["server"]["public_url"], app.state.cfg.public_url);

    // Method-level errors are JSON-RPC errors; tool-level ones are results.
    let v = mcp.request("resources/list", json!({})).await;
    assert_eq!(v["error"]["code"], -32601);
    let v = mcp
        .request(
            "tools/call",
            json!({ "name": "drop_tables", "arguments": {} }),
        )
        .await;
    assert_eq!(v["error"]["code"], -32602);
    let v = mcp.request("tools/call", json!({ "arguments": {} })).await;
    assert_eq!(v["error"]["code"], -32602);
}

#[tokio::test]
async fn version_negotiation_and_transport_rules() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let mut mcp = app.mcp(&ann.session);
    mcp.headers.clear();

    // The older revision is echoed; an unknown one gets ours.
    let v = mcp
        .request(
            "initialize",
            json!({ "protocolVersion": "2025-03-26", "capabilities": {} }),
        )
        .await;
    assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
    let v = mcp
        .request(
            "initialize",
            json!({ "protocolVersion": "2099-01-01", "capabilities": {} }),
        )
        .await;
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");

    // An unsupported MCP-Protocol-Version header is 400.
    let (s, _, body) = app
        .post_mcp(
            Some(&ann.session),
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
            &[("mcp-protocol-version", "2024-01-01")],
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32600);

    // Malformed JSON: parse error.
    let (s, _, body) = app.post_mcp(Some(&ann.session), "{not json", &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (v["error"]["code"].clone(), v["id"].clone()),
        (json!(-32700), Value::Null)
    );

    // Not a request object.
    let (s, _, body) = app
        .post_mcp(
            Some(&ann.session),
            r#"{"jsonrpc":"1.0","id":3,"method":"ping"}"#,
            &[],
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
        -32600
    );

    // Batches: refused in 2025-06-18, answered in 2025-03-26.
    let batch = r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":2,"method":"tools/list"}]"#;
    let (s, _, body) = app
        .post_mcp(
            Some(&ann.session),
            batch,
            &[("mcp-protocol-version", "2025-06-18")],
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    let (s, _, body) = app.post_mcp(Some(&ann.session), batch, &[]).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "no header means 2025-03-26-or-initialize: {body}"
    );
    let (s, _, body) = app
        .post_mcp(
            Some(&ann.session),
            batch,
            &[("mcp-protocol-version", "2025-03-26")],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let replies = v.as_array().unwrap();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 11);

    // A client response to a (never sent) server request: accepted, no body.
    let (s, _, body) = app
        .post_mcp(
            Some(&ann.session),
            r#"{"jsonrpc":"2.0","id":"x","result":{}}"#,
            &[],
        )
        .await;
    assert_eq!((s, body.as_str()), (StatusCode::ACCEPTED, ""));

    // GET (no server-to-client stream) and other methods: 405, Allow: POST.
    for method in [Method::GET, Method::DELETE, Method::PUT] {
        let r = app
            .http
            .request(method.clone(), app.url("/mcp"))
            .bearer_auth(&ann.session)
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(r.headers()["allow"], "POST");
    }

    // No session id is issued: the server is stateless.
    let (s, h, _) = app
        .post_mcp(
            Some(&ann.session),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            &[],
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("mcp-session-id").is_none());

    // Over the body limit: 413.
    let huge = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{"pad":"{}"}}}}"#,
        "x".repeat(canvas_cloud_mcp::MAX_BODY_BYTES)
    );
    let (s, _, _) = app.post_mcp(Some(&ann.session), huge, &[]).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn unauthenticated_requests_get_401_with_the_bearer_challenge() {
    let app = TestApp::new().await;
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    for token in [None, Some("cvt_not-a-real-token"), Some("garbage")] {
        let (s, h, body) = app.post_mcp(token, ping, &[]).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{token:?}");
        assert_eq!(h["www-authenticate"], "Bearer realm=\"canvas-cloud\"");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(v["error"].is_string(), "{v}");
    }
}

#[tokio::test]
async fn origin_guard() {
    let app = TestApp::with(|cfg| {
        cfg.cors.allowed_origins = vec!["https://app.example.test".into()];
        cfg.public_url = "https://canvas.example.test".into();
    })
    .await;
    let ann = app.user("ann").await;
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    for (origin, want) in [
        (None, StatusCode::OK),
        (Some("https://app.example.test"), StatusCode::OK),
        (Some("https://canvas.example.test"), StatusCode::OK),
        (Some("https://evil.example.test"), StatusCode::FORBIDDEN),
        (Some("http://app.example.test"), StatusCode::FORBIDDEN),
        (Some("null"), StatusCode::FORBIDDEN),
    ] {
        let headers: Vec<(&str, &str)> = origin.map(|o| ("origin", o)).into_iter().collect();
        let (s, _, body) = app.post_mcp(Some(&ann.session), ping, &headers).await;
        assert_eq!(s, want, "{origin:?}: {body}");
        if want == StatusCode::FORBIDDEN {
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["error"]["code"], -32000);
        }
    }
    // The guard runs before authentication: a bad Origin never learns whether a token works.
    let (s, _, _) = app
        .post_mcp(None, ping, &[("origin", "https://evil.example.test")])
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Browser MCP clients on an allowed origin pass the CORS preflight.
    let r = app
        .http
        .request(Method::OPTIONS, app.url("/mcp"))
        .header("origin", "https://app.example.test")
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "authorization,content-type,mcp-protocol-version",
        )
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    assert_eq!(
        r.headers()["access-control-allow-origin"],
        "https://app.example.test"
    );
    let allowed = r.headers()["access-control-allow-headers"]
        .to_str()
        .unwrap();
    assert!(allowed.contains("mcp-protocol-version"), "{allowed}");
}

#[tokio::test]
async fn every_tool_happy_path_and_one_error_each() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let bob = app.user("bob").await;
    let pat = app.pat(&ann, Scope::Write).await;
    let mut mcp = app.mcp(&pat);
    let bob_pat = app.pat(&bob, Scope::Write).await;
    let mut bob_mcp = app.mcp(&bob_pat);

    // list_canvases: Personal first.
    let list = mcp.ok("list_canvases", json!({})).await;
    let canvases = list["canvases"].as_array().unwrap();
    assert_eq!(canvases[0]["kind"], "personal");
    let personal = canvases[0]["id"].as_str().unwrap().to_owned();

    // create_canvas
    let c = mcp
        .ok("create_canvas", json!({ "name": "  Roadmap  " }))
        .await;
    assert_eq!(
        (c["name"].clone(), c["kind"].clone(), c["role"].clone()),
        (json!("Roadmap"), json!("shared"), json!("owner"))
    );
    let canvas = c["id"].as_str().unwrap().to_owned();
    let e = mcp.err("create_canvas", json!({})).await;
    assert_eq!(e["error"], "bad_request");
    assert!(e["message"].as_str().unwrap().contains("name"), "{e}");
    let e = mcp.err("create_canvas", json!({ "name": "   " })).await;
    assert_eq!(e["error"], "bad_request");

    // rename_canvas
    let r = mcp
        .ok(
            "rename_canvas",
            json!({ "canvas": canvas, "name": "Roadmap 2" }),
        )
        .await;
    assert_eq!(r["name"], "Roadmap 2");
    let e = mcp
        .err(
            "rename_canvas",
            json!({ "canvas": "personal", "name": "Mine" }),
        )
        .await;
    assert_eq!(e["error"], "bad_request");

    // apply_ops + read_canvas
    let ops = mcp
        .ok(
            "apply_ops",
            json!({
                "canvas": canvas,
                "as": { "name": "Planner", "color": "purple" },
                "ops": [
                    { "op": "add", "shape": { "type": "sticky", "text": "Ship v1", "id": "goal", "x": 0, "y": 0 } },
                    { "op": "add", "shape": { "type": "sticky", "text": "Risk", "id": "risk", "x": 400, "y": 0 } },
                    { "op": "connect", "from": "goal", "to": "shape:risk", "label": "blocked by" },
                    { "op": "update", "id": "nope", "patch": { "text": "x" } },
                ],
            }),
        )
        .await;
    assert_eq!(ops["applied"], 3, "{ops}");
    assert_eq!(ops["ids"][0], "goal");
    assert_eq!(ops["errors"][0]["index"], 3);
    let read = mcp.ok("read_canvas", json!({ "canvas": canvas })).await;
    assert_eq!(read["canvas"]["name"], "Roadmap 2");
    assert_eq!(read["count"], 3);
    let goal = read["shapes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "goal")
        .unwrap();
    assert_eq!(
        (goal["text"].clone(), goal["by"].clone()),
        (json!("Ship v1"), json!("Planner"))
    );
    let arrow = read["shapes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["type"] == "arrow")
        .unwrap();
    assert_eq!(arrow["label"], "blocked by");
    // ids / types narrow the read; arrays or comma strings both work.
    let only = mcp
        .ok(
            "read_canvas",
            json!({ "canvas": canvas, "types": ["arrow"] }),
        )
        .await;
    assert_eq!(only["shapes"].as_array().unwrap().len(), 1);
    let only = mcp
        .ok(
            "read_canvas",
            json!({ "canvas": canvas, "ids": "goal,risk" }),
        )
        .await;
    assert_eq!(only["shapes"].as_array().unwrap().len(), 2);
    // Strangers learn nothing.
    let e = bob_mcp
        .err("read_canvas", json!({ "canvas": canvas }))
        .await;
    assert_eq!(e["error"], "not_found");
    let e = mcp.err("read_canvas", json!({})).await;
    assert_eq!(e["error"], "bad_request");
    // Nothing applied → the call is an error; clear needs confirm.
    let e = mcp
        .err(
            "apply_ops",
            json!({ "canvas": canvas, "ops": [{ "op": "clear" }] }),
        )
        .await;
    assert_eq!(e["applied"], 0, "{e}");
    assert!(
        e["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("confirm"),
        "{e}"
    );
    let e = mcp
        .err("apply_ops", json!({ "canvas": canvas, "ops": "not ops" }))
        .await;
    assert_eq!(e["error"], "bad_request");
    let e = bob_mcp
        .err("apply_ops", json!({ "canvas": canvas, "ops": "not ops" }))
        .await;
    assert_eq!(
        e["error"], "not_found",
        "membership is checked before the envelope"
    );
    // Ops sent as a JSON string (some clients stringify nested arguments) work too.
    let ok = mcp
        .ok(
            "apply_ops",
            json!({ "canvas": canvas, "ops": "[{\"op\":\"move\",\"id\":\"goal\",\"dx\":10,\"dy\":0}]" }),
        )
        .await;
    assert_eq!(ok["applied"], 1);

    // list_members
    let m = mcp.ok("list_members", json!({ "canvas": canvas })).await;
    let members = m["members"].as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(
        (members[0]["user_id"].clone(), members[0]["role"].clone()),
        (json!(ann.id.to_string()), json!("owner"))
    );
    let e = mcp
        .err(
            "list_members",
            json!({ "canvas": "00000000-0000-0000-0000-000000000000" }),
        )
        .await;
    assert_eq!(e["error"], "not_found");
    assert!(
        e["message"].as_str().unwrap().contains("list_canvases"),
        "{e}"
    );

    // invite
    let i = mcp
        .ok("invite", json!({ "canvas": canvas, "email": bob.email }))
        .await;
    assert_eq!(
        (
            i["status"].clone(),
            i["created"].clone(),
            i["nudged"].clone()
        ),
        (json!("pending"), json!(true), json!(false))
    );
    assert_eq!(i["invitee"]["id"], bob.id.to_string());
    let again = mcp
        .ok("invite", json!({ "canvas": canvas, "email": bob.email }))
        .await;
    assert_eq!(again["created"], false);
    let e = mcp
        .err("invite", json!({ "canvas": canvas, "email": ann.email }))
        .await;
    assert_eq!(e["error"], "bad_request");

    // create_share_link → bob joins over REST.
    let link = mcp
        .ok("create_share_link", json!({ "canvas": canvas }))
        .await;
    let token = link["token"].as_str().unwrap();
    assert_eq!(
        link["url"],
        format!("{}/join/{token}", app.state.cfg.public_url)
    );
    assert_eq!(link["role"], "editor");
    let (s, _) = app
        .rest(
            &bob.session,
            Method::POST,
            &format!("/v1/canvas-links/{token}/join"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let m = mcp.ok("list_members", json!({ "canvas": canvas })).await;
    assert_eq!(m["members"].as_array().unwrap().len(), 2);
    let e = mcp
        .err("create_share_link", json!({ "canvas": "personal" }))
        .await;
    assert_eq!(e["error"], "bad_request");
    assert_eq!(
        personal,
        mcp.ok("read_canvas", json!({ "canvas": "personal" })).await["canvas"]["id"]
    );

    // list_mentions / mark_mentions_read: bob mentions ann in the canvas chat.
    let (s, sent) = app
        .rest(
            &bob.session,
            Method::POST,
            &format!("/v1/canvases/{canvas}/mentions"),
            Some(
                json!({ "message_id": "m1", "user_ids": [ann.id], "excerpt": "@ann look at this" }),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{sent}");
    let l = mcp
        .ok("list_mentions", json!({ "unread_only": true }))
        .await;
    assert_eq!(l["unread"], 1);
    let mention = &l["mentions"][0];
    assert_eq!(mention["excerpt"], "@ann look at this");
    assert_eq!(mention["from"]["id"], bob.id.to_string());
    let e = mcp.err("list_mentions", json!({ "limit": 0 })).await;
    assert_eq!(e["error"], "bad_request");
    let done = mcp
        .ok("mark_mentions_read", json!({ "ids": [mention["id"]] }))
        .await;
    assert_eq!(done, json!({ "ok": true, "updated": 1 }));
    let l = mcp
        .ok("list_mentions", json!({ "unread_only": "true" }))
        .await;
    assert_eq!(
        (l["unread"].clone(), l["mentions"].as_array().unwrap().len()),
        (json!(0), 0)
    );
    let e = mcp
        .err("mark_mentions_read", json!({ "ids": ["not-a-uuid"] }))
        .await;
    assert_eq!(e["error"], "bad_request");

    // mark_mentions_read with nothing: every unread mention; with a canvas: that canvas's.
    for n in 0..3 {
        let (s, _) = app
            .rest(
                &bob.session,
                Method::POST,
                &format!("/v1/canvases/{canvas}/mentions"),
                Some(
                    json!({ "message_id": format!("n{n}"), "user_ids": [ann.id], "excerpt": "hi" }),
                ),
            )
            .await;
        assert_eq!(s, StatusCode::OK);
    }
    let done = mcp
        .ok("mark_mentions_read", json!({ "canvas": canvas }))
        .await;
    assert_eq!(done["updated"], 3);
    let (s, _) = app
        .rest(
            &bob.session,
            Method::POST,
            &format!("/v1/canvases/{canvas}/mentions"),
            Some(json!({ "message_id": "last", "user_ids": [ann.id], "excerpt": "hi" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let done = mcp.ok("mark_mentions_read", json!({})).await;
    assert_eq!(done, json!({ "ok": true, "updated": 1 }));
    assert_eq!(mcp.ok("list_mentions", json!({})).await["unread"], 0);

    // whoami: through a session too.
    let me = app.mcp(&ann.session).ok("whoami", json!({})).await;
    assert_eq!(me["via"], json!({ "kind": "session" }));
    let e = mcp.err("whoami", json!("not an object")).await;
    assert_eq!(e["error"], "bad_request");
    let e = mcp.err("list_canvases", json!([1, 2])).await;
    assert_eq!(e["error"], "bad_request");
}

#[tokio::test]
async fn read_only_tokens_see_and_call_only_the_read_tools() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let ro = app.pat(&ann, Scope::Read).await;
    let mut mcp = app.mcp(&ro);
    let tools = mcp.request("tools/list", json!({})).await;
    assert_eq!(
        names(&tools["result"]["tools"]),
        [
            "whoami",
            "list_canvases",
            "read_canvas",
            "list_members",
            "list_mentions"
        ]
    );
    for t in tools["result"]["tools"].as_array().unwrap() {
        assert_eq!(t["annotations"]["readOnlyHint"], true, "{t}");
    }
    // A POST is fine for a read-only token here: scope is enforced per tool.
    let me = mcp.ok("whoami", json!({})).await;
    assert_eq!(
        (me["read_only"].clone(), me["via"].clone()),
        (json!(true), json!({ "kind": "token", "scope": "read" }))
    );
    let read = mcp.ok("read_canvas", json!({ "canvas": "personal" })).await;
    assert_eq!(read["canvas"]["kind"], "personal");
    for (tool, args) in [
        ("create_canvas", json!({ "name": "x" })),
        (
            "apply_ops",
            json!({ "canvas": "personal", "ops": [{ "op": "add", "shape": { "type": "sticky" } }] }),
        ),
        ("mark_mentions_read", json!({})),
        ("create_share_link", json!({ "canvas": "personal" })),
    ] {
        let e = mcp.err(tool, args).await;
        assert_eq!(e["error"], "read_only_token", "{tool}");
        assert!(e["message"].as_str().unwrap().contains("read-only"), "{e}");
    }
    let read = mcp.ok("read_canvas", json!({ "canvas": "personal" })).await;
    assert_eq!(read["count"], 0, "nothing was written");
}

#[tokio::test]
async fn personal_resolves_for_every_credential_kind() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let pat = app.pat(&ann, Scope::Write).await;
    let (app_id, key) = app.app_key_for(&ann, "ann-subject").await;

    let first = app
        .mcp(&pat)
        .ok("read_canvas", json!({ "canvas": "personal" }))
        .await;
    assert_eq!(first["canvas"]["kind"], "personal");
    let id = first["canvas"]["id"].clone();
    let upper = app
        .mcp(&pat)
        .ok("read_canvas", json!({ "canvas": " PERSONAL " }))
        .await;
    assert_eq!(upper["canvas"]["id"], id);
    let listed = app.mcp(&ann.session).ok("list_canvases", json!({})).await;
    assert_eq!(listed["canvases"][0]["id"], id);

    // A linked app acting for its user (cva_ + X-Canvas-User).
    let mut as_app = app.mcp(&key);
    as_app
        .headers
        .push(("x-canvas-user".into(), "ann-subject".into()));
    let me = as_app.ok("whoami", json!({})).await;
    assert_eq!(me["user"]["id"], ann.id.to_string());
    assert_eq!(
        me["via"],
        json!({ "kind": "app", "app_id": app_id, "subject": "ann-subject" })
    );
    let r = as_app
        .ok("apply_ops", json!({ "canvas": "personal", "ops": [{ "op": "add", "shape": { "type": "text", "text": "from the app" } }] }))
        .await;
    assert_eq!(r["applied"], 1);
    let read = app
        .mcp(&pat)
        .ok("read_canvas", json!({ "canvas": "personal" }))
        .await;
    assert_eq!(read["canvas"]["id"], id);
    assert_eq!(read["shapes"][0]["text"], "from the app");
    // Without X-Canvas-User an app key is not a user.
    let (s, _, _) = app
        .post_mcp(
            Some(&key),
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
            &[],
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_human_in_the_room_sees_the_agents_edit_live() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let bob = app.user("bob").await;
    let pat = app.pat(&ann, Scope::Write).await;
    let mut agent = app.mcp(&pat);
    let canvas = agent.ok("create_canvas", json!({ "name": "Live" })).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let link = agent
        .ok("create_share_link", json!({ "canvas": canvas }))
        .await;
    let (s, _) = app
        .rest(
            &bob.session,
            Method::POST,
            &format!("/v1/canvas-links/{}/join", link["token"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);

    // Bob has the canvas open in a y-websocket client.
    let mut human = app.peer(&bob.session, &canvas).await;
    assert!(human.shape_json("note").is_none());
    let r = agent
        .ok(
            "apply_ops",
            json!({
                "canvas": canvas,
                "as": { "name": "Scribe" },
                "ops": [{ "op": "add", "shape": { "type": "sticky", "id": "note", "text": "Hello from MCP" } }],
            }),
        )
        .await;
    assert_eq!(r["applied"], 1);
    assert!(
        human
            .pump_until(Duration::from_secs(5), |p| p
                .shape_json("note")
                .is_some_and(|s| s.contains("Hello from MCP")))
            .await,
        "the human's client never saw the agent's sticky"
    );
    // And the agent's read reflects the human's view of the room (who wrote it, live agents).
    let read = agent.ok("read_canvas", json!({ "canvas": canvas })).await;
    assert_eq!(read["shapes"][0]["by"], "Scribe");
    assert!(
        read["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "Scribe"),
        "{read}"
    );
}

#[tokio::test]
async fn oversized_reads_come_back_as_a_summary() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let pat = app.pat(&ann, Scope::Write).await;
    let mut mcp = app.mcp(&pat);
    let text = "lorem ipsum ".repeat(1500); // 18 000 characters
    let ops: Vec<Value> = (0..16)
        .map(|i| json!({ "op": "add", "shape": { "type": "sticky", "id": format!("s{i}"), "text": text } }))
        .chain([json!({ "op": "add", "shape": { "type": "frame", "id": "f", "title": "Area", "x": -2000, "y": -2000 } })])
        .collect();
    let r = mcp
        .ok("apply_ops", json!({ "canvas": "personal", "ops": ops }))
        .await;
    assert_eq!(r["applied"], 17, "{r}");

    // Clipped text keeps the normal read small enough.
    let normal = mcp.ok("read_canvas", json!({ "canvas": "personal" })).await;
    assert_eq!(normal["shapes"].as_array().unwrap().len(), 17);
    assert!(normal.get("truncated").is_none());

    // full=true would be ~290 KB: summarized, with a way forward.
    let result = mcp
        .call("read_canvas", json!({ "canvas": "personal", "full": true }))
        .await;
    assert_eq!(result["isError"], false);
    let text_len = result["content"][0]["text"].as_str().unwrap().len();
    assert!(
        text_len < canvas_cloud_mcp::tools::MAX_READ_BYTES,
        "{text_len}"
    );
    let s = &result["structuredContent"];
    assert_eq!(s["truncated"], true);
    assert_eq!(s["count"], 17);
    assert_eq!(s["by_type"], json!({ "sticky": 16, "frame": 1 }));
    assert_eq!(s["frames"][0]["id"], "f");
    assert_eq!(s["frames"][0]["label"], "Area");
    assert_eq!(s["shapes"].as_array().unwrap().len(), 17);
    assert!(s["bytes"].as_u64().unwrap() > 200 * 1024);
    let hint = s["hint"].as_str().unwrap();
    assert!(hint.contains("ids") && hint.contains("types"), "{hint}");

    // Narrowing as the hint says gets the full text back.
    let one = mcp
        .ok(
            "read_canvas",
            json!({ "canvas": "personal", "full": true, "ids": ["s3"] }),
        )
        .await;
    assert_eq!(one["shapes"][0]["text"].as_str().unwrap().len(), text.len());
}

#[tokio::test]
async fn llms_txt_and_openapi_are_public_and_point_at_this_server() {
    let app = TestApp::with(|cfg| cfg.public_url = "https://canvas.example.test".into()).await;

    let r = app.http.get(app.url("/llms.txt")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "text/plain; charset=utf-8");
    let txt = r.text().await.unwrap();
    assert!(txt.starts_with("# Canvas Cloud"));
    assert!(txt.contains("https://canvas.example.test/mcp"), "{txt}");
    assert!(txt.contains("https://canvas.example.test/v1/openapi.json"));
    assert!(txt.contains("https://canvas.example.test/account"));
    assert!(txt.contains("wss://canvas.example.test/v1/canvases/"));
    for tool in ALL_TOOLS {
        assert!(txt.contains(tool), "{tool}");
    }

    let r = app
        .http
        .get(app.url("/v1/openapi.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/json"));
    let doc: Value = r.json().await.unwrap();
    assert!(doc["openapi"].as_str().unwrap().starts_with("3.1."));
    assert_eq!(
        doc["servers"],
        json!([{ "url": "https://canvas.example.test", "description": "this server" }])
    );
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));

    let r = app.http.get(app.url("/v1/info")).send().await.unwrap();
    let info: Value = r.json().await.unwrap();
    assert!(
        info["features"].as_array().unwrap().contains(&json!("mcp")),
        "{info}"
    );
}
