//! Shared integration-test harness for the linked-apps routes: real native Postgres
//! (`CANVAS_CLOUD_TEST_APPS_DATABASE_URL`, default
//! `postgres://farce@localhost:5433/cvc_apps_test`), the real core app with the engine and
//! apps routers on an ephemeral port, users/sessions/apps inserted through core's identity
//! helpers, an SSE reader and a minimal y-websocket client (`yrs` + tokio-tungstenite).

#![allow(dead_code, clippy::missing_panics_doc)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine as _;
use canvas_cloud_core::config::Config;
use canvas_cloud_core::identity::{create_app, create_session, insert_user, NewUser};
use canvas_cloud_core::ids;
use canvas_cloud_core::state::{AppState, SharedState};
use canvas_cloud_core::Mounts;
use futures::{SinkExt as _, StreamExt as _};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
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
use yrs::sync::{Message, MessageReader, SyncMessage};
use yrs::updates::decoder::{Decode as _, DecoderV1};
use yrs::updates::encoder::Encode as _;
use yrs::{Doc, ReadTxn as _, StateVector, Transact as _, TransactionMut, Update};

pub const DATABASE_URL: &str = "postgres://farce@localhost:5433/cvc_apps_test";
pub const T: Duration = Duration::from_secs(5);

/// The test database: `CANVAS_CLOUD_TEST_APPS_DATABASE_URL`, else [`DATABASE_URL`].
pub fn database_url() -> String {
    std::env::var("CANVAS_CLOUD_TEST_APPS_DATABASE_URL").unwrap_or_else(|_| DATABASE_URL.to_owned())
}

