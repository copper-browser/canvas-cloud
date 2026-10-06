use super::*;
use crate::schema::out_to_json;
use serde_json::json;
use yrs::updates::decoder::Decode;
use yrs::{Doc, GetString, ReadTxn, Transact};

fn ctx() -> OpsCtx {
    OpsCtx {
        by: "tester".into(),
        by_id: "u-tester".into(),
        now_ms: 1_000,
        confirm_clear: false,
        origin: Point { x: 0.0, y: 0.0 },
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run(doc: &Doc, ops: Value) -> OpsResult {
    let shapes = doc.get_or_insert_map("shapes");
    let mut txn = doc.transact_mut();
    apply_json_ops(&mut txn, &shapes, ops.as_array().unwrap(), &ctx())
}

fn shape(doc: &Doc, id: &str) -> Value {
    let shapes = doc.get_or_insert_map("shapes");
    let txn = doc.transact();
    shapes
        .get(&txn, id)
        .map_or(Value::Null, |o| out_to_json(&txn, &o))
}

fn count(doc: &Doc) -> u32 {
    let shapes = doc.get_or_insert_map("shapes");
    let txn = doc.transact();
    shapes.len(&txn)
}

fn ok_ids(r: &OpsResult) -> Vec<String> {
    r.ids.iter().flatten().cloned().collect()
}

#[test]
fn parse_ops() {
    assert_eq!(
        Op::parse(&json!({"op":"move","id":"shape:a","dx":1})).unwrap(),
        Op::Move {
            id: "a".into(),
            dx: 1.0,
            dy: 0.0
        }
    );
    assert_eq!(
        Op::parse(&json!({"op":"clear"})).unwrap(),
        Op::Clear { confirm: false }
    );
    assert_eq!(
        Op::parse(&json!({"op":"update","id":"a","props":{"text":"x"}})).unwrap(),
        Op::Update {
            id: "a".into(),
            patch: json!({"text":"x"}).as_object().unwrap().clone()
        }
    );
    assert!(Op::parse(&json!({"op":"explode"}))
        .unwrap_err()
        .starts_with("unknown op \"explode\""));
    assert!(Op::parse(&json!("add")).is_err());
    assert!(Op::parse(&json!({"op":"move","id":"a","dx":"far"})).is_err());
    assert!(Op::parse(&json!({"op":"update","id":"a"})).is_err());
    assert!(Op::parse(&json!({"op":"delete"})).is_err());
}

#[test]
fn add_with_explicit_geometry_and_defaults() {
    let doc = Doc::new();
    let r = run(
        &doc,
        json!([{"op":"add","shape":{"id":"s1","type":"sticky","x":10.4,"y":20.6,"text":"hi","by":"spoof"}}]),
    );
    assert_eq!(r.applied, 1, "{r:?}");
    assert_eq!(r.ids, vec![Some("s1".to_owned())]);
    let s = shape(&doc, "s1");
    assert_eq!(s["id"], "s1");
    assert_eq!(s["type"], "sticky");
    assert_eq!(s["x"], 10, "boxes are rounded");
    assert_eq!(s["y"], 21);
    assert_eq!(s["w"], 200);
    assert_eq!(s["h"], 200);
    assert_eq!(s["text"], "hi");
    assert_eq!(s["color"], "yellow");
    assert_eq!(s["by"], "tester");
    assert_eq!(s["createdAt"], 1000);
    assert_eq!(s["updatedAt"], 1000);
    assert_eq!(s["z"], 1);
    // Sticky bodies are a Y.Text.
    let shapes = doc.get_or_insert_map("shapes");
    let txn = doc.transact();
    let Some(Out::YMap(m)) = shapes.get(&txn, "s1") else {
        panic!()
    };
    assert!(matches!(m.get(&txn, "text"), Some(Out::YText(_))));
    assert_eq!(r.anchor, Some(Point { x: 110.4, y: 120.6 }));
}

#[test]
fn add_places_in_free_space_with_uuid_ids() {
    let doc = Doc::new();
    let r = run(
        &doc,
        json!([
            {"op":"add","shape":{"type":"sticky"}},
            {"op":"add","shape":{"type":"sticky"}},
            {"op":"add","shape":{"type":"frame","title":"F"}},
            {"op":"add","shape":{"type":"text","text":"t"}}
        ]),
    );
    assert_eq!(r.applied, 4, "{r:?}");
    let ids = ok_ids(&r);
    for id in &ids {
        assert_eq!(uuid::Uuid::parse_str(id).unwrap().get_version_num(), 7);
    }
    let first = shape(&doc, &ids[0]);
    assert_eq!(
        (first["x"].clone(), first["y"].clone()),
        (json!(-100), json!(-100))
    );
    let boxes: Vec<BoxF> = ids
        .iter()
        .map(|id| {
            let s = shape(&doc, id);
            BoxF {
                x: s["x"].as_f64().unwrap(),
                y: s["y"].as_f64().unwrap(),
                w: s["w"].as_f64().unwrap(),
                h: s["h"].as_f64().unwrap(),
            }
        })
        .collect();
    for (i, a) in boxes.iter().enumerate() {
        for b in &boxes[i + 1..] {
            assert!(
                !crate::geometry::boxes_overlap(a, b, 0.0),
                "{a:?} overlaps {b:?}"
            );
        }
    }
    let z: Vec<f64> = ids
        .iter()
        .map(|id| shape(&doc, id)["z"].as_f64().unwrap())
        .collect();
    assert_eq!(z, vec![1.0, 2.0, 3.0, 4.0]);
    assert_eq!(shape(&doc, &ids[3])["h"], 36);
}

#[test]
fn add_validation_errors() {
    let doc = Doc::new();
    let r = run(
        &doc,
        json!([
            {"op":"add","shape":{"x":1}},
            {"op":"add","shape":{"type":"blob"}},
            {"op":"add","shape":{"type":"sticky","color":"red"}},
            {"op":"add","shape":{"type":"sticky","w":-5}},
            {"op":"add","shape":{"type":"link"}},
            {"op":"add","shape":{"type":"link","url":"javascript:alert(1)"}},
            {"op":"add","shape":{"type":"image"}},
            {"op":"add","shape":{"type":"image","src":"http://insecure/x.png"}},
            {"op":"add","shape":{"type":"arrow","from":{"x":0,"y":0}}},
            {"op":"add","shape":{"type":"arrow","from":{"x":0,"y":0},"to":{"x":5,"y":5},"x":3}},
            {"op":"add","shape":{"type":"sticky","url":"https://x.y"}},
            {"op":"add","shape":{"id":"bad id!","type":"text"}},
            {"op":"add","shape":{"id":"dup","type":"text"}},
            {"op":"add","shape":{"id":"dup","type":"text"}},
            {"op":"add","shape":{"type":"text","fontSize":2}},
            {"op":"nope"},
            42
        ]),
    );
    assert_eq!(r.applied, 1, "{r:?}");
    assert_eq!(r.ids.len(), 17);
    assert_eq!(r.ids[12], Some("dup".to_owned()));
    let idx: Vec<i64> = r.errors.iter().map(|e| e.index).collect();
    assert_eq!(
        idx,
        vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 14, 15, 16]
    );
    assert_eq!(r.errors[14].op, "nope");
    assert_eq!(r.errors[15].op, "");
    assert!(
        r.errors[9].error.contains("arrows have no box"),
        "{:?}",
        r.errors[9]
    );
    assert!(
        r.errors[10].error.contains("unknown prop `url` for sticky"),
        "{:?}",
        r.errors[10]
    );
    assert_eq!(count(&doc), 1);
}

#[test]
fn aliases_images_links_and_min_sizes() {
    let doc = Doc::new();
    let r = run(
        &doc,
        json!([
            {"op":"add","shape":{"id":"i","type":"image","src":"https://x/y.png","naturalW":960,"naturalH":480,"x":0,"y":0}},
            {"op":"add","shape":{"id":"l","type":"link","url":"www.example.com/a","text":"Ex","color":"#FF00aa","x":0,"y":400}},
            {"op":"add","shape":{"id":"f","type":"frame","text":"Group","x":0,"y":800,"image":{"src":"data:image/png;base64,AA","naturalW":1000,"naturalH":500}}},
            {"op":"add","shape":{"id":"tiny","type":"sticky","w":10,"h":10,"x":0,"y":-400}},
            {"op":"add","shape":{"id":"lk","type":"link","url":"https://www.rust-lang.org","x":800,"y":0}}
        ]),
    );
    assert_eq!(r.applied, 5, "{r:?}");
    let i = shape(&doc, "i");
    assert_eq!((i["w"].clone(), i["h"].clone()), (json!(480), json!(240)));
    let l = shape(&doc, "l");
    assert_eq!(l["url"], "https://www.example.com/a");
    assert_eq!(l["title"], "Ex", "text is a link's title");
    assert_eq!(l["color"], "#FF00aa");
    let f = shape(&doc, "f");
    assert_eq!(f["title"], "Group");
    assert_eq!((f["w"].clone(), f["h"].clone()), (json!(640), json!(320)));
    assert_eq!(f["image"]["naturalW"], 1000);
    let tiny = shape(&doc, "tiny");
    assert_eq!(
        (tiny["w"].clone(), tiny["h"].clone()),
        (json!(96), json!(64))
    );
    assert_eq!(shape(&doc, "lk")["title"], "rust-lang.org");
}

#[test]
fn update_merges_splices_text_and_ignores_server_keys() {
    let doc = Doc::new();
    run(
        &doc,
        json!([{"op":"add","shape":{"id":"a","type":"sticky","x":0,"y":0,"text":"hello world"}}]),
    );
    let r = run(
        &doc,
        json!([
            {"op":"update","id":"a","patch":{"text":"hello brave world","color":"pink","by":"spoof","type":"text"}},
            {"op":"update","id":"shape:a","props":{"w":50}},
            {"op":"update","id":"a","patch":{"x":null}},
            {"op":"update","id":"a","patch":{"label":"nope"}},
            {"op":"update","id":"missing","patch":{"text":"x"}}
        ]),
    );
    assert_eq!(r.applied, 2, "{r:?}");
    assert_eq!(r.ids[..2], [Some("a".to_owned()), Some("a".to_owned())]);
    assert_eq!(r.errors.len(), 3);
    let s = shape(&doc, "a");
    assert_eq!(s["text"], "hello brave world");
    assert_eq!(s["color"], "pink");
    assert_eq!(s["by"], "tester", "by is the creator");
    assert_eq!(s["type"], "sticky");
    assert_eq!(s["w"], 96, "clamped to the minimum");

    // A plain-string body (from an older writer) becomes a Y.Text on edit.
    let shapes = doc.get_or_insert_map("shapes");
    {
        let mut txn = doc.transact_mut();
        let Some(Out::YMap(m)) = shapes.get(&txn, "a") else {
            panic!()
        };
        m.insert(&mut txn, "text", "plain ünïcode");
    }
    run(
        &doc,
        json!([{"op":"update","id":"a","patch":{"text":"plain ünïcödé!"}}]),
    );
    let txn = doc.transact();
    let Some(Out::YMap(m)) = shapes.get(&txn, "a") else {
        panic!()
    };
    let Some(Out::YText(t)) = m.get(&txn, "text") else {
        panic!("text is a Y.Text")
    };
    assert_eq!(t.get_string(&txn), "plain ünïcödé!");
}

#[test]
fn text_splice_is_minimal() {
    assert_eq!(
        text_splice("hello world", "hello brave world"),
        (6, 0, "brave ")
    );
    assert_eq!(text_splice("abc", "abc"), (3, 0, ""));
    assert_eq!(text_splice("abcdef", "abef"), (2, 2, ""));
    assert_eq!(text_splice("", "x"), (0, 0, "x"));
    assert_eq!(text_splice("añb", "aöb"), (1, 2, "ö"));
    assert_eq!(text_splice("aaa", "aa"), (2, 1, ""));
}

#[test]
fn move_carries_frame_children_and_arrow_points() {
    let doc = Doc::new();
    run(
        &doc,
        json!([
            {"op":"add","shape":{"id":"f","type":"frame","x":0,"y":0,"w":500,"h":500}},
            {"op":"add","shape":{"id":"in","type":"sticky","x":50,"y":50,"w":100,"h":100}},
            {"op":"add","shape":{"id":"out","type":"sticky","x":800,"y":0,"w":100,"h":100}},
            {"op":"add","shape":{"id":"ar","type":"arrow","from":{"x":0,"y":0},"to":{"ref":"out"}}}
        ]),
    );
    let r = run(
        &doc,
        json!([
            {"op":"move","id":"f","dx":10,"dy":20},
            {"op":"move","id":"ar","dx":5,"dy":5},
            {"op":"move","id":"zzz","dx":1,"dy":1}
        ]),
    );
    assert_eq!(r.applied, 2, "{r:?}");
    assert_eq!(shape(&doc, "f")["x"], 10);
    assert_eq!(shape(&doc, "in")["y"], 70, "frame children move along");
    assert_eq!(shape(&doc, "out")["x"], 800, "others stay");
    let ar = shape(&doc, "ar");
    assert_eq!(ar["from"], json!({"x":5,"y":5}));
    assert_eq!(ar["to"], json!({"ref":"out"}));
    assert_eq!(ar["x"], 0, "arrows keep no box of their own");
}

#[test]
fn resize_rules() {
    let doc = Doc::new();
    run(
        &doc,
        json!([
            {"op":"add","shape":{"id":"a","type":"sticky","x":0,"y":0}},
            {"op":"add","shape":{"id":"b","type":"sticky","x":400,"y":0}},
            {"op":"connect","id":"c","from":"a","to":"b"}
        ]),
    );
    let r = run(
        &doc,
        json!([
            {"op":"resize","id":"a","w":260},
            {"op":"resize","id":"a","w":0,"h":100},
            {"op":"resize","id":"c","w":10,"h":10},
            {"op":"resize","id":"b","w":1,"h":1}
        ]),
    );
    assert_eq!(r.applied, 2, "{r:?}");
    let a = shape(&doc, "a");
    assert_eq!((a["w"].clone(), a["h"].clone()), (json!(260), json!(200)));
    let b = shape(&doc, "b");
    assert_eq!((b["w"].clone(), b["h"].clone()), (json!(96), json!(64)));
    assert!(r.errors[1].error.contains("arrow shapes have no size"));
}

#[test]
fn delete_cascades_to_arrows() {
    let doc = Doc::new();
    run(
        &doc,
        json!([
            {"op":"add","shape":{"id":"a","type":"sticky","x":0,"y":0}},
            {"op":"add","shape":{"id":"b","type":"sticky","x":400,"y":0}},
            {"op":"add","shape":{"id":"c","type":"sticky","x":800,"y":0}},
            {"op":"connect","id":"ab","from":"a","to":"b"},
            {"op":"connect","id":"bc","from":"b","to":"c","fromSide":"right","toSide":"left"},
            {"op":"connect","id":"ac","from":"a","to":"c"}
        ]),
    );
    assert_eq!(shape(&doc, "bc")["from"], json!({"ref":"b","side":"right"}));
    let r = run(
        &doc,
        json!([{"op":"delete","id":"b"},{"op":"delete","id":"b"}]),
    );
    assert_eq!(r.applied, 1, "{r:?}");
    assert_eq!(r.ids, vec![Some("b".to_owned()), None]);
    for gone in ["b", "ab", "bc"] {
        assert_eq!(shape(&doc, gone), Value::Null, "{gone}");
    }
    assert_eq!(shape(&doc, "ac")["type"], "arrow");
    assert_eq!(count(&doc), 3);
}

#[test]
fn connect_validation_and_by() {
    let doc = Doc::new();
    run(
        &doc,
        json!([
            {"op":"add","shape":{"id":"a","type":"sticky","x":0,"y":0}},
            {"op":"add","shape":{"id":"b","type":"sticky","x":400,"y":0}},
            {"op":"add","shape":{"id":"ar","type":"arrow","from":{"x":0,"y":0},"to":{"x":1,"y":1}}}
        ]),
    );
    let r = run(
        &doc,
        json!([
            {"op":"connect","from":"a","to":"a"},
            {"op":"connect","from":"a","to":"missing"},
            {"op":"connect","from":"a","to":"ar"},
            {"op":"connect","from":"a","to":"b","fromSide":"middle"},
            {"op":"connect","from":"a","to":"b","color":"blurple"},
            {"op":"connect","from":"shape:a","to":{"id":"b"},"label":"next","color":"blue"}
        ]),
    );
    assert_eq!(r.applied, 1, "{r:?}");
    assert_eq!(r.errors.len(), 5);
    let id = r.ids[5].clone().unwrap();
    let arrow = shape(&doc, &id);
    assert_eq!(arrow["from"], json!({"ref":"a"}));
    assert_eq!(arrow["to"], json!({"ref":"b"}));
    assert_eq!(arrow["label"], "next");
    assert_eq!(arrow["color"], "blue");
    assert_eq!(arrow["by"], "tester");
    // Anchor is the arrow's midpoint.
    assert_eq!(r.anchor, Some(Point { x: 300.0, y: 100.0 }));

    let r = run(
        &doc,
        json!([{"op":"update","id":id,"patch":{"to":"a"}}, {"op":"update","id":id,"patch":{"to":{"x":9,"y":9}}}]),
    );
    assert_eq!(r.applied, 1, "{r:?}");
    assert_eq!(shape(&doc, &id)["to"], json!({"x":9,"y":9}));
}

#[test]
fn clear_requires_confirmation() {
    let doc = Doc::new();
    run(
        &doc,
        json!([{"op":"add","shape":{"type":"sticky"}},{"op":"add","shape":{"type":"text"}}]),
    );
    let r = run(&doc, json!([{"op":"clear"}]));
    assert_eq!(r.applied, 0);
    assert_eq!(count(&doc), 2);
    let r = run(&doc, json!([{"op":"clear","confirm":true}]));
    assert_eq!(r.applied, 1);
    assert_eq!(r.ids, vec![None]);
    assert_eq!(count(&doc), 0);

    run(&doc, json!([{"op":"add","shape":{"type":"sticky"}}]));
    let shapes = doc.get_or_insert_map("shapes");
    let mut txn = doc.transact_mut();
    let mut c = ctx();
    c.confirm_clear = true;
    let r = apply_ops(&mut txn, &shapes, &[Op::Clear { confirm: false }], &c);
    assert_eq!(r.applied, 1);
    assert_eq!(shapes.len(&txn), 0);
}

#[test]
fn too_many_ops_is_a_request_error() {
    let doc = Doc::new();
    let ops: Vec<Value> = (0..=MAX_OPS).map(|_| json!({"op":"clear"})).collect();
    let r = run(&doc, Value::Array(ops));
    assert_eq!(r.applied, 0);
    assert_eq!(r.errors[0].index, -1);
    assert_eq!(r.ids.len(), 0, "no ids for an empty op list");
}

#[test]
fn ops_produce_a_transferable_update() {
    let doc = Doc::new();
    let shapes = doc.get_or_insert_map("shapes");
    let before = doc.transact().state_vector();
    {
        let mut txn = doc.transact_mut();
        let op = Op::parse(&json!({"op":"add","shape":{"id":"s","type":"sticky","text":"sync"}}))
            .unwrap();
        assert_eq!(apply_ops(&mut txn, &shapes, &[op], &ctx()).applied, 1);
    }
    let update = doc.transact().encode_state_as_update_v1(&before);
    let other = Doc::new();
    other
        .transact_mut()
        .apply_update(yrs::Update::decode_v1(&update).unwrap())
        .unwrap();
    assert_eq!(shape(&other, "s")["text"], "sync");
}

#[test]
fn actors_and_presence() {
    assert_eq!(Actor::parse(None).unwrap(), None);
    assert_eq!(
        Actor::parse(Some(&json!("Claude"))).unwrap().unwrap().id,
        "agent:Claude"
    );
    let a = Actor::parse(Some(&json!({"name":"  Planner  "})))
        .unwrap()
        .unwrap();
    assert_eq!(
        (a.id.as_str(), a.name.as_str()),
        ("agent:planner", "Planner")
    );
    let b = Actor::parse(Some(&json!({}))).unwrap().unwrap();
    assert_eq!(b.name, "Agent");
    assert!(Actor::parse(Some(&json!(3))).is_err());
    assert_eq!(color_for("agent:planner"), color_for("agent:planner"));
    assert!(color_for("x").starts_with("hsl("));

    let doc = Doc::new();
    let agents = doc.get_or_insert_map("agents");
    let mut txn = doc.transact_mut();
    write_agent(
        &mut txn,
        &agents,
        "ag",
        &a.writing(Some(Point { x: 1.4, y: 2.6 })),
        5,
    );
    let v = out_to_json(&txn, &agents.get(&txn, "ag").unwrap());
    assert_eq!(v["name"], "Planner");
    assert_eq!(v["status"], "writing");
    assert_eq!(v["cursor"], json!({"x":1,"y":3}));
    assert_eq!(v["updatedAt"], 5);
    // A partial patch keeps the rest.
    write_agent(
        &mut txn,
        &agents,
        "ag",
        &AgentPatch {
            status: Some("idle"),
            ..AgentPatch::default()
        },
        6,
    );
    let v = out_to_json(&txn, &agents.get(&txn, "ag").unwrap());
    assert_eq!(v["status"], "idle");
    assert_eq!(v["name"], "Planner");
    assert_eq!(v["cursor"], json!({"x":1,"y":3}));
}

#[test]
fn url_and_src_validation() {
    assert_eq!(
        valid_url(&json!("example.com")).unwrap(),
        "https://example.com/"
    );
    assert_eq!(valid_url(&json!("mailto:a@b.c")).unwrap(), "mailto:a@b.c");
    assert!(valid_url(&json!("ftp://x")).unwrap_err().contains("ftp:"));
    assert!(valid_url(&json!("  ")).is_err());
    assert!(valid_image_src(&json!("data:image/png;base64,AA")).is_ok());
    assert!(valid_image_src(&json!("data:text/html,<b>")).is_err());
    assert!(valid_image_src(&json!("https://a.b/c d.png")).is_err());
    let huge = format!("data:image/png;base64,{}", "A".repeat(MAX_DATA_URL));
    assert!(valid_image_src(&json!(huge))
        .unwrap_err()
        .contains("over 2 MB"));
}

// ---- checklist (the page's Canvas/src/__tests__/checklist-ops.test.ts) --------------------

fn read_one(doc: &Doc, id: &str) -> Value {
    let txn = doc.transact();
    let out = crate::read::read_canvas_txn(&txn, &crate::read::ReadOpts::default());
    let shape = out
        .shapes
        .into_iter()
        .find(|s| s.id == id)
        .expect("shape in the read");
    serde_json::to_value(shape).unwrap()
}

fn picks(v: &Value) -> Vec<Value> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["pick"].clone())
        .collect()
}

