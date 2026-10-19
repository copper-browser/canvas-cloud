//! Shared integration-test harness for the accounts crate: real native Postgres
//! (`CANVAS_CLOUD_TEST_ACCOUNTS_DATABASE_URL`, default
//! `postgres://farce@localhost:5433/cvc_accounts_test`), the real core app with the engine and
//! accounts routers on an ephemeral port, a reqwest client (no redirects), an SSE frame
//! reader and an in-process mock OIDC provider ([`idp`]).

#![allow(dead_code, clippy::missing_panics_doc)]

pub mod idp;

use std::net::SocketAddr;
use std::time::Duration;

use canvas_cloud_core::config::Config;
use canvas_cloud_core::identity::{
    create_access_token, create_app, create_session, hash_password, insert_user, NewUser, Scope,
};
use canvas_cloud_core::ids;
use canvas_cloud_core::state::{AppState, SharedState};
use canvas_cloud_core::Mounts;
use futures::StreamExt as _;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

pub const DATABASE_URL: &str = "postgres://farce@localhost:5433/cvc_accounts_test";

pub fn database_url() -> String {
    std::env::var("CANVAS_CLOUD_TEST_ACCOUNTS_DATABASE_URL")
        .unwrap_or_else(|_| DATABASE_URL.to_owned())
}

static MIGRATED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn migrate(pool: &PgPool, reset: bool) {
    MIGRATED
        .get_or_init(|| async {
            let migrator = &canvas_cloud_core::db::MIGRATOR;
            if reset {
                sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
                    .execute(pool)
                    .await
                    .expect("resetting schema");
            }
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

pub const PASSWORD: &str = "correct horse battery staple";

/// One running server.
pub struct TestApp {
    pub state: SharedState,
    pub addr: SocketAddr,
    pub base: String,
    pub http: reqwest::Client,
}

/// A user with a password and a session.
#[derive(Clone, Debug)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub token: String,
}

pub fn unique_email(name: &str, domain: &str) -> String {
    let tag = ids::uuid_v7().simple().to_string();
    format!("{name}-{}@{domain}", &tag[20..])
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    /// A server whose test config `f` adjusted first (auth rate limit defaults to 100 000).
    pub async fn with(f: impl FnOnce(&mut Config)) -> Self {
        let mut cfg = Config::for_tests(&database_url());
        cfg.limits.auth_per_minute = 100_000;
        f(&mut cfg);
        Self::start(cfg, false).await
    }

    /// Like [`Self::with`], over an emptied database (the first call in the process drops
    /// and recreates the schema) — for tests of instance-wide state such as the signup policy.
    pub async fn fresh(database_url: &str, f: impl FnOnce(&mut Config)) -> Self {
        let mut cfg = Config::for_tests(database_url);
        cfg.limits.auth_per_minute = 100_000;
        f(&mut cfg);
        Self::start(cfg, true).await
    }

    async fn start(mut cfg: Config, reset: bool) -> Self {
        // `public_url` is this server (OIDC callback URLs and same-origin redirects use it).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        cfg.public_url = format!("http://{addr}");
        let pool = PgPoolOptions::new()
            .max_connections(6)
            .acquire_timeout(Duration::from_secs(30))
            .connect(&cfg.database_url)
            .await
            .expect(
                "connecting to the accounts test database \
                 (createdb -w -h localhost -p 5433 cvc_accounts_test)",
            );
        migrate(&pool, reset).await;
        let state = AppState::new(pool, cfg);
        let router = canvas_cloud_core::app_with(
            state.clone(),
            Mounts::new()
                .v1(canvas_cloud_engine::router())
                .v1(canvas_cloud_accounts::router())
                .root(canvas_cloud_engine::landing_router())
                .root(canvas_cloud_accounts::root_router())
                .features(canvas_cloud_engine::FEATURES)
                .features(canvas_cloud_accounts::FEATURES),
        );
        let serve = router.into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, serve).await;
        });
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        Self {
            state,
            addr,
            base: format!("http://{addr}"),
            http,
        }
    }

    pub fn db(&self) -> &PgPool {
        &self.state.db
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Insert a password user directly and sign them in (session token).
    pub async fn user(&self, name: &str) -> User {
        self.user_in(name, "example.test").await
    }

    pub async fn user_in(&self, name: &str, domain: &str) -> User {
        let email = unique_email(name, domain);
        let hash = hash_password(PASSWORD.into()).await.unwrap();
        let id = insert_user(
            self.db(),
            &NewUser {
                email: &email,
                display_name: Some(name),
                password_hash: Some(&hash),
                ..NewUser::default()
            },
        )
        .await
        .unwrap();
        let token = create_session(self.db(), id, Some("accounts-tests"))
            .await
            .unwrap();
        User { id, email, token }
    }

    /// A password-less user (as a linked app or OIDC would create) — no session.
    pub async fn federated_user(&self, email: &str) -> Uuid {
        insert_user(
            self.db(),
            &NewUser {
                email,
                ..NewUser::default()
            },
        )
        .await
        .unwrap()
    }

    pub async fn pat(&self, user_id: Uuid, scope: Scope) -> String {
        create_access_token(self.db(), user_id, "tests", scope, None)
            .await
            .unwrap()
            .1
    }

    /// A linked app with `user_id` provisioned as `subject`: `(app_id, key)`.
    pub async fn app_for(&self, user_id: Uuid, subject: &str) -> (Uuid, String) {
        let (app_id, key) = create_app(self.db(), "accounts tests", false, true)
            .await
            .unwrap();
        sqlx::query("INSERT INTO app_users (app_id, subject, user_id) VALUES ($1, $2, $3)")
            .bind(app_id)
            .bind(subject)
            .bind(user_id)
            .execute(self.db())
            .await
            .unwrap();
        (app_id, key)
    }

    /// One JSON call; `token` = bearer credential.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.call_with(method, path, token, body, &[]).await
    }

    pub async fn call_with(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut req = self.http.request(method, self.url(path));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        (status, value)
    }

    pub async fn get(&self, path: &str, token: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, Some(token), None).await
    }

    pub async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, path, token, Some(body)).await
    }

    pub async fn login(&self, email: &str, password: &str) -> (StatusCode, Value) {
        self.post(
            "/v1/auth/login",
            None,
            serde_json::json!({ "email": email, "password": password }),
        )
        .await
    }

    /// Open `GET /v1/events` with `token` as a bearer header plus extra headers.
    pub async fn events(&self, token: &str, headers: &[(&str, &str)]) -> Sse {
        let mut req = self
            .http
            .get(self.url("/v1/events"))
            .bearer_auth(token)
            .header("accept", "text/event-stream");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        Sse::open(req).await
    }
}