static MIGRATED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn migrate(pool: &PgPool) {
    MIGRATED
        .get_or_init(|| async {
            let migrator = &canvas_cloud_core::db::MIGRATOR;
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
}

/// Who calls: a bearer credential, optionally acting as an app subject.
#[derive(Clone, Debug)]
pub struct Caller {
    pub token: String,
    pub subject: Option<String>,
    /// The user id behind it (when known).
    pub user_id: Option<Uuid>,
    pub email: String,
}

/// A linked app.
#[derive(Clone, Debug)]
pub struct App {
    pub id: Uuid,
    pub key: String,
}

impl App {
    /// The app itself (`/v1/apps/self/*`).
    pub fn me(&self) -> Caller {
        Caller {
            token: self.key.clone(),
            subject: None,
            user_id: None,
            email: String::new(),
        }
    }

    /// The app acting for `subject`.
    pub fn acting(&self, subject: &str) -> Caller {
        Caller {
            token: self.key.clone(),
            subject: Some(subject.to_owned()),
            user_id: None,
            email: String::new(),
        }
    }
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    pub async fn with(f: impl FnOnce(&mut Config)) -> Self {
        let mut cfg = Config::for_tests(&database_url());
        f(&mut cfg);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(30))
            .connect(&cfg.database_url)
            .await
            .expect("connecting to the apps test database (createdb -w -h localhost -p 5433 cvc_apps_test)");
        migrate(&pool).await;
        let state = AppState::new(pool, cfg);
        let router = canvas_cloud_core::app_with(
            state.clone(),
            Mounts::new()
                .v1(canvas_cloud_engine::router())
                .v1(canvas_cloud_apps::router())
                .root(canvas_cloud_engine::landing_router())
                .features(canvas_cloud_engine::FEATURES)
                .features(canvas_cloud_apps::FEATURES),
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
        }
    }

    pub fn db(&self) -> &PgPool {
        &self.state.db
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}/v1{path}", self.addr)
    }

    /// A unique email for `name`.
    pub fn email(name: &str) -> String {
        let tag = ids::uuid_v7().simple().to_string();
        format!("{name}-{}@example.test", &tag[20..])
    }

    /// A human with a password-less account and a session (`cvs_…`).
    pub async fn human(&self, name: &str) -> Caller {
        let email = Self::email(name);
        let id = insert_user(
            self.db(),
            &NewUser {
                email: &email,
                display_name: Some(name),
                password_hash: Some("$argon2id$not-a-real-hash"),
                ..NewUser::default()
            },
        )
        .await
        .unwrap();
        let token = create_session(self.db(), id, Some("apps-tests"))
            .await
            .unwrap();
        Caller {
            token,
            subject: None,
            user_id: Some(id),
            email,
        }
    }

    pub async fn app(&self, allow_import: bool, trust_email: bool) -> App {
        let (id, key) = create_app(self.db(), "apps tests", allow_import, trust_email)
            .await
            .unwrap();
        App { id, key }
    }

    /// Provisions `subject` (fresh email) for `app`; the caller acting as them.
    pub async fn provision(&self, app: &App, subject: &str) -> Caller {
        let email = Self::email(subject);
        let (s, v) = self
            .put(
                &app.me(),
                &format!("/apps/self/users/{subject}"),
                json!({ "email": email, "display_name": subject }),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let mut c = app.acting(subject);
        c.user_id = Some(Uuid::parse_str(v["user_id"].as_str().unwrap()).unwrap());
        c.email = email;
        c
    }

    /// One call through the full app.
    pub async fn call(
        &self,
        who: Option<&Caller>,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(format!("/v1{path}"));
        if let Some(c) = who {
            req = req.header("authorization", format!("Bearer {}", c.token));
            if let Some(s) = &c.subject {
                req = req.header("x-canvas-user", s);
            }
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        self.send(req).await
    }

    pub async fn send(&self, req: Request<Body>) -> (StatusCode, Value) {
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

    pub async fn get(&self, who: &Caller, path: &str) -> (StatusCode, Value) {
        self.call(Some(who), Method::GET, path, None).await
    }

    pub async fn post(&self, who: &Caller, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Some(who), Method::POST, path, Some(body)).await
    }

    pub async fn put(&self, who: &Caller, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Some(who), Method::PUT, path, Some(body)).await
    }

    pub async fn delete(&self, who: &Caller, path: &str) -> (StatusCode, Value) {
        self.call(Some(who), Method::DELETE, path, None).await
    }

    /// `POST /v1/apps/self/import[?mode=…]` with an NDJSON body.
    pub async fn import(
        &self,
        app: &App,
        mode: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, Value) {
        let path = match mode {
            Some(m) => format!("/v1/apps/self/import?mode={m}"),
            None => "/v1/apps/self/import".to_owned(),
        };
        let req = Request::post(path)
            .header("authorization", format!("Bearer {}", app.key))
            .header("content-type", "application/x-ndjson")
            .body(Body::from(body))
            .unwrap();
        self.send(req).await
    }

    /// Creates a shared canvas owned by `who`; returns its id.
    pub async fn canvas(&self, who: &Caller, name: &str) -> String {
        let (s, v) = self.post(who, "/canvases", json!({ "name": name })).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v["id"].as_str().unwrap().to_owned()
    }

    /// Makes `guest` an editor of `canvas` via invite + accept.
    pub async fn share(&self, owner: &Caller, canvas: &str, guest: &Caller) {
        let (s, inv) = self
            .post(
                owner,
                &format!("/canvases/{canvas}/invites"),
                json!({ "email": guest.email }),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{inv}");
        let id = inv["id"].as_str().unwrap();
        let (s, v) = self
            .post(guest, &format!("/invites/{id}/accept"), Value::Null)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    /// Opens a raw WebSocket (the handshake result is returned as-is).
    pub async fn ws_raw(
        &self,
        canvas: &str,
        who: &Caller,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, tokio_tungstenite::tungstenite::Error>
    {
        let url = format!("ws://{}/v1/canvases/{canvas}/ws", self.addr);
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {}", who.token).parse().unwrap(),
        );
        if let Some(s) = &who.subject {
            req.headers_mut()
                .insert("x-canvas-user", s.parse().unwrap());
        }
        tokio_tungstenite::connect_async(req)
            .await
            .map(|(ws, _)| ws)
    }

    /// Connects a y-websocket peer and completes the initial sync.
    pub async fn peer(&self, who: &Caller, canvas: &str) -> Peer {
        let ws = self.ws_raw(canvas, who).await.expect("ws connect");
        let mut p = Peer {
            doc: Doc::new(),
            ws,
            closed: None,
            synced: false,
        };
        let sv = p.doc.transact().state_vector();
        p.send(&Message::Sync(SyncMessage::SyncStep1(sv))).await;
        assert!(p.pump_until(T, |p| p.synced).await, "initial sync");
        p
    }

    /// The raw stored history of a canvas, oldest first, decrypted: the snapshot (if any)
    /// followed by the update log — what a migrator exports.
    pub async fn stored_updates(&self, canvas: Uuid) -> Vec<Vec<u8>> {
        let key = canvas_cloud_engine::store::doc_key(self.db(), &self.state.crypto, canvas)
            .await
            .unwrap();
        let mut out = Vec::new();
        let snapshot: Option<(i64, Vec<u8>)> =
            sqlx::query_as("SELECT seq, state FROM canvas_snapshots WHERE canvas_id = $1")
                .bind(canvas)
                .fetch_optional(self.db())
                .await
                .unwrap();
        let since = snapshot.as_ref().map_or(0, |s| s.0);
        if let Some((_, sealed)) = snapshot {
            out.push(canvas_cloud_engine::store::open(&key, canvas, &sealed).unwrap());
        }
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            r#"SELECT "update" FROM canvas_updates WHERE canvas_id = $1 AND seq > $2 ORDER BY seq"#,
        )
        .bind(canvas)
        .bind(since)
        .fetch_all(self.db())
        .await
        .unwrap();
        for sealed in rows {
            out.push(canvas_cloud_engine::store::open(&key, canvas, &sealed).unwrap());
        }
        out
    }
}

/// One NDJSON body.
pub fn ndjson(lines: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in lines {
        out.extend_from_slice(serde_json::to_string(l).unwrap().as_bytes());
        out.push(b'\n');
    }
    out
}

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A complete update creating `n` stickies (one client, one transaction).
pub fn sticky_update(n: usize) -> Vec<u8> {
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        for i in 0..n {
            add_sticky(
                &mut txn,
                &format!("s{i}"),
                &format!("note {i}"),
                f64::from(u32::try_from(i).unwrap()) * 10.0,
            );
        }
    }
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    update
}

/// Adds a sticky as a Y.Map of props (what the canvas page does).
pub fn add_sticky(txn: &mut TransactionMut, id: &str, text: &str, x: f64) {
    use yrs::{Any, Map as _, MapPrelim, WriteTxn as _};
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

/// A minimal y-websocket client.
pub struct Peer {
    pub doc: Doc,
    pub ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
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
                _ => {}
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
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(Ok(_)) = self.ws.next().await {}
        })
        .await;
    }
}