fn pick_keys(doc: &Doc, id: &str) -> usize {
    shape(doc, id)
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with("pick:"))
        .count()
}

#[test]
fn checklist_add_with_defaults_rows_picks_and_size() {
    let doc = Doc::new();
    let r = run(
        &doc,
        json!([{ "op": "add", "shape": { "type": "checklist", "id": "tue", "x": 0, "y": 0, "title": "Tue",
            "rows": ["Ann", "Bob", { "label": "Cy", "id": "cy" }], "picks": { "Ann": "yes", "cy": "No" } } }]),
    );
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let s = shape(&doc, "tue");
    assert_eq!(s["type"], "checklist");
    assert_eq!(s["title"], "Tue");
    assert_eq!(s["columns"], json!(["Yes", "No"]));
    assert_eq!(s["color"], "green");
    assert_eq!(s["w"], 324);
    assert_eq!(s["h"], 49 + 28 + 34 * 3 + 14);
    assert_eq!(s["by"], "tester");
    assert_eq!(s["rows"][2], json!({ "id": "cy", "label": "Cy" }));
    assert_eq!(
        s["pick:cy"],
        json!({ "col": "No", "by": "tester", "byId": "u-tester", "at": 1000 })
    );
    let read = read_one(&doc, "tue");
    assert_eq!(picks(&read), [json!("Yes"), Value::Null, json!("No")]);
    assert_eq!(read["tally"], json!({ "Yes": 1, "No": 1 }));
    assert_eq!(read["rows"][0]["by"], "tester");
    assert_eq!(read["rows"][0]["at"], 1000);
    assert_eq!(
        read["rows"][1],
        json!({ "id": read["rows"][1]["id"], "label": "Bob", "pick": null })
    );
    assert!(read.get("picks").is_none());
}

