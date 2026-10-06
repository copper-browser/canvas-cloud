//! The checklist (RSVP) shape over REST `/ops` + `/read` and the live room: the same
//! props, validation and limits as the canvas page (`Canvas/src/canvas/checklist.ts`),
//! and picks on different rows made at the same moment all stick.

#![allow(clippy::too_many_lines)]

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{json, Value};
use yrs::{Any, Map as _, Out, TransactionMut, WriteTxn as _};

const T: Duration = Duration::from_secs(5);

fn checklist(read: &Value, id: &str) -> Value {
    read["shapes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .cloned()
        .unwrap_or(Value::Null)
}

fn picks(c: &Value) -> Vec<Value> {
    c["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["pick"].clone())
        .collect()
}

#[tokio::test]
async fn ops_and_read_speak_checklist() {
    let app = TestApp::new().await;
    let ann = app.user("ann").await;
    let id = app.canvas(&ann, "Team dinners").await;
    let base = format!("/canvases/{id}");

    let (s, r) = app
        .post(
            &ann,
            &format!("{base}/ops"),
            json!({ "as": { "id": "agent:planner", "name": "Planner" }, "ops": [
                { "op": "add", "shape": { "id": "tue", "type": "checklist", "x": 0, "y": 0, "title": "Tue · Oct 7",
                    "rows": ["Ann", "Bob", { "label": "Cy", "id": "cy" }], "picks": { "Ann": "yes" } } },
                { "op": "add", "shape": { "id": "todo", "type": "checklist", "columns": ["Done"], "rows": ["Book a table"] } }
            ]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["applied"], 2, "{r}");

    let (s, read) = app.get(&ann, &format!("{base}/read")).await;
    assert_eq!(s, StatusCode::OK, "{read}");
    let tue = checklist(&read, "tue");
    assert_eq!(tue["type"], "checklist");
    assert_eq!(tue["title"], "Tue · Oct 7");
    assert_eq!(tue["columns"], json!(["Yes", "No"]));
    assert_eq!(tue["color"], "green");
    assert_eq!(tue["by"], "Planner");
    assert_eq!(picks(&tue), [json!("Yes"), Value::Null, Value::Null]);
    assert_eq!(tue["rows"][0]["by"], "Planner");
    assert!(tue["rows"][0]["at"].as_f64().unwrap() > 1.7e12);
    assert_eq!(
        tue["rows"][2],
        json!({ "id": "cy", "label": "Cy", "pick": null })
    );
    assert_eq!(tue["tally"], json!({ "Yes": 1, "No": 0 }));
    assert!(tue.get("picks").is_none(), "{tue}");
    let todo = checklist(&read, "todo");
    assert_eq!(
        (todo["w"].clone(), todo["h"].clone()),
        (json!(280), json!(97))
    );

    // The pick is stored on its own key, attributed to the agent.
    let doc = app.state_doc(&ann, &id).await;
    assert_eq!(
        common::shape_prop(
            &doc,
            "tue",
            &format!("pick:{}", tue["rows"][0]["id"].as_str().unwrap())
        )
        .unwrap()["byId"],
        "agent:planner"
    );

    // Update: picks by label or id (null clears), rows and columns replace the lists.
    let (_, r) = app
        .post(
            &ann,
            &format!("{base}/ops"),
            json!({ "ops": [
                { "op": "update", "id": "tue", "patch": { "picks": { "Ann": null, "cy": "No", "Bob": true } } },
                { "op": "update", "id": "todo", "patch": { "picks": { "book a table": "done" }, "title": "Before Friday" } }
            ]}),
        )
        .await;
    assert!(r["errors"].as_array().unwrap().is_empty(), "{r}");
    let (_, read) = app.get(&ann, &format!("{base}/read?types=checklist")).await;
    let tue = checklist(&read, "tue");
    assert_eq!(picks(&tue), [Value::Null, json!("Yes"), json!("No")]);
    assert_eq!(tue["rows"][1]["by"], "ann", "the caller, without `as`");
    let todo = checklist(&read, "todo");
    assert_eq!(todo["title"], "Before Friday");
    assert_eq!(todo["tally"], json!({ "Done": 1 }));

    let (_, r) = app
        .post(
            &ann,
            &format!("{base}/ops"),
            json!({ "ops": [
                { "op": "update", "id": "tue", "patch": { "rows": ["Cy", "Dee", "Bob"], "columns": ["Yes", "No", "Maybe"] } }
            ]}),
        )
        .await;
    assert!(r["errors"].as_array().unwrap().is_empty(), "{r}");
    let (_, read) = app.get(&ann, &format!("{base}/read")).await;
    let tue = checklist(&read, "tue");
    let labels: Vec<&str> = tue["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["Cy", "Dee", "Bob"]);
    assert_eq!(picks(&tue), [json!("No"), Value::Null, json!("Yes")]);
    assert_eq!(tue["tally"], json!({ "Yes": 1, "No": 1, "Maybe": 0 }));
    assert_eq!(tue["w"], 14 * 2 + 168 + 64 * 3);

    // Validation and limits, reported per op like the page.
    let too_many: Vec<String> = (0..61).map(|i| format!("Person {i}")).collect();
    let (_, r) = app
        .post(
            &ann,
            &format!("{base}/ops"),
            json!({ "ops": [
                { "op": "update", "id": "tue", "patch": { "picks": { "Zed": "Yes" } } },
                { "op": "update", "id": "tue", "patch": { "picks": { "Cy": "Later" } } },
                { "op": "add", "shape": { "type": "checklist", "rows": too_many } },
                { "op": "add", "shape": { "type": "checklist", "columns": ["Yes", "YES"] } },
                { "op": "add", "shape": { "type": "checklist", "columns": [] } },
                { "op": "add", "shape": { "type": "checklist", "rows": [{ "label": "x", "id": "no spaces" }] } },
                { "op": "add", "shape": { "type": "checklist", "url": "https://example.com" } },
                { "op": "add", "shape": { "type": "poll" } }
            ]}),
        )
        .await;
    let errors: Vec<&str> = r["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["error"].as_str().unwrap())
        .collect();
    assert_eq!(r["applied"], 0, "{r}");
    assert_eq!(errors[0], "`picks`: no row \"Zed\"");
    assert_eq!(errors[1], "`picks`: no column \"Later\" (Yes, No, Maybe)");
    assert_eq!(errors[2], "`rows`: at most 60 rows");
    assert_eq!(errors[3], "`columns` has \"YES\" twice");
    assert_eq!(errors[4], "`columns` must be a list of 1 to 4 names");
    assert_eq!(errors[5], "`rows`: an id must be 1–32 of A–Z a–z 0–9 _ -");
    assert!(
        errors[6].starts_with("unknown prop `url` for checklist"),
        "{}",
        errors[6]
    );
    assert_eq!(
        errors[7],
        "`shape.type` must be sticky, text, frame, arrow, image, link or checklist"
    );
    let (_, read) = app.get(&ann, &format!("{base}/read")).await;
    assert_eq!(read["count"], 2);
}

/// Every row's pick through REST at once, one request per person: all of them stick.
#[tokio::test]
async fn concurrent_rest_picks_on_different_rows_all_stick() {
    let app = TestApp::new().await;
    let owner = app.user("host").await;
    let id = app.canvas(&owner, "RSVP").await;
    let people = ["ann", "bob", "cy", "dee", "eve", "fay"];
    let rows: Vec<Value> = people
        .iter()
        .map(|p| json!({ "label": p, "id": p }))
        .collect();
    let (s, r) = app
        .post(
            &owner,
            &format!("/canvases/{id}/ops"),
            json!({ "ops": [{ "op": "add", "shape": { "id": "c", "type": "checklist", "rows": rows } }] }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let mut users = Vec::new();
    for p in people {
        let u = app.user(p).await;
        app.share(&owner, &id, &u).await;
        users.push(u);
    }
    let calls = users.iter().enumerate().map(|(i, u)| {
        let app = &app;
        let id = &id;
        async move {
            let col = if i % 2 == 0 { "Yes" } else { "No" };
            let body = json!({ "ops": [{ "op": "update", "id": "c", "patch": { "picks": { people[i]: col } } }] });
            app.post(u, &format!("/canvases/{id}/ops"), body).await
        }
    });
    for (s, r) in futures::future::join_all(calls).await {
        assert_eq!(s, StatusCode::OK, "{r}");
        assert_eq!(r["applied"], 1, "{r}");
    }
    let (_, read) = app.get(&owner, &format!("/canvases/{id}/read")).await;
    let c = checklist(&read, "c");
    assert_eq!(
        picks(&c),
        [
            json!("Yes"),
            json!("No"),
            json!("Yes"),
            json!("No"),
            json!("Yes"),
            json!("No")
        ]
    );
    assert_eq!(c["tally"], json!({ "Yes": 3, "No": 3 }));
    for (row, p) in c["rows"].as_array().unwrap().iter().zip(people) {
        assert_eq!(row["by"], p, "each pick is its picker's");
    }
}

fn pick(txn: &mut TransactionMut, shape: &str, row: &str, col: &str, by: &str) {
    let shapes = txn.get_or_insert_map("shapes");
    let Some(Out::YMap(m)) = shapes.get(txn, shape) else {
        panic!("no shape {shape}")
    };
    let value = Any::from(std::collections::HashMap::from([
        ("col".to_owned(), Any::from(col)),
        ("by".to_owned(), Any::from(by)),
        ("byId".to_owned(), Any::from(by)),
        ("at".to_owned(), Any::from(1_700_000_000_000_i64)),
    ]));
    m.insert(txn, format!("pick:{row}"), value);
}

/// Two editors in the live room click different rows at the same moment (each writes
/// before hearing the other): both picks stick, for both of them and on the server.
#[tokio::test]
async fn live_peers_picking_different_rows_at_once_both_stick() {
    let app = TestApp::new().await;
    let a = app.user("ada").await;
    let b = app.user("grace").await;
    let id = app.canvas(&a, "Live RSVP").await;
    app.share(&a, &id, &b).await;
    app.post(
        &a,
        &format!("/canvases/{id}/ops"),
        json!({ "ops": [{ "op": "add", "shape": { "id": "c", "type": "checklist",
            "rows": [{ "label": "Ada", "id": "ada" }, { "label": "Grace", "id": "grace" }] } }] }),
    )
    .await;
    let mut pa = app.peer(&a, &id).await;
    let mut pb = app.peer(&b, &id).await;
    assert!(pa.pump_until(T, |p| p.shape_count() == 1).await);
    assert!(pb.pump_until(T, |p| p.shape_count() == 1).await);
    // Neither has heard the other's click when it makes its own.
    let (ea, eb) = tokio::join!(
        pa.edit(|txn| pick(txn, "c", "ada", "Yes", "ada")),
        pb.edit(|txn| pick(txn, "c", "grace", "No", "grace")),
    );
    let ((), ()) = (ea, eb);
    let both = |p: &common::Peer| {
        p.shape_prop("c", "pick:ada")
            .is_some_and(|v| v["col"] == "Yes")
            && p.shape_prop("c", "pick:grace")
                .is_some_and(|v| v["col"] == "No")
    };
    assert!(pa.pump_until(T, both).await, "A sees both picks");
    assert!(pb.pump_until(T, both).await, "B sees both picks");
    let (_, read) = app.get(&a, &format!("/canvases/{id}/read")).await;
    let c = checklist(&read, "c");
    assert_eq!(picks(&c), [json!("Yes"), json!("No")], "{c}");
    assert_eq!(c["tally"], json!({ "Yes": 1, "No": 1 }));
    pa.close().await;
    pb.close().await;
}