/// One server-sent event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sse {
    pub event: String,
    pub id: Option<String>,
    pub data: String,
}

/// An open `GET /v1/apps/self/events` stream over real HTTP.
pub struct EventStream {
    resp: reqwest::Response,
    buf: String,
    pub comments: usize,
}

impl EventStream {
    pub async fn open(app: &TestApp, key: &str, last_event_id: Option<&str>) -> Self {
        let mut rb = reqwest::Client::new()
            .get(app.url("/apps/self/events"))
            .header("authorization", format!("Bearer {key}"))
            .header("accept", "text/event-stream");
        if let Some(id) = last_event_id {
            rb = rb.header("last-event-id", id);
        }
        let resp = rb.send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert!(resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
        Self {
            resp,
            buf: String::new(),
            comments: 0,
        }
    }

    /// The next event (comments skipped), or `None` after `timeout`.
    pub async fn next(&mut self, timeout: Duration) -> Option<Sse> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(pos) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..pos + 2).collect();
                let mut ev = Sse::default();
                let mut data: Vec<&str> = Vec::new();
                let mut any = false;
                for line in block.lines() {
                    if line.starts_with(':') {
                        self.comments += 1;
                        continue;
                    }
                    let (field, value) = line.split_once(':').unwrap_or((line, ""));
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "event" => value.clone_into(&mut ev.event),
                        "id" => ev.id = Some(value.to_owned()),
                        "data" => data.push(value),
                        _ => continue,
                    }
                    any = true;
                }
                if any {
                    ev.data = data.join("\n");
                    return Some(ev);
                }
                continue;
            }
            let chunk = tokio::time::timeout_at(deadline, self.resp.chunk()).await;
            match chunk {
                Ok(Ok(Some(bytes))) => self.buf.push_str(&String::from_utf8_lossy(&bytes)),
                _ => return None,
            }
        }
    }

    /// Events until `pred` matches one (returned), or `None` after `timeout`.
    pub async fn until(&mut self, timeout: Duration, pred: impl Fn(&Sse) -> bool) -> Option<Sse> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let ev = self.next(left).await?;
            if pred(&ev) {
                return Some(ev);
            }
        }
    }

    /// Whether the server ended the stream within `timeout`.
    pub async fn ended(&mut self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, self.resp.chunk()).await {
                Ok(Ok(Some(_))) => {}
                Ok(Ok(None) | Err(_)) => return true,
                Err(_) => return false,
            }
        }
    }
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

/// Every event published so far for `user` (drains a subscriber created earlier).
pub fn drain_kinds(
    rx: &mut tokio::sync::broadcast::Receiver<canvas_cloud_core::events::Event>,
) -> Vec<(Uuid, String)> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        #[allow(unreachable_patterns)]
        if let canvas_cloud_core::events::Event::Canvas { canvas_id, kind } = ev {
            out.push((canvas_id, kind));
        }
    }
    out
}

pub fn map_of(v: &Value) -> HashMap<String, Value> {
    v.as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}