#[test]
fn checklist_one_column_is_a_todo_list() {
    let doc = Doc::new();
    run(
        &doc,
        json!([{ "op": "add", "shape": { "type": "checklist", "id": "todo", "columns": ["Done"],
            "rows": ["Book table"], "picks": { "book table": true } } }]),
    );
    let s = shape(&doc, "todo");
    assert_eq!(s["w"], 280);
    assert_eq!(s["h"], 49 + 34 + 14);
    let read = read_one(&doc, "todo");
    assert_eq!(read["rows"][0]["pick"], "Done");
    assert_eq!(read["tally"], json!({ "Done": 1 }));
}

#[test]
fn checklist_bad_input_fails_the_whole_add() {
    let doc = Doc::new();
    let r = run(
        &doc,
        json!([
            { "op": "add", "shape": { "type": "checklist", "rows": ["Ann"], "picks": { "Ann": "Maybe" } } },
            { "op": "add", "shape": { "type": "checklist", "columns": ["A", "B", "C", "D", "E"] } },
            { "op": "add", "shape": { "type": "checklist", "rows": "Ann" } },
            { "op": "add", "shape": { "type": "checklist", "rows": ["Ann"], "fontSize": 20 } }
        ]),
    );
    assert_eq!(r.applied, 0);
    let at: Vec<i64> = r.errors.iter().map(|e| e.index).collect();
    assert_eq!(at, [0, 1, 2, 3]);
    assert_eq!(r.errors[0].error, "`picks`: no column \"Maybe\" (Yes, No)");
    assert!(
        r.errors[3].error.contains("unknown prop `fontSize`"),
        "{}",
        r.errors[3].error
    );
    assert_eq!(count(&doc), 0);
}

