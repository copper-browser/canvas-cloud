//! The server writes the same Y.Doc as the canvas page for the 0.2.0 vocabulary (checklist
//! cards, legacy checklists, timelines, live links, comment threads), and reads it back
//! the same way.
//!
//! Each `tests/fixtures/ops_v2/<name>.json` was made by running the page's own
//! `applyOps` / `readCanvas` (Canvas App `src/ops.ts`) under Bun — see `gen.ts` there.
//! Here the same ops run through [`apply_json_ops`] on an empty document and the result
//! must match: what each step applied and its errors; a typed dump of the `shapes` and
//! `comments` roots (`Y.Map` / `Y.Text` / `Y.Array` apart from plain values, and every
//! number tagged with the lib0 encoding it gets); and the read.
//!
//! Generated ids (UUIDs) are compared by order of first appearance.

#![allow(
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::float_cmp
)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use canvas_cloud_engine::checklist::Person;
use canvas_cloud_engine::ops::{apply_json_ops, OpsCtx};
use canvas_cloud_engine::read::{read_canvas_txn, ReadOpts};
use canvas_cloud_engine::schema::json_to_any;
use serde_json::{json, Map as JsonMap, Value};
use yrs::types::ToJson as _;
use yrs::{Any, Array as _, Doc, GetString as _, Map as _, MapPrelim, Out, ReadTxn, Transact as _};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ops_v2")
}

