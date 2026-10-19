//! Identity rules (CONTRACT §4) against a real native Postgres
//! (`CANVAS_CLOUD_TEST_CORE_DATABASE_URL`, default
//! `postgres://localhost:5432/cvc_core_test`): sessions, personal access tokens, app
//! keys, the extractors and the store helpers, through the real `app_with` stack.

#![allow(clippy::missing_panics_doc, clippy::too_many_lines)]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use canvas_cloud_core::config::Config;
use canvas_cloud_core::identity::{
    self, create_access_token, create_app, create_session, credential_digest, insert_user, AppAuth,
    AuthUser, AuthUserAnyScope, NewUser, Scope,
};
use canvas_cloud_core::state::{AppState, SharedState};
use canvas_cloud_core::{ids, Mounts};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

const DATABASE_URL: &str = "postgres://localhost:5432/cvc_core_test";

fn database_url() -> String {
    std::env::var("CANVAS_CLOUD_TEST_CORE_DATABASE_URL").unwrap_or_else(|_| DATABASE_URL.into())
}

static MIGRATED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn state() -> SharedState {
    let cfg = Config::for_tests(&database_url());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&cfg.database_url)
        .await
        .expect("connecting (createdb -w -h localhost cvc_core_test)");
    MIGRATED
        .get_or_init(|| async {
            canvas_cloud_core::db::migrate(&pool)
                .await
                .expect("migrate");
        })
        .await;
    AppState::new(pool, cfg)
}

fn who(u: &AuthUser) -> Value {
    json!({
        "user_id": u.user_id,
        "email": u.email,
        "display_name": u.display_name,
        "via": u.via,
        "read_only": u.is_read_only(),
    })
}

fn test_app(state: &SharedState) -> Router {
    let v1 = Router::new()
        .route(
            "/whoami",
            get(|u: AuthUser| async move { Json(who(&u)) })
                .post(|u: AuthUser| async move { Json(who(&u)) }),
        )
        .route(
            "/twice",
            get(|a: AuthUser, b: AuthUser| async move { Json(json!({ "same": a == b })) }),
        )
        .route(
            "/any-scope",
            post(|AuthUserAnyScope(u): AuthUserAnyScope| async move { Json(who(&u)) }),
        )
        .route(
            "/apps/self",
            get(|a: AppAuth| async move {
                Json(json!({ "app_id": a.app_id, "name": a.name, "allow_import": a.allow_import }))
            }),
        );
    canvas_cloud_core::app_with(state.clone(), Mounts::new().v1(v1))
}

struct Call<'a> {
    method: Method,
    path: &'a str,
    token: Option<&'a str>,
    headers: Vec<(&'a str, &'a str)>,
}

impl<'a> Call<'a> {
    fn get(path: &'a str, token: &'a str) -> Self {
        Self {
            method: Method::GET,
            path,
            token: Some(token),
            headers: Vec::new(),
        }
    }

    fn method(mut self, m: Method) -> Self {
        self.method = m;
        self
    }

    fn header(mut self, k: &'a str, v: &'a str) -> Self {
        self.headers.push((k, v));
        self
    }

    fn anonymous(mut self) -> Self {
        self.token = None;
        self
    }

    async fn send(self, app: &Router) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(self.method)
            .uri(format!("/v1{}", self.path));
        if let Some(t) = self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        for (k, v) in self.headers {
            req = req.header(k, v);
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }
}

async fn user(db: &PgPool, name: &str) -> (Uuid, String) {
    let email = format!(
        "{name}-{}@Example.test",
        &ids::uuid_v7().simple().to_string()[20..]
    );
    let id = insert_user(
        db,
        &NewUser {
            email: &email,
            display_name: Some(name),
            ..NewUser::default()
        },
    )
    .await
    .unwrap();
    (id, email)
}

