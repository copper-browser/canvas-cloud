//! Cross-implementation check against the real canvas page code (Yjs + y-websocket +
//! `Canvas/src/ops.ts`), run with Bun. Ignored by default; run with
//!
//! ```sh
//! COPPER_CANVAS_DIR=~/src/copper-canvasweb/Canvas \
//!   cargo test -p copper-cloud-canvas --test js_interop -- --ignored
//! ```
//!
//! It proves: a stock `y-websocket` `WebsocketProvider` syncs through the room both ways,
//! awareness relays between JS peers, ops applied by the page reach the server and vice
//! versa, and the server's `/read` equals the page's `readCanvas` on the same document —
//! checklists included (the page must have `src/canvas/checklist.ts`).

#![allow(clippy::too_many_lines)]

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, BufReader};

const SCRIPT: &str = r"
import * as Y from 'yjs'
import { WebsocketProvider } from 'y-websocket'
const CANVAS = process.env.CANVAS_DIR
const { CanvasStore } = await import(`${CANVAS}/src/canvas/doc.ts`)
const { applyOps, readCanvas } = await import(`${CANVAS}/src/ops.ts`)
const { WS_BASE, CANVAS_ID, TOKEN_A, TOKEN_B, KEY } = process.env

class WS extends WebSocket {
  constructor(url, _protocols) {
    super(url, { headers: { 'x-copper-instance': KEY } })
  }
}
function connect(token) {
  const doc = new Y.Doc()
  const provider = new WebsocketProvider(WS_BASE, `${CANVAS_ID}/ws`, doc, {
    WebSocketPolyfill: WS,
    params: { token },
    disableBc: true,
  })
  return { doc, provider, store: new CanvasStore(doc) }
}
const waitFor = (what, pred, ms = 8000) =>
  new Promise((resolve, reject) => {
    const t0 = Date.now()
    const tick = () => {
      if (pred()) resolve(true)
      else if (Date.now() - t0 > ms) reject(new Error(`timeout: ${what}`))
      else setTimeout(tick, 20)
    }
    tick()
  })
const out = (k, v) => console.log(JSON.stringify({ k, v }))
const read = s =>
  readCanvas({ store: s.store, canvas: { id: CANVAS_ID, name: 'Interop', kind: 'shared' }, viewport: null, selection: [] })

try {
  const a = connect(TOKEN_A)
  const b = connect(TOKEN_B)
  await waitFor('sync', () => a.provider.synced && b.provider.synced)
  out('read_initial', read(a))
  const r = applyOps(
    { store: a.store, viewportCenter: () => ({ x: 0, y: 0 }) },
    {
      ops: [
        { op: 'add', shape: { id: 'js1', type: 'sticky', text: 'from js', x: 0, y: 600 } },
        { op: 'update', id: 'srv1', patch: { text: 'edited by js' } },
        { op: 'connect', id: 'jsarrow', from: 'js1', to: 'srv1', label: 'js arrow' },
        { op: 'add', shape: { id: 'jsframe', type: 'frame', title: 'Group', x: -100, y: 500, w: 600, h: 400 } },
        { op: 'update', id: 'srvlist', patch: { picks: { Bob: 'No' }, rows: ['Ann', 'Bob', 'Cy', 'Dee'], columns: ['Yes', 'No', 'Maybe'] } },
        { op: 'add', shape: { id: 'jslist', type: 'checklist', columns: ['Done'], rows: ['Book a table'], x: 1200, y: 600 } },
      ],
      as: { name: 'JS' },
    }
  )
  out('apply', r)
  await waitFor('b sees js edits', () => b.store.get('js1') && b.store.get('srv1')?.text === 'edited by js')
  a.provider.awareness.setLocalStateField('user', { name: 'Ann JS', color: '#123456' })
  await waitFor('awareness', () => [...b.provider.awareness.getStates().values()].some(s => s.user?.name === 'Ann JS'))
  out('awareness', b.provider.awareness.getStates().size)
  out('ready', true)
  await waitFor(
    'server ops',
    () =>
      a.store.get('srv2') !== undefined &&
      b.store.get('srv1')?.text === 'edited by js, then server' &&
      Object.keys(a.store.get('jslist')?.picks ?? {}).length === 1 &&
      Object.keys(a.store.get('srvlist')?.picks ?? {}).length === 3,
    10000
  )
  await new Promise(r => setTimeout(r, 200))
  out('read_final', read(a))
  a.provider.destroy()
  b.provider.destroy()
  process.exit(0)
} catch (e) {
  out('error', String(e))
  process.exit(1)
}
";