#[test]
fn checklist_update_picks_one_row_at_a_time() {
    let doc = Doc::new();
    run(
        &doc,
        json!([{ "op": "add", "shape": { "type": "checklist", "id": "c", "rows": ["Ann", "Bob", "Cy"],
            "picks": { "Ann": "Yes", "Bob": "No" } } }]),
    );
    let r = run(
        &doc,
        json!([{ "op": "update", "id": "c", "patch": { "picks": { "Ann": null, "Cy": "yes" } } }]),
    );
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let read = read_one(&doc, "c");
    assert_eq!(picks(&read), [Value::Null, json!("No"), json!("Yes")]);
    assert_eq!(read["tally"], json!({ "Yes": 1, "No": 1 }));
    assert_eq!(pick_keys(&doc, "c"), 2);
}

#[test]
fn checklist_rows_keep_their_picks_and_columns_drop_theirs() {
    let doc = Doc::new();
    run(
        &doc,
        json!([{ "op": "add", "shape": { "type": "checklist", "id": "c", "rows": ["Ann", "Bob"],
            "picks": { "Ann": "Yes", "Bob": "No" } } }]),
    );
    let ann = shape(&doc, "c")["rows"][0]["id"].clone();
    run(
        &doc,
        json!([{ "op": "update", "id": "c", "patch": { "rows": ["Bob", "Dee", "Ann"],
            "columns": ["Yes", "No", "Maybe"], "picks": { "Dee": "maybe" } } }]),
    );
    let s = shape(&doc, "c");
    assert_eq!(s["rows"][2]["id"], ann);
    assert_eq!(s["columns"], json!(["Yes", "No", "Maybe"]));
    assert_eq!(s["w"], 14 * 2 + 168 + 64 * 3);
    assert_eq!(s["h"], 49 + 28 + 34 * 3 + 14);
    assert_eq!(
        picks(&read_one(&doc, "c")),
        [json!("No"), json!("Maybe"), json!("Yes")]
    );
    run(
        &doc,
        json!([{ "op": "update", "id": "c", "patch": { "columns": ["Yes", "Maybe"], "rows": ["Bob", "Dee"] } }]),
    );
    assert_eq!(picks(&read_one(&doc, "c")), [Value::Null, json!("Maybe")]);
    assert_eq!(pick_keys(&doc, "c"), 1);
    // A column renamed only in case keeps its picks, under the new spelling.
    run(
        &doc,
        json!([{ "op": "update", "id": "c", "patch": { "columns": ["Yes", "MAYBE"] } }]),
    );
    assert_eq!(picks(&read_one(&doc, "c")), [Value::Null, json!("MAYBE")]);
}