/// One SSE frame (comments are skipped).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub event: String,
    pub id: Option<String>,
    pub data: String,
}

impl Frame {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.data).unwrap()
    }
}

/// A minimal `text/event-stream` reader.
pub struct Sse {
    pub status: StatusCode,
    pub headers: reqwest::header::HeaderMap,
    stream: Option<futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>>,
    buf: String,
}

impl Sse {
    pub async fn open(req: reqwest::RequestBuilder) -> Self {
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let stream = status.is_success().then(|| resp.bytes_stream().boxed());
        Self {
            status,
            headers,
            stream,
            buf: String::new(),
        }
    }

    /// The next frame within `wait`, or `None` (timeout or end of stream).
    pub async fn next(&mut self, wait: Duration) -> Option<Frame> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            while let Some(pos) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..pos + 2).collect();
                let mut frame = Frame::default();
                let mut any = false;
                for line in block.lines() {
                    if line.starts_with(':') || line.is_empty() {
                        continue;
                    }
                    let (k, v) = line.split_once(':').unwrap_or((line, ""));
                    let v = v.strip_prefix(' ').unwrap_or(v);
                    any = true;
                    match k {
                        "event" => v.clone_into(&mut frame.event),
                        "id" => frame.id = Some(v.to_owned()),
                        "data" => {
                            if !frame.data.is_empty() {
                                frame.data.push('\n');
                            }
                            frame.data.push_str(v);
                        }
                        _ => {}
                    }
                }
                if any {
                    return Some(frame);
                }
            }
            let stream = self.stream.as_mut()?;
            let chunk = tokio::time::timeout_at(deadline, stream.next()).await.ok()??;
            self.buf
                .push_str(&String::from_utf8_lossy(&chunk.ok()?).replace("\r\n", "\n"));
        }
    }

    /// Frames until one named `event` arrives (returns it), within `wait`.
    pub async fn until(&mut self, event: &str, wait: Duration) -> Option<Frame> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let f = self.next(left).await?;
            if f.event == event {
                return Some(f);
            }
        }
    }
}
