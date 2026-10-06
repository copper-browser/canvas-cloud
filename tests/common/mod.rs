//! Shared integration-test harness: real Postgres (`COPPER_CLOUD_TEST_CANVAS_DATABASE_URL`,
//! default `postgres://localhost:5432/copper_cloud_test_canvas`), the real core app + canvas
//! router on an ephemeral port, users/sessions inserted directly, and a minimal y-websocket
//! client built on `yrs` + tokio-tungstenite.

#![allow(dead_code, clippy::missing_panics_doc)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine as _;
use copper_cloud_core::config::Config;
use copper_cloud_core::crypto::Crypto;
use copper_cloud_core::ids;
use copper_cloud_core::state::{AppState, SharedState};
use futures::{SinkExt as _, StreamExt as _};
use http_body_util::BodyExt as _;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower::ServiceExt as _;
use uuid::Uuid;
use yrs::encoding::read::Cursor;
use yrs::sync::awareness::AwarenessUpdateEntry;
use yrs::sync::{AwarenessUpdate, Message, MessageReader, SyncMessage};
use yrs::updates::decoder::{Decode as _, DecoderV1};
use yrs::updates::encoder::Encode as _;
use yrs::{
    ClientID, Doc, Map as _, Out, ReadTxn as _, StateVector, Transact as _, TransactionMut, Update,
};

pub const DATABASE_URL: &str = "postgres://localhost:5432/copper_cloud_test_canvas";

/// The test database: `COPPER_CLOUD_TEST_CANVAS_DATABASE_URL`, else [`DATABASE_URL`] — so
/// parallel checkouts can each test against their own database.
pub fn database_url() -> String {
    std::env::var("COPPER_CLOUD_TEST_CANVAS_DATABASE_URL")
        .unwrap_or_else(|_| DATABASE_URL.to_owned())
}

static MIGRATED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn migrate(pool: &PgPool) {
    MIGRATED
        .get_or_init(|| async {
            let migrator = sqlx::migrate::Migrator::new(Path::new("../copper-cloud/migrations"))
                .await
                .expect("reading migrations");
            if let Err(e) = migrator.run(pool).await {
                // A migration changed during development: start from a clean schema.
                eprintln!("migrations failed ({e}); resetting the test schema");
                sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
                    .execute(pool)
                    .await
                    .expect("resetting schema");
                migrator.run(pool).await.expect("running migrations");
            }
        })
        .await;
}

/// One running server.
pub struct TestApp {
    pub state: SharedState,
    pub router: axum::Router,
    pub addr: SocketAddr,
    pub instance_key: String,
}