#[test]
fn checklist_update_errors_change_nothing() {
    let doc = Doc::new();
    run(
        &doc,
        json!([{ "op": "add", "shape": { "type": "checklist", "id": "c", "title": "T", "rows": ["Ann"] } }]),
    );
    let r = run(
        &doc,
        json!([{ "op": "update", "id": "c", "patch": { "title": "New", "picks": { "Zed": "Yes" } } }]),
    );
    assert_eq!(r.errors[0].error, "`picks`: no row \"Zed\"");
    assert_eq!(shape(&doc, "c")["title"], "T");
}

#[test]
fn checklist_moves_resizes_with_a_floor_and_takes_text_as_title() {
    let doc = Doc::new();
    run(
        &doc,
        json!([
            { "op": "add", "shape": { "type": "checklist", "id": "c", "x": 0, "y": 0, "rows": ["Ann"] } },
            { "op": "move", "id": "c", "dx": 10, "dy": 20 },
            { "op": "resize", "id": "c", "w": 50, "h": 50 },
            { "op": "add", "shape": { "type": "checklist", "id": "d", "text": "Wed dinner" } }
        ]),
    );
    let s = shape(&doc, "c");
    assert_eq!(
        (
            s["x"].clone(),
            s["y"].clone(),
            s["w"].clone(),
            s["h"].clone()
        ),
        (json!(10), json!(20), json!(200), json!(72))
    );
    assert_eq!(shape(&doc, "d")["title"], "Wed dinner");
    assert_eq!(read_one(&doc, "d")["columns"], json!(["Yes", "No"]));
}

