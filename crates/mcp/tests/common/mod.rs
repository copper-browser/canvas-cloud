//! Test harness for the agent seams: a real server (core + engine + mcp routers, exactly as
//! `canvas_cloud::app` composes them, minus the crates this one cannot depend on) on an
//! ephemeral port, over native PostgreSQL (`CANVAS_CLOUD_TEST_MCP_DATABASE_URL`, default
//! `postgres://farce@localhost:5433/cvc_mcp_test`), with an HTTP MCP client and a minimal
//! y-websocket peer.

#![allow(dead_code, clippy::missing_panics_doc)]

use std::net::SocketAddr;
use std::time::Duration;

use canvas_cloud_core::config::Config;
use canvas_cloud_core::identity::{
    create_access_token, create_app, create_session, insert_user, NewUser, Scope,
};
use canvas_cloud_core::ids;
use canvas_cloud_core::state::{AppState, SharedState};
use canvas_cloud_core::Mounts;
use futures::{SinkExt as _, StreamExt as _};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;
use yrs::encoding::read::Cursor;
use yrs::sync::{Message, MessageReader, SyncMessage};
use yrs::types::ToJson as _;
use yrs::updates::decoder::{Decode as _, DecoderV1};
use yrs::updates::encoder::Encode as _;
use yrs::{Doc, Map as _, ReadTxn as _, Transact as _, Update};

pub const DATABASE_URL: &str = "postgres://farce@localhost:5433/cvc_mcp_test";

pub fn database_url() -> String {
    std::env::var("CANVAS_CLOUD_TEST_MCP_DATABASE_URL").unwrap_or_else(|_| DATABASE_URL.to_owned())
}