/// A signed-in user.
#[derive(Clone, Debug)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub token: String,
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with_config(Config::for_tests(&database_url())).await
    }

    /// A second server over the same database and config (same master key): what a restart
    /// of this one sees. Rooms are process-wide, so evict them first for a cold start.
    pub async fn restart(&self) -> Self {
        Self::with_config(Config::clone(&self.state.cfg)).await
    }

    async fn with_config(cfg: Config) -> Self {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(30))
            .connect(&cfg.database_url)
            .await
            .expect("connecting to the canvas test database (createdb copper_cloud_test_canvas)");
        migrate(&pool).await;
        let instance_key = cfg.instance_key.clone();
        let state = AppState::new(pool, cfg);
        let router = copper_cloud_core::app_with(
            state.clone(),
            copper_cloud_canvas::router(),
            copper_cloud_canvas::landing_router(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve = router
            .clone()
            .into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, serve).await;
        });
        Self {
            state,
            router,
            addr,
            instance_key,
        }
    }

    pub fn db(&self) -> &PgPool {
        &self.state.db
    }

    /// Inserts a user + device + session directly (core schema) and returns its bearer token.
    pub async fn user(&self, name: &str) -> User {
        let id = ids::uuid_v7();
        let email = format!("{name}-{}@example.test", &id.simple().to_string()[20..]);
        let wrapped = self.state.crypto.wrap_key(&Crypto::new_key());
        sqlx::query(
            "INSERT INTO users (id, email, display_name, password_hash, data_key_wrapped)
             VALUES ($1, $2, $3, 'not-a-real-hash', $4)",
        )
        .bind(id)
        .bind(&email)
        .bind(name)
        .bind(&wrapped)
        .execute(self.db())
        .await
        .unwrap();
        let device = ids::uuid_v7();
        sqlx::query("INSERT INTO devices (user_id, id, name) VALUES ($1, $2, 'test')")
            .bind(id)
            .bind(device)
            .execute(self.db())
            .await
            .unwrap();
        let token = ids::random_token();
        sqlx::query(
            "INSERT INTO sessions (id, user_id, device_id, token_sha256, expires_at)
             VALUES ($1, $2, $3, $4, now() + interval '1 day')",
        )
        .bind(ids::uuid_v7())
        .bind(id)
        .bind(device)
        .bind(&ids::sha256(token.as_bytes())[..])
        .execute(self.db())
        .await
        .unwrap();
        User { id, email, token }
    }

    /// One REST call through the full app (instance gate included).
    pub async fn call(
        &self,
        user: Option<&User>,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("/v1{path}"))
            .header("x-copper-instance", &self.instance_key);
        if let Some(u) = user {
            req = req.header("authorization", format!("Bearer {}", u.token));
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        (status, value)
    }

    pub async fn get(&self, user: &User, path: &str) -> (StatusCode, Value) {
        self.call(Some(user), Method::GET, path, None).await
    }

    pub async fn post(&self, user: &User, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Some(user), Method::POST, path, Some(body)).await
    }

    pub async fn patch(&self, user: &User, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Some(user), Method::PATCH, path, Some(body)).await
    }

    pub async fn delete(&self, user: &User, path: &str) -> (StatusCode, Value) {
        self.call(Some(user), Method::DELETE, path, None).await
    }

    /// Creates a shared canvas owned by `user`; returns its id.
    pub async fn canvas(&self, user: &User, name: &str) -> String {
        let (s, v) = self
            .post(user, "/canvases", serde_json::json!({ "name": name }))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v["id"].as_str().unwrap().to_owned()
    }

    /// Makes `guest` an editor of `canvas` via invite + accept.
    pub async fn share(&self, owner: &User, canvas: &str, guest: &User) {
        let (s, inv) = self
            .post(
                owner,
                &format!("/canvases/{canvas}/invites"),
                serde_json::json!({ "email": guest.email }),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{inv}");
        let id = inv["id"].as_str().unwrap();
        let (s, v) = self
            .post(guest, &format!("/invites/{id}/accept"), Value::Null)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    /// The canvas document as served by `GET /state`, decoded into a fresh doc.
    pub async fn state_doc(&self, user: &User, canvas: &str) -> Doc {
        let (s, v) = self.get(user, &format!("/canvases/{canvas}/state")).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(v["state"].as_str().unwrap())
            .unwrap();
        let doc = Doc::new();
        doc.transact_mut()
            .apply_update(Update::decode_v1(&bytes).unwrap())
            .unwrap();
        doc
    }

    pub fn ws_url(&self, canvas: &str, token: &str) -> String {
        format!("ws://{}/v1/canvases/{canvas}/ws?token={token}", self.addr)
    }

    /// Opens a raw WebSocket (the handshake result is returned as-is).
    pub async fn ws_raw(
        &self,
        canvas: &str,
        token: &str,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, tokio_tungstenite::tungstenite::Error>
    {
        let mut req = self.ws_url(canvas, token).into_client_request().unwrap();
        req.headers_mut()
            .insert("x-copper-instance", self.instance_key.parse().unwrap());
        tokio_tungstenite::connect_async(req)
            .await
            .map(|(ws, _)| ws)
    }

    /// Connects a y-websocket peer and completes the initial sync.
    pub async fn peer(&self, user: &User, canvas: &str) -> Peer {
        let ws = self.ws_raw(canvas, &user.token).await.expect("ws connect");
        let mut p = Peer {
            doc: Doc::new(),
            ws,
            awareness: HashMap::new(),
            closed: None,
            synced: false,
        };
        let sv = p.doc.transact().state_vector();
        p.send(&Message::Sync(SyncMessage::SyncStep1(sv))).await;
        assert!(
            p.pump_until(Duration::from_secs(5), |p| p.synced).await,
            "initial sync"
        );
        p
    }
}

/// A minimal y-websocket client.
pub struct Peer {
    pub doc: Doc,
    pub ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// Latest awareness JSON per client id (`"null"` once removed).
    pub awareness: HashMap<u64, String>,
    /// Close code received from the server.
    pub closed: Option<u16>,
    synced: bool,
}

impl Peer {
    pub async fn send(&mut self, msg: &Message) {
        self.ws
            .send(WsMessage::Binary(msg.encode_v1().into()))
            .await
            .unwrap();
    }

    /// Applies a local change and sends the resulting update.
    pub async fn edit(&mut self, f: impl FnOnce(&mut TransactionMut)) {
        let before = self.doc.transact().state_vector();
        {
            let mut txn = self.doc.transact_mut();
            f(&mut txn);
        }
        let update = self.doc.transact().encode_state_as_update_v1(&before);
        self.send(&Message::Sync(SyncMessage::Update(update))).await;
    }

    /// Publishes this peer's awareness state.
    pub async fn set_awareness(&mut self, clock: u32, json: &str) {
        let id = self.doc.client_id();
        let update = AwarenessUpdate {
            clients: HashMap::from([(
                id,
                AwarenessUpdateEntry {
                    clock,
                    json: json.into(),
                },
            )]),
        };
        self.send(&Message::Awareness(update)).await;
    }

    pub fn client_id(&self) -> u64 {
        self.doc.client_id().get()
    }

    fn handle(&mut self, bytes: &[u8]) -> Vec<Message> {
        let mut replies = Vec::new();
        let mut decoder = DecoderV1::new(Cursor::new(bytes));
        for msg in MessageReader::new(&mut decoder) {
            match msg.expect("server sent an undecodable message") {
                Message::Sync(SyncMessage::SyncStep1(sv)) => {
                    let u = self.doc.transact().encode_state_as_update_v1(&sv);
                    replies.push(Message::Sync(SyncMessage::SyncStep2(u)));
                }
                Message::Sync(SyncMessage::SyncStep2(u)) => {
                    self.doc
                        .transact_mut()
                        .apply_update(Update::decode_v1(&u).unwrap())
                        .unwrap();
                    self.synced = true;
                }
                Message::Sync(SyncMessage::Update(u)) => {
                    self.doc
                        .transact_mut()
                        .apply_update(Update::decode_v1(&u).unwrap())
                        .unwrap();
                }
                Message::Awareness(a) => {
                    for (id, e) in a.clients {
                        self.awareness.insert(id.get(), e.json.to_string());
                    }
                }
                Message::AwarenessQuery | Message::Auth(_) | Message::Custom(..) => {}
            }
        }
        replies
    }

    /// Reads and handles server messages until `pred` holds (true) or `timeout` passes.
    pub async fn pump_until(&mut self, timeout: Duration, pred: impl Fn(&Peer) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if pred(self) {
                return true;
            }
            let next = tokio::time::timeout_at(deadline, self.ws.next()).await;
            let Ok(Some(Ok(msg))) = next else {
                return pred(self);
            };
            match msg {
                WsMessage::Binary(b) => {
                    for reply in self.handle(&b) {
                        self.send(&reply).await;
                    }
                }
                WsMessage::Close(frame) => {
                    self.closed = Some(frame.map_or(1005, |f| u16::from(f.code)));
                    return pred(self);
                }
                _ => {}
            }
        }
    }

    /// Waits for the server to close the socket; returns the close code.
    pub async fn wait_closed(&mut self, timeout: Duration) -> Option<u16> {
        self.pump_until(timeout, |p| p.closed.is_some()).await;
        self.closed
    }

    pub async fn close(mut self) {
        let _ = self
            .ws
            .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: CloseCode::Normal,
                reason: "bye".into(),
            }))
            .await;
        // Drain until the server acknowledges.
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(Ok(_)) = self.ws.next().await {}
        })
        .await;
    }

    /// Text of a sticky (a Y.Map in `shapes`) as this peer sees it.
    pub fn shape_prop(&self, id: &str, key: &str) -> Option<Value> {
        shape_prop(&self.doc, id, key)
    }

    pub fn shape_count(&self) -> u32 {
        shape_count(&self.doc)
    }
}