#[test]
fn checklist_concurrent_picks_on_different_rows_both_stick() {
    let a = Doc::with_client_id(1);
    run(
        &a,
        json!([{ "op": "add", "shape": { "type": "checklist", "id": "c", "rows": [{"label": "Ann", "id": "ann"}, {"label": "Bob", "id": "bob"}] } }]),
    );
    let b = Doc::with_client_id(2);
    let full = a
        .transact()
        .encode_state_as_update_v1(&yrs::StateVector::default());
    b.transact_mut()
        .apply_update(yrs::Update::decode_v1(&full).unwrap())
        .unwrap();
    // Both write before either hears from the other.
    run(
        &a,
        json!([{ "op": "update", "id": "c", "patch": { "picks": { "ann": "Yes" } } }]),
    );
    run(
        &b,
        json!([{ "op": "update", "id": "c", "patch": { "picks": { "bob": "No" } } }]),
    );
    let ua = a
        .transact()
        .encode_state_as_update_v1(&b.transact().state_vector());
    let ub = b
        .transact()
        .encode_state_as_update_v1(&a.transact().state_vector());
    a.transact_mut()
        .apply_update(yrs::Update::decode_v1(&ub).unwrap())
        .unwrap();
    b.transact_mut()
        .apply_update(yrs::Update::decode_v1(&ua).unwrap())
        .unwrap();
    for d in [&a, &b] {
        assert_eq!(picks(&read_one(d, "c")), [json!("Yes"), json!("No")]);
    }
}