static MIGRATED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn migrate(pool: &PgPool) {
    MIGRATED
        .get_or_init(|| async {
            let migrator = &canvas_cloud_core::db::MIGRATOR;
            if let Err(e) = migrator.run(pool).await {
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

pub struct TestApp {
    pub state: SharedState,
    pub addr: SocketAddr,
    pub http: reqwest::Client,
}

#[derive(Clone, Debug)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    /// A `cvs_` session.
    pub session: String,
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    /// `public_url` defaults to the server's own `http://127.0.0.1:<port>`.
    pub async fn with(f: impl FnOnce(&mut Config)) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut cfg = Config::for_tests(&database_url());
        cfg.public_url = format!("http://{addr}");
        f(&mut cfg);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(30))
            .connect(&cfg.database_url)
            .await
            .expect(
                "connecting to the mcp test database \
                 (createdb -w -h localhost -p 5433 cvc_mcp_test)",
            );
        migrate(&pool).await;
        let state = AppState::new(pool, cfg);
        let router = canvas_cloud_core::app_with(
            state.clone(),
            Mounts::new()
                .v1(canvas_cloud_engine::router())
                .v1(canvas_cloud_mcp::router())
                .root(canvas_cloud_engine::landing_router())
                .root(canvas_cloud_mcp::root_router())
                .features(canvas_cloud_engine::FEATURES)
                .features(canvas_cloud_mcp::FEATURES),
        );
        let serve = router.into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, serve).await;
        });
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        Self { state, addr, http }
    }

    pub fn db(&self) -> &PgPool {
        &self.state.db
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub async fn user(&self, name: &str) -> User {
        let tag = ids::uuid_v7().simple().to_string();
        let email = format!("{name}-{}@example.test", &tag[20..]);
        let id = insert_user(
            self.db(),
            &NewUser {
                email: &email,
                display_name: Some(name),
                ..NewUser::default()
            },
        )
        .await
        .unwrap();
        let session = create_session(self.db(), id, Some("mcp-tests"))
            .await
            .unwrap();
        User { id, email, session }
    }

    pub async fn pat(&self, user: &User, scope: Scope) -> String {
        create_access_token(self.db(), user.id, "mcp tests", scope, None)
            .await
            .unwrap()
            .1
    }

    /// A linked-app key with `user` provisioned as `subject`.
    pub async fn app_key_for(&self, user: &User, subject: &str) -> (Uuid, String) {
        let (app_id, key) = create_app(self.db(), "mcp tests", false, true)
            .await
            .unwrap();
        sqlx::query("INSERT INTO app_users (app_id, subject, user_id) VALUES ($1, $2, $3)")
            .bind(app_id)
            .bind(subject)
            .bind(user.id)
            .execute(self.db())
            .await
            .unwrap();
        (app_id, key)
    }

    /// A REST call (`path` absolute, e.g. `/v1/canvases`).
    pub async fn rest(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = self.http.request(method, self.url(path)).bearer_auth(token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    pub fn mcp(&self, token: &str) -> Mcp<'_> {
        Mcp {
            app: self,
            token: token.to_owned(),
            next_id: 0,
            headers: vec![("mcp-protocol-version".into(), "2025-06-18".into())],
        }
    }

    /// `POST /mcp` with a raw body; returns status, headers and the body text.
    pub async fn post_mcp(
        &self,
        token: Option<&str>,
        body: impl Into<reqwest::Body>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, reqwest::header::HeaderMap, String) {
        let mut req = self
            .http
            .post(self.url("/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        (status, headers, resp.text().await.unwrap())
    }

    pub async fn peer(&self, token: &str, canvas: &str) -> Peer {
        let url = format!("ws://{}/v1/canvases/{canvas}/ws?token={token}", self.addr);
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("ws connect");
        let mut p = Peer {
            doc: Doc::new(),
            ws,
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

/// An MCP client over HTTP (one JSON-RPC message per POST).
pub struct Mcp<'a> {
    pub app: &'a TestApp,
    pub token: String,
    pub next_id: i64,
    pub headers: Vec<(String, String)>,
}

impl Mcp<'_> {
    /// Sends one message; returns the HTTP status and the parsed body (`Null` when empty).
    pub async fn send(&self, message: &Value) -> (StatusCode, Value) {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let (status, h, body) = self
            .app
            .post_mcp(
                Some(&self.token),
                serde_json::to_vec(message).unwrap(),
                &headers,
            )
            .await;
        if body.is_empty() {
            return (status, Value::Null);
        }
        assert!(
            h["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json"),
            "{h:?}"
        );
        (status, serde_json::from_str(&body).unwrap())
    }

    /// A request; returns the whole JSON-RPC response (asserting `200` and the id).
    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let (status, v) = self
            .send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        assert_eq!(status, StatusCode::OK, "{method}: {v}");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], id, "{v}");
        v
    }

    /// `tools/call`; returns the `result` (a tool result, success or `isError`).
    pub async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let v = self
            .request(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
            )
            .await;
        assert!(v.get("error").is_none(), "{tool}: {v}");
        let result = v["result"].clone();
        let content = result["content"].as_array().expect("content");
        assert_eq!(content.len(), 1, "{result}");
        assert_eq!(content[0]["type"], "text");
        result
    }

    /// A successful tool call: its `structuredContent` (checked equal to the text content).
    pub async fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments).await;
        assert_eq!(result["isError"], false, "{tool}: {result}");
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"], "{tool}");
        result["structuredContent"].clone()
    }

    /// A failed tool call: the error body (`{"error", "message", …}`) from its text content.
    pub async fn err(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments).await;
        assert_eq!(result["isError"], true, "{tool}: {result}");
        assert!(result.get("structuredContent").is_none(), "{result}");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }
}

/// A minimal y-websocket client.
pub struct Peer {
    pub doc: Doc,
    pub ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    synced: bool,
}

impl Peer {
    pub async fn send(&mut self, msg: &Message) {
        self.ws
            .send(WsMessage::Binary(msg.encode_v1().into()))
            .await
            .unwrap();
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
            if let WsMessage::Binary(b) = msg {
                for reply in self.handle(&b) {
                    self.send(&reply).await;
                }
            }
        }
    }

    /// The JSON of shape `id` in the peer's copy of the document, if it has it.
    pub fn shape_json(&self, id: &str) -> Option<String> {
        let shapes = self.doc.get_or_insert_map("shapes");
        let txn = self.doc.transact();
        let v = shapes.get(&txn, id)?;
        let mut out = String::new();
        v.to_json(&txn).to_json(&mut out);
        Some(out)
    }
}