fn ws_headers(c: Call<'_>) -> Call<'_> {
    c.header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
}

#[tokio::test]
async fn sessions_authenticate_slide_and_expire() {
    let st = state().await;
    let app = test_app(&st);
    let (id, email) = user(&st.db, "ann").await;
    let token = create_session(&st.db, id, Some("tests/1.0")).await.unwrap();
    assert!(token.starts_with("cvs_"));

    let (s, v) = Call::get("/whoami", &token).send(&app).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["user_id"], id.to_string());
    assert_eq!(v["email"], email);
    assert_eq!(v["display_name"], "ann");
    assert_eq!(v["via"]["kind"], "session");
    // Several extractors in one handler: one resolution, same caller.
    let (s, v) = Call::get("/twice", &token).send(&app).await;
    assert_eq!((s, v["same"].as_bool()), (StatusCode::OK, Some(true)));

    // Only the SHA-256 of the full token is stored.
    let digest = credential_digest(&token);
    let (sid, ua): (Uuid, Option<String>) =
        sqlx::query_as("SELECT id, user_agent FROM sessions WHERE token_sha256 = $1")
            .bind(&digest[..])
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert_eq!(ua.as_deref(), Some("tests/1.0"));

    // Sliding expiry, touched at most once a minute.
    sqlx::query(
        "UPDATE sessions SET last_used_at = now() - interval '2 minutes',
                             expires_at = now() + interval '1 day' WHERE id = $1",
    )
    .bind(sid)
    .execute(&st.db)
    .await
    .unwrap();
    let (s, _) = Call::get("/whoami", &token).send(&app).await;
    assert_eq!(s, StatusCode::OK);
    let days: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM expires_at - now())::float8 / 86400 FROM sessions WHERE id = $1",
    )
    .bind(sid)
    .fetch_one(&st.db)
    .await
    .unwrap();
    assert!((29.9..=30.1).contains(&days), "slid to 30 days: {days}");
    sqlx::query("UPDATE sessions SET expires_at = now() + interval '1 day' WHERE id = $1")
        .bind(sid)
        .execute(&st.db)
        .await
        .unwrap();
    let (s, _) = Call::get("/whoami", &token).send(&app).await;
    assert_eq!(s, StatusCode::OK);
    let days: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM expires_at - now())::float8 / 86400 FROM sessions WHERE id = $1",
    )
    .bind(sid)
    .fetch_one(&st.db)
    .await
    .unwrap();
    assert!(days < 1.1, "not touched again within a minute: {days}");

    // Expired → 401; purge removes it.
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(sid)
        .execute(&st.db)
        .await
        .unwrap();
    let (s, v) = Call::get("/whoami", &token).send(&app).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], "unauthorized");
    canvas_cloud_core::db::purge_expired_sessions(&st.db)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE id = $1")
        .bind(sid)
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(left, 0);

    // Logout revokes.
    let token = create_session(&st.db, id, None).await.unwrap();
    let (_, v) = Call::get("/whoami", &token).send(&app).await;
    let sid: Uuid = v["via"]["session_id"].as_str().unwrap().parse().unwrap();
    assert!(identity::revoke_session(&st.db, sid).await.unwrap());
    let (s, _) = Call::get("/whoami", &token).send(&app).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Missing, malformed, unknown and wrong-scheme credentials.
    let unknown = identity::mint(identity::CredentialKind::Session).0;
    for bad in ["", "cvs_short", "nope", unknown.as_str()] {
        let (s, v) = Call::get("/whoami", bad).send(&app).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{bad:?}");
        assert_eq!(v["error"], "unauthorized");
    }
    let (s, _) = Call::get("/whoami", "").anonymous().send(&app).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = Call::get("/whoami", "")
        .anonymous()
        .header("authorization", &format!("Basic {token}"))
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn disabled_users_are_refused() {
    let st = state().await;
    let app = test_app(&st);
    let (id, _) = user(&st.db, "dora").await;
    let session = create_session(&st.db, id, None).await.unwrap();
    let (_, pat) = create_access_token(&st.db, id, "cli", Scope::Write, None)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled = true WHERE id = $1")
        .bind(id)
        .execute(&st.db)
        .await
        .unwrap();
    for t in [&session, &pat] {
        let (s, v) = Call::get("/whoami", t).send(&app).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"], "account_disabled");
    }
}