fn shapes_of(read: &Value) -> Vec<Value> {
    read["shapes"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
#[ignore = "needs bun and COPPER_CANVAS_DIR (the canvas page sources with node_modules)"]
async fn real_y_websocket_and_page_ops_interoperate() {
    let Some(canvas_dir) = std::env::var_os("COPPER_CANVAS_DIR").map(PathBuf::from) else {
        eprintln!("COPPER_CANVAS_DIR not set; skipping");
        return;
    };
    let canvas_dir = canvas_dir.canonicalize().unwrap();
    let work = std::env::temp_dir().join(format!("copper-canvas-interop-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&work).unwrap();
    std::os::unix::fs::symlink(canvas_dir.join("node_modules"), work.join("node_modules")).unwrap();
    std::fs::write(work.join("interop.mjs"), SCRIPT).unwrap();

    let app = TestApp::new().await;
    let a = app.user("ann").await;
    let b = app.user("ben").await;
    let id = app.canvas(&a, "Interop").await;
    app.share(&a, &id, &b).await;
    let ops = |body: Value| {
        let app = &app;
        let a = &a;
        let id = &id;
        async move {
            let (s, r) = app.post(a, &format!("/canvases/{id}/ops"), body).await;
            assert_eq!(s, StatusCode::OK, "{r}");
            assert!(r["errors"].as_array().unwrap().is_empty(), "{r}");
            r
        }
    };
    ops(json!({ "as": { "name": "Server agent" }, "ops": [
        { "op": "add", "shape": { "id": "srv1", "type": "sticky", "text": "from server", "x": 400, "y": 0 } },
        { "op": "add", "shape": { "id": "srvlink", "type": "link", "url": "example.com/path", "x": 800, "y": 0 } },
        { "op": "connect", "id": "srvarrow", "from": "srv1", "to": "srvlink", "fromSide": "right" },
        { "op": "add", "shape": { "id": "srvlist", "type": "checklist", "title": "Dinner", "rows": ["Ann", "Bob", "Cy"],
            "picks": { "Ann": "Yes" }, "x": 0, "y": -600 } }
    ]}))
    .await;

    let mut child = tokio::process::Command::new("bun")
        .arg("run")
        .arg(work.join("interop.mjs"))
        .current_dir(&work)
        .env("CANVAS_DIR", &canvas_dir)
        .env("WS_BASE", format!("ws://{}/v1/canvases", app.addr))
        .env("CANVAS_ID", &id)
        .env("TOKEN_A", &a.token)
        .env("TOKEN_B", &b.token)
        .env("KEY", &app.instance_key)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("bun on PATH");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut got: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while let Ok(Ok(Some(line))) = tokio::time::timeout_at(deadline, lines.next_line()).await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            eprintln!("bun: {line}");
            continue;
        };
        let k = msg["k"].as_str().unwrap_or_default().to_owned();
        assert_ne!(k, "error", "{}", msg["v"]);
        if k == "ready" {
            ops(json!({ "ops": [
                { "op": "add", "shape": { "id": "srv2", "type": "text", "text": "late", "x": 0, "y": -300 } },
                { "op": "update", "id": "srv1", "patch": { "text": "edited by js, then server" } },
                { "op": "update", "id": "jslist", "patch": { "picks": { "book a table": true } } },
                { "op": "update", "id": "srvlist", "patch": { "picks": { "Dee": "maybe" } } }
            ]}))
            .await;
        }
        got.insert(k, msg["v"].clone());
        if got.contains_key("read_final") {
            break;
        }
    }
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let _ = std::fs::remove_dir_all(&work);
    assert!(
        matches!(status, Ok(Ok(s)) if s.success()),
        "bun exited badly: {status:?}"
    );

    // The page saw the server-made shapes exactly as the server reads them.
    let initial = &got["read_initial"];
    assert_eq!(shapes_of(initial).len(), 4, "{initial}");
    assert_eq!(got["apply"]["applied"], 6, "{}", got["apply"]);
    assert!(got["awareness"].as_u64().unwrap() >= 2);

    // Server and page agree on the final document, shape for shape.
    let (s, server) = app.get(&a, &format!("/canvases/{id}/read?full=true")).await;
    assert_eq!(s, StatusCode::OK);
    let page = &got["read_final"];
    assert_eq!(
        server["count"], page["count"],
        "server {server}\npage {page}"
    );
    let (ss, ps) = (shapes_of(&server), shapes_of(page));
    assert_eq!(ss.len(), ps.len());
    for (sv, pv) in ss.iter().zip(&ps) {
        assert_eq!(sv, pv, "shape differs between server and page");
    }
    let srv1 = ss.iter().find(|s| s["id"] == "srv1").unwrap();
    assert_eq!(srv1["text"], "edited by js, then server");
    let js1 = ss.iter().find(|s| s["id"] == "js1").unwrap();
    assert_eq!(js1["by"], "JS");
    assert_eq!(js1["frame"], "jsframe");
    // Checklists: picks made on both sides, read the same by both.
    let srvlist = ss.iter().find(|s| s["id"] == "srvlist").unwrap();
    let picked: Vec<&Value> = srvlist["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["pick"])
        .collect();
    assert_eq!(
        picked,
        [&json!("Yes"), &json!("No"), &Value::Null, &json!("Maybe")]
    );
    assert_eq!(srvlist["tally"], json!({ "Yes": 1, "No": 1, "Maybe": 1 }));
    let jslist = ss.iter().find(|s| s["id"] == "jslist").unwrap();
    assert_eq!(jslist["rows"][0]["pick"], "Done");
    assert_eq!(jslist["rows"][0]["by"], "ann");
}