pub fn shape_prop(doc: &Doc, id: &str, key: &str) -> Option<Value> {
    let txn = doc.transact();
    let shapes = txn.get_map("shapes")?;
    let Some(Out::YMap(m)) = shapes.get(&txn, id) else {
        return None;
    };
    m.get(&txn, key)
        .map(|o| copper_cloud_canvas::schema::out_to_json(&txn, &o))
}

pub fn shape_count(doc: &Doc) -> u32 {
    let txn = doc.transact();
    txn.get_map("shapes").map_or(0, |m| m.len(&txn))
}

/// Adds a sticky as a Y.Map of props (what the TS page does).
pub fn add_sticky(txn: &mut TransactionMut, id: &str, text: &str, x: f64) {
    use yrs::{Any, MapPrelim, WriteTxn as _};
    let shapes = txn.get_or_insert_map("shapes");
    let props: Vec<(&str, Any)> = vec![
        ("id", id.into()),
        ("type", "sticky".into()),
        ("x", x.into()),
        ("y", 0.0.into()),
        ("w", 200.0.into()),
        ("h", 140.0.into()),
        ("color", "yellow".into()),
        ("text", text.into()),
        ("by", "peer".into()),
    ];
    shapes.insert(txn, id, MapPrelim::from_iter(props));
}

/// Sets one prop of an existing shape.
pub fn set_prop(txn: &mut TransactionMut, id: &str, key: &str, value: &str) {
    use yrs::WriteTxn as _;
    let shapes = txn.get_or_insert_map("shapes");
    if let Some(Out::YMap(m)) = shapes.get(txn, id) {
        m.insert(txn, key, value);
    }
}

pub fn client_id(v: u64) -> ClientID {
    ClientID::new(v)
}

/// Polls `f` until it returns true or `timeout` passes.
pub async fn eventually<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if f().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub fn sv_b64(doc: &Doc) -> String {
    base64::engine::general_purpose::STANDARD.encode(doc.transact().state_vector().encode_v1())
}

pub fn empty_sv() -> StateVector {
    StateVector::default()
}

pub fn arc<T>(t: T) -> Arc<T> {
    Arc::new(t)
}