fn load(name: &str) -> Value {
    let path = fixture_dir().join(format!("{name}.json"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap()
}

/// The tag lib0's `writeAny` (and yrs) gives a number.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn number(v: f64, int: Option<i64>) -> Value {
    if let Some(n) = int {
        if (-0x7FFF_FFFF..=0x7FFF_FFFF).contains(&n) {
            return json!({ "i": n });
        }
        return if f64::from(n as f32) == n as f64 {
            json!({ "f32": n })
        } else {
            json!({ "f64": n })
        };
    }
    if f64::from(v as f32) == v {
        json!({ "f32": v })
    } else {
        json!({ "f64": v })
    }
}

fn dump_any(a: &Any) -> Value {
    match a {
        Any::Null => Value::Null,
        Any::Undefined => json!({ "undefined": true }),
        Any::Bool(b) => Value::Bool(*b),
        Any::Number(n) => match n {
            yrs::any::Number::Int(i) => number(0.0, Some(*i)),
            yrs::any::Number::Float(f) => number(*f, None),
        },
        Any::String(s) => Value::String(s.to_string()),
        Any::Array(items) => Value::Array(items.iter().map(dump_any).collect()),
        Any::Map(m) => {
            let sorted: BTreeMap<&String, Value> =
                m.iter().map(|(k, v)| (k, dump_any(v))).collect();
            Value::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Any::Buffer(_) => json!({ "buffer": true }),
    }
}

fn dump_out<T: ReadTxn>(txn: &T, o: &Out) -> Value {
    match o {
        Out::Any(a) => dump_any(a),
        Out::YText(t) => json!({ "Y.Text": t.get_string(txn) }),
        Out::YMap(m) => {
            let mut keys: Vec<String> = m.keys(txn).map(str::to_owned).collect();
            keys.sort();
            let mut obj = JsonMap::new();
            for k in keys {
                if let Some(v) = m.get(txn, &k) {
                    obj.insert(k, dump_out(txn, &v));
                }
            }
            json!({ "Y.Map": obj })
        }
        Out::YArray(a) => {
            json!({ "Y.Array": a.iter(txn).map(|v| dump_out(txn, &v)).collect::<Vec<_>>() })
        }
        other => json!({ "other": other.to_json(txn).to_string() }),
    }
}

fn dump_root<T: ReadTxn>(txn: &T, name: &str) -> Value {
    match txn.get_map(name) {
        Some(m) => dump_out(txn, &Out::YMap(m)),
        None => json!({ "Y.Map": {} }),
    }
}

/// Replaces every UUID (in values and keys) with `uuid#N`, numbered in order of first
/// appearance (keys sorted at each level).
fn canon(v: &Value) -> Value {
    fn is_uuid(s: &str) -> bool {
        s.len() == 36 && uuid::Uuid::parse_str(s).is_ok()
    }
    fn name(s: &str, seen: &mut Vec<String>) -> String {
        if !is_uuid(s) {
            return s.to_owned();
        }
        let i = seen.iter().position(|x| x == s).unwrap_or_else(|| {
            seen.push(s.to_owned());
            seen.len() - 1
        });
        format!("uuid#{i}")
    }
    fn walk(v: &Value, seen: &mut Vec<String>) -> Value {
        match v {
            Value::String(s) => Value::String(name(s, seen)),
            Value::Array(a) => Value::Array(a.iter().map(|x| walk(x, seen)).collect()),
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                let mut out = JsonMap::new();
                for k in keys {
                    let nk = name(k, seen);
                    out.insert(nk, walk(&o[k], seen));
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
    walk(v, &mut Vec::new())
}

/// The first place two JSON values differ, as a path.
fn first_diff(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                let p = format!("{path}.{k}");
                match (x.get(k), y.get(k)) {
                    (Some(l), Some(r)) => {
                        if let Some(d) = first_diff(l, r, &p) {
                            return Some(d);
                        }
                    }
                    (l, r) => return Some(format!("{p}: server {l:?} ≠ page {r:?}")),
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) => {
            for (i, (l, r)) in x.iter().zip(y).enumerate() {
                if let Some(d) = first_diff(l, r, &format!("{path}[{i}]")) {
                    return Some(d);
                }
            }
            (x.len() != y.len())
                .then(|| format!("{path}: server has {} items, page {}", x.len(), y.len()))
        }
        _ if a == b => None,
        _ => Some(format!("{path}: server {a} ≠ page {b}")),
    }
}

fn assert_same(what: &str, server: &Value, page: &Value) {
    let (s, p) = (canon(server), canon(page));
    if let Some(d) = first_diff(&s, &p, what) {
        panic!(
            "{d}\n--- server\n{}\n--- page\n{}",
            serde_json::to_string_pretty(&s).unwrap(),
            serde_json::to_string_pretty(&p).unwrap()
        );
    }
}

fn check(name: &str) {
    let fx = load(name);
    let doc = Doc::new();
    let shapes = doc.get_or_insert_map("shapes");
    if let Some(seed) = fx["seed"].as_object() {
        let mut txn = doc.transact_mut();
        for (id, props) in seed {
            let m = shapes.insert(&mut txn, id.as_str(), MapPrelim::default());
            for (k, v) in props.as_object().unwrap() {
                m.insert(&mut txn, k.as_str(), json_to_any(v));
            }
        }
    }
    let mut ctx = OpsCtx::new(fx["by"].as_str().unwrap());
    fx["byId"].as_str().unwrap().clone_into(&mut ctx.by_id);
    ctx.now_ms = fx["now"].as_i64().unwrap();
    ctx.people = fx["people"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| Person {
            id: p["id"].as_str().unwrap().to_owned(),
            name: p["name"].as_str().unwrap().to_owned(),
            email: p["email"].as_str().map(str::to_owned),
        })
        .collect();
    for (i, step) in fx["steps"].as_array().unwrap().iter().enumerate() {
        let r = {
            let mut txn = doc.transact_mut();
            apply_json_ops(&mut txn, &shapes, step["ops"].as_array().unwrap(), &ctx)
        };
        let errors: Vec<Value> = r
            .errors
            .iter()
            .map(|e| json!({ "index": e.index, "error": e.error }))
            .collect();
        assert_same(
            &format!("{name} step {i}"),
            &json!({ "applied": r.applied, "ids": r.ids, "errors": errors }),
            &json!({ "applied": step["applied"], "ids": step["ids"], "errors": step["errors"] }),
        );
    }
    let txn = doc.transact();
    assert_same(
        &format!("{name} doc"),
        &json!({ "shapes": dump_root(&txn, "shapes"), "comments": dump_root(&txn, "comments") }),
        &fx["doc"],
    );
    let read = read_canvas_txn(
        &txn,
        &ReadOpts {
            now_ms: ctx.now_ms,
            ..ReadOpts::default()
        },
    );
    let read = serde_json::to_value(&read).unwrap();
    assert_same(
        &format!("{name} read"),
        &json!({ "shapes": read["shapes"], "comments": read["comments"] }),
        &fx["read"],
    );
}

#[test]
fn checklist_card() {
    check("checklist_card");
}

#[test]
fn checklist_legacy() {
    check("checklist_legacy");
}

#[test]
fn timeline() {
    check("timeline");
}

#[test]
fn web() {
    check("web");
}

#[test]
fn comments() {
    check("comments");
}
