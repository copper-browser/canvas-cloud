//! The composed binary app and the admin CLI against native Postgres
//! (`CANVAS_CLOUD_TEST_DATABASE_URL`, default `postgres://farce@localhost:5433/cvc_bin_test`).

#![allow(clippy::too_many_lines)]

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use canvas_cloud::cli::Cli;
use canvas_cloud_core::config::Config;
use canvas_cloud_core::identity;
use canvas_cloud_core::state::{AppState, SharedState};
use clap::Parser as _;
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use uuid::Uuid;

fn database_url() -> String {
    std::env::var("CANVAS_CLOUD_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://farce@localhost:5433/cvc_bin_test".into())
}

static MIGRATED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn state_with(f: impl FnOnce(&mut Config)) -> SharedState {
    let mut cfg = Config::for_tests(&database_url());
    f(&mut cfg);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&cfg.database_url)
        .await
        .expect("connecting (createdb -w -h localhost -p 5433 cvc_bin_test)");
    MIGRATED
        .get_or_init(|| async {
            canvas_cloud_core::db::migrate(&pool)
                .await
                .expect("migrate");
        })
        .await;
    AppState::new(pool, cfg)
}

async fn call(
    app: &Router,
    method: Method,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let (s, _, body) = raw(app, method, path, token).await;
    (s, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn raw(
    app: &Router,
    method: Method,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cvc-{tag}-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn composed_app_serves_api_landing_and_info() {
    let st = state_with(|_| {}).await;
    let app = canvas_cloud::app(st.clone());
    let email = format!("bin-{}@example.test", Uuid::now_v7().simple());
    let uid = identity::insert_user(
        &st.db,
        &identity::NewUser {
            email: &email,
            ..identity::NewUser::default()
        },
    )
    .await
    .unwrap();
    let session = identity::create_session(&st.db, uid, None).await.unwrap();

    let (s, v) = call(&app, Method::GET, "/v1/canvases", Some(&session)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let canvases = v.as_array().unwrap();
    assert_eq!(canvases.len(), 1, "{v}");
    assert_eq!(canvases[0]["kind"], "personal");

    let (s, v) = call(&app, Method::GET, "/v1/info", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["name"], "canvas-cloud");
    for f in ["app_users", "canvases", "mentions", "share_links", "people"] {
        assert!(
            v["features"].as_array().unwrap().contains(&json!(f)),
            "{f}: {v}"
        );
    }
    assert_eq!(v["web"], json!({ "root": false, "app_url": null }));

    let (s, _, body) = raw(&app, Method::GET, "/join/not-a-token", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(String::from_utf8(body).unwrap().contains("canvas app"));
    let (s, v) = call(&app, Method::GET, "/nope", None).await;
    assert_eq!(
        (s, &v["error"]),
        (StatusCode::NOT_FOUND, &json!("not_found"))
    );
    let (s, _) = call(&app, Method::GET, "/v1/canvases", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn web_root_app_and_share_links() {
    let web = temp_dir("web");
    std::fs::write(
        web.join("index.html"),
        "<!doctype html><title>canvas</title>",
    )
    .unwrap();
    std::fs::create_dir_all(web.join("assets")).unwrap();
    std::fs::write(web.join("assets/index.js"), "export {}").unwrap();
    let st = state_with(|cfg| {
        cfg.public_url = "https://canvas.example.test".into();
        cfg.web_root = Some(web.clone());
    })
    .await;
    let app = canvas_cloud::app(st.clone());

    let (s, h, body) = raw(&app, Method::GET, "/", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CACHE_CONTROL], "no-cache");
    assert!(h[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    assert_eq!(body, b"<!doctype html><title>canvas</title>");
    let (s, h, _) = raw(&app, Method::GET, "/assets/index.js", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .contains("javascript"));
    let (s, _) = call(&app, Method::GET, "/assets/missing.js", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // API, healthz and /mcp are never shadowed by the web root.
    let (s, v) = call(&app, Method::GET, "/v1/info", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["web"],
        json!({ "root": true, "app_url": "https://canvas.example.test/#/join/{token}" })
    );
    assert_eq!(v["public_url"], "https://canvas.example.test");
    let (s, v) = call(&app, Method::GET, "/v1/nope", None).await;
    assert_eq!(
        (s, &v["error"]),
        (StatusCode::NOT_FOUND, &json!("not_found"))
    );
    let (s, _) = call(&app, Method::GET, "/mcp/x", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, body) = raw(&app, Method::GET, "/healthz", None).await;
    assert_eq!((s, body.as_slice()), (StatusCode::OK, b"ok".as_slice()));

    // A share link opened in a browser lands in the served app's join route.
    let token = canvas_cloud_core::ids::random_token();
    let (s, h, _) = raw(&app, Method::GET, &format!("/join/{token}"), None).await;
    assert_eq!(s, StatusCode::FOUND);
    assert_eq!(
        h[header::LOCATION],
        format!("https://canvas.example.test/#/join/{token}").as_str()
    );
    std::fs::remove_dir_all(&web).unwrap();
}

fn cli(config: &std::path::Path, args: &[&str]) -> anyhow::Result<std::process::ExitCode> {
    let mut command_line = vec!["canvas-cloud", "--config", config.to_str().unwrap()];
    command_line.extend_from_slice(args);
    canvas_cloud::cli::run(Cli::try_parse_from(command_line)?)
}

#[test]
fn admin_cli_manages_users_settings_and_apps() {
    let dir = temp_dir("cli");
    let config = dir.join("canvas-cloud.toml");
    std::fs::write(
        &config,
        format!(
            "listen = \"127.0.0.1:18481\"\ndatabase_url = \"{}\"\nmaster_key = \"{}\"\nmetrics_listen = \"off\"\n[tls]\nmode = \"off\"\n",
            database_url(),
            canvas_cloud_core::ids::b64url(&[9u8; 32]),
        ),
    )
    .unwrap();
    let pw = dir.join("pw");
    std::fs::write(&pw, "correct horse battery\n").unwrap();
    cli(&config, &["migrate"]).unwrap();

    let email = format!("cli-{}@example.test", Uuid::now_v7().simple());
    let pw_path = pw.to_str().unwrap();
    cli(
        &config,
        &[
            "admin",
            "users",
            "create",
            "--email",
            &email,
            "--password-file",
            pw_path,
            "--display-name",
            "Cli",
        ],
    )
    .unwrap();
    assert!(
        cli(
            &config,
            &[
                "admin",
                "users",
                "create",
                "--email",
                &email,
                "--password-file",
                pw_path
            ]
        )
        .is_err(),
        "duplicate email"
    );
    std::fs::write(&pw, "short\n").unwrap();
    assert!(
        cli(
            &config,
            &[
                "admin",
                "users",
                "create",
                "--email",
                "x@example.test",
                "--password-file",
                pw_path
            ]
        )
        .is_err(),
        "weak password"
    );
    cli(&config, &["admin", "users", "list"]).unwrap();
    cli(&config, &["admin", "users", "disable", "--email", &email]).unwrap();
    cli(&config, &["admin", "users", "enable", "--email", &email]).unwrap();
    std::fs::write(&pw, "another long password\n").unwrap();
    cli(
        &config,
        &[
            "admin",
            "users",
            "set-password",
            "--email",
            &email,
            "--password-file",
            pw_path,
        ],
    )
    .unwrap();
    cli(
        &config,
        &[
            "admin",
            "settings",
            "set",
            "signup_domains",
            "Example.test, b.co",
        ],
    )
    .unwrap();
    cli(&config, &["admin", "settings", "get"]).unwrap();
    cli(
        &config,
        &["apps", "create", "--name", "Cli app", "--allow-import"],
    )
    .unwrap();
    cli(&config, &["apps", "list"]).unwrap();
    assert!(cli(&config, &["apps", "revoke", &Uuid::now_v7().to_string()]).is_err());
    assert!(cli(
        &config,
        &[
            "apps",
            "set",
            &Uuid::now_v7().to_string(),
            "--allow-import",
            "false"
        ]
    )
    .is_err());

    // Check the effects in the database.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let pool = sqlx::PgPool::connect(&database_url()).await.unwrap();
        let (disabled, has_pw, name): (bool, bool, String) = sqlx::query_as(
            "SELECT disabled, password_hash IS NOT NULL, display_name FROM users WHERE lower(email) = lower($1)",
        )
        .bind(&email)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((disabled, has_pw, name.as_str()), (false, true, "Cli"));
        let hash: String =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE lower(email) = lower($1)")
                .bind(&email)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(identity::verify_password("another long password".into(), Some(hash))
            .await
            .unwrap());
        let domains = identity::get_setting(&pool, "signup_domains").await.unwrap();
        assert_eq!(domains, Some(json!(["example.test", "b.co"])));
        let audits: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM admin_audit WHERE actor = 'cli' AND action = 'user.create' AND detail->>'email' = $1",
        )
        .bind(email.to_lowercase())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(audits, 1);
        identity::set_setting(&pool, "signup_domains", &json!([]))
            .await
            .unwrap();
    });
    // `apps set` flips an app's policy.
    let app_id: Uuid = rt.block_on(async {
        let pool = sqlx::PgPool::connect(&database_url()).await.unwrap();
        sqlx::query_scalar(
            "SELECT id FROM apps WHERE name = 'Cli app' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    });
    let id = app_id.to_string();
    cli(&config, &["apps", "set", &id, "--allow-import", "false"]).unwrap();
    cli(&config, &["apps", "set", &id, "--trust-email", "false"]).unwrap();
    assert!(
        cli(&config, &["apps", "set", &id]).is_err(),
        "nothing to set"
    );
    rt.block_on(async {
        let pool = sqlx::PgPool::connect(&database_url()).await.unwrap();
        let flags: (bool, bool) =
            sqlx::query_as("SELECT allow_import, trust_email FROM apps WHERE id = $1")
                .bind(app_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(flags, (false, false));
    });
    cli(
        &config,
        &["admin", "users", "delete", "--email", &email, "--yes"],
    )
    .unwrap();
    assert!(cli(&config, &["admin", "users", "disable", "--email", &email]).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