#[tokio::test]
async fn access_tokens_and_scopes() {
    let st = state().await;
    let app = test_app(&st);
    let (id, _) = user(&st.db, "pat").await;
    let (wid, write) = create_access_token(&st.db, id, "  agent  ", Scope::Write, None)
        .await
        .unwrap();
    let (rid, read) = create_access_token(&st.db, id, "reader", Scope::Read, None)
        .await
        .unwrap();
    assert!(write.starts_with("cvt_") && read.starts_with("cvt_"));
    let name: String = sqlx::query_scalar("SELECT name FROM access_tokens WHERE id = $1")
        .bind(wid)
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(name, "agent");
    assert!(
        create_access_token(&st.db, id, "   ", Scope::Read, None)
            .await
            .is_err(),
        "empty name"
    );

    let (s, v) = Call::get("/whoami", &write)
        .method(Method::POST)
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["via"],
        json!({ "kind": "token", "token_id": wid, "scope": "write" })
    );
    let used: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT last_used_at FROM access_tokens WHERE id = $1")
            .bind(wid)
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert!(used.is_some(), "last_used_at recorded");

    // Read scope: GET/HEAD only, never a WebSocket upgrade.
    let (s, v) = Call::get("/whoami", &read).send(&app).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["read_only"], true);
    let (s, _) = Call::get("/whoami", &read)
        .method(Method::HEAD)
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = Call::get("/whoami", &read)
        .method(Method::POST)
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(v["error"], "read_only_token");
    let (s, v) = ws_headers(Call::get("/whoami", &read)).send(&app).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(v["error"], "read_only_token");
    // AuthUserAnyScope leaves the scope to the handler (MCP enforces it per tool).
    let (s, v) = Call::get("/any-scope", &read)
        .method(Method::POST)
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["read_only"], true);

    // Expired and revoked tokens are dead.
    sqlx::query("UPDATE access_tokens SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(rid)
        .execute(&st.db)
        .await
        .unwrap();
    let (s, _) = Call::get("/whoami", &read).send(&app).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    sqlx::query("UPDATE access_tokens SET revoked_at = now() WHERE id = $1")
        .bind(wid)
        .execute(&st.db)
        .await
        .unwrap();
    let (s, _) = Call::get("/whoami", &write).send(&app).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn query_tokens_only_for_websocket_and_event_stream() {
    let st = state().await;
    let app = test_app(&st);
    let (id, _) = user(&st.db, "qt").await;
    let token = create_session(&st.db, id, None).await.unwrap();
    let path = format!("/whoami?token={token}");
    let (s, _) = Call::get(&path, "").anonymous().send(&app).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "plain GET ignores ?token=");
    let (s, v) = Call::get(&path, "")
        .anonymous()
        .header("accept", "text/event-stream")
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["user_id"], id.to_string());
    let (s, v) = ws_headers(Call::get(&path, "").anonymous())
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = Call::get(&path, "")
        .anonymous()
        .method(Method::POST)
        .header("accept", "text/event-stream")
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "event streams are GETs");
}

#[tokio::test]
async fn app_keys_act_for_their_users_or_as_themselves() {
    let st = state().await;
    let app = test_app(&st);
    let (uid, email) = user(&st.db, "linked").await;
    let (app_id, key) = create_app(&st.db, "Linker", true, false).await.unwrap();
    assert!(key.starts_with("cva_"));
    sqlx::query("INSERT INTO app_users (app_id, subject, user_id) VALUES ($1, 'sub-1', $2)")
        .bind(app_id)
        .bind(uid)
        .execute(&st.db)
        .await
        .unwrap();

    let (s, v) = Call::get("/whoami", &key)
        .header("x-canvas-user", "sub-1")
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["user_id"], uid.to_string());
    assert_eq!(v["email"], email);
    assert_eq!(
        v["via"],
        json!({ "kind": "app", "app_id": app_id, "subject": "sub-1" })
    );
    let (s, v) = Call::get("/whoami", &key)
        .header("x-canvas-user", "sub-2")
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], "unknown_user");
    let (s, v) = Call::get("/whoami", &key).send(&app).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], "unauthorized");
    let long = "x".repeat(201);
    let (s, _) = Call::get("/whoami", &key)
        .header("x-canvas-user", &long)
        .send(&app)
        .await;
    assert_ne!(s, StatusCode::OK);

    // As itself (no X-Canvas-User) on /apps/self/*.
    let (s, v) = Call::get("/apps/self", &key).send(&app).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["app_id"], app_id.to_string());
    assert_eq!(v["allow_import"], true);
    let (s, _) = Call::get("/apps/self", &key)
        .header("x-canvas-user", "sub-1")
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let session = create_session(&st.db, uid, None).await.unwrap();
    let (s, _) = Call::get("/apps/self", &session).send(&app).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // X-Canvas-User means nothing next to a user credential: the caller stays itself.
    let (s, v) = Call::get("/whoami", &session)
        .header("x-canvas-user", "sub-1")
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["via"]["kind"], "session");

    let used: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT last_used_at FROM apps WHERE id = $1")
            .bind(app_id)
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert!(used.is_some());

    // Listing, rotation (old key dies), revocation.
    let listed = identity::list_apps(&st.db).await.unwrap();
    let row = listed.iter().find(|a| a.id == app_id).unwrap();
    assert_eq!((row.users, row.trust_email), (1, false));
    let new_key = identity::rotate_app_key(&st.db, app_id)
        .await
        .unwrap()
        .unwrap();
    let (s, _) = Call::get("/apps/self", &key).send(&app).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = Call::get("/apps/self", &new_key).send(&app).await;
    assert_eq!(s, StatusCode::OK);
    assert!(identity::revoke_app(&st.db, app_id).await.unwrap());
    let (s, _) = Call::get("/apps/self", &new_key).send(&app).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = Call::get("/whoami", &new_key)
        .header("x-canvas-user", "sub-1")
        .send(&app)
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn users_are_unique_by_email_case_insensitively() {
    let st = state().await;
    let (id, email) = user(&st.db, "Case").await;
    let err = insert_user(
        &st.db,
        &NewUser {
            email: &email.to_uppercase(),
            ..NewUser::default()
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        canvas_cloud_core::error::ApiError::Conflict(_)
    ));
    assert_eq!(
        identity::user_id_by_email(&st.db, &email.to_lowercase())
            .await
            .unwrap(),
        Some(id)
    );
    // No display name given → the local part.
    let plain = format!("plain-{}@example.test", ids::uuid_v7().simple());
    let pid = insert_user(
        &st.db,
        &NewUser {
            email: &plain,
            ..NewUser::default()
        },
    )
    .await
    .unwrap();
    let name: String = sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
        .bind(pid)
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert!(name.starts_with("plain-"));
}

#[tokio::test]
async fn info_is_public_and_signup_follows_settings() {
    let st = state().await;
    let app = test_app(&st);
    identity::set_setting(&st.db, "allow_signup", &json!(false))
        .await
        .unwrap();
    let no_password_users: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM users WHERE password_hash IS NOT NULL)",
    )
    .fetch_one(&st.db)
    .await
    .unwrap();
    let (s, v) = Call::get("/info", "").anonymous().send(&app).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "canvas-cloud");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["public_url"], "http://localhost:8480");
    assert_eq!(v["signup"], no_password_users);
    assert_eq!(v["auth"], json!({ "password": true, "oidc": [] }));
    assert_eq!(v["web"], json!({ "root": false, "app_url": null }));
    assert!(v["features"]
        .as_array()
        .unwrap()
        .contains(&json!("app_users")));
    assert_eq!(v["limits"]["auth_per_minute"], 10);
    assert_eq!(v["limits"]["import_max_bytes"], 512 << 20);
    identity::set_setting(&st.db, "allow_signup", &json!(true))
        .await
        .unwrap();
    let (_, v) = Call::get("/info", "").anonymous().send(&app).await;
    assert_eq!(v["signup"], true);
    identity::set_setting(&st.db, "allow_signup", &json!(false))
        .await
        .unwrap();

    // OIDC providers and the served web app are advertised.
    let toml = format!(
        "database_url = \"{}\"\nmaster_key = \"{}\"\nlisten = \"127.0.0.1:8480\"\nweb_root = \"/srv/app\"\n[tls]\nmode = \"off\"\n[[oidc]]\nid = \"google\"\nname = \"Google\"\nissuer = \"https://accounts.google.com\"\nclient_id = \"c\"\n[[oidc]]\nid = \"corp\"\nissuer = \"https://sso.example.test\"\nclient_id = \"c\"\n",
        database_url(),
        ids::b64url(&[7u8; 32])
    );
    let cfg = Config::from_toml_str(&toml, &|_| None).unwrap();
    let st2 = AppState::new(st.db.clone(), cfg);
    let (_, v) = Call::get("/info", "")
        .anonymous()
        .send(&test_app(&st2))
        .await;
    assert_eq!(
        v["auth"]["oidc"],
        json!([
            { "id": "google", "name": "Google", "start": "/v1/auth/oidc/google/start" },
            { "id": "corp", "name": "corp", "start": "/v1/auth/oidc/corp/start" },
        ])
    );
    assert_eq!(
        v["web"],
        json!({ "root": true, "app_url": "http://localhost:8480/#/join/{token}" })
    );
}

#[tokio::test]
async fn migrations_are_complete_and_idempotent() {
    let st = state().await;
    canvas_cloud_core::db::migrate(&st.db).await.unwrap();
    let (applied, pending) = canvas_cloud_core::db::pending_migrations(&st.db)
        .await
        .unwrap();
    assert!(pending.is_empty(), "{pending:?}");
    assert_eq!(applied, canvas_cloud_core::db::MIGRATOR.iter().count());
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = 'public' ORDER BY 1",
    )
    .fetch_all(&st.db)
    .await
    .unwrap();
    for t in [
        "users",
        "sessions",
        "access_tokens",
        "apps",
        "app_users",
        "server_settings",
        "admin_audit",
        "canvases",
        "canvas_members",
        "canvas_mentions",
    ] {
        assert!(tables.iter().any(|x| x == t), "{t} in {tables:?}");
    }
}
