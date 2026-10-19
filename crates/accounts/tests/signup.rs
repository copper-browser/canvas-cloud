//! The signup policy, which depends on instance-wide state (whether any password account
//! exists, the `allow_signup` / `signup_domains` settings). Runs on its own database
//! (`CANVAS_CLOUD_TEST_ACCOUNTS_FRESH_DATABASE_URL`, default
//! `postgres://farce@localhost:5433/cvc_accounts_fresh_test`) whose schema is dropped and
//! recreated at the start, with every test serialized and starting from an empty `users`.

#![allow(clippy::too_many_lines)]

mod common;

use canvas_cloud_core::identity::set_setting;
use common::idp::{MockIdp, Signer};
use common::{unique_email, TestApp, PASSWORD};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

fn database_url() -> String {
    std::env::var("CANVAS_CLOUD_TEST_ACCOUNTS_FRESH_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://farce@localhost:5433/cvc_accounts_fresh_test".into())
}

/// A server over an emptied database (no users, default settings).
async fn empty(f: impl FnOnce(&mut canvas_cloud_core::config::Config)) -> TestApp {
    let app = TestApp::fresh(&database_url(), f).await;
    sqlx::raw_sql("TRUNCATE users, server_settings, oidc_states CASCADE")
        .execute(app.db())
        .await
        .unwrap();
    app
}

async fn signup(app: &TestApp, email: &str) -> (StatusCode, Value) {
    app.post(
        "/v1/auth/signup",
        None,
        json!({ "email": email, "password": PASSWORD, "display_name": "New Person" }),
    )
    .await
}

async fn info_signup(app: &TestApp) -> bool {
    let (_, v) = app.call(reqwest::Method::GET, "/v1/info", None, None).await;
    v["signup"].as_bool().unwrap()
}

#[tokio::test]
async fn first_account_then_closed_then_open_then_domains() {
    let _serial = SERIAL.lock().await;
    let app = empty(|_| {}).await;
    assert!(info_signup(&app).await, "first user may sign up");

    // An app-provisioned (password-less) user does not count as "the first account".
    let fed = unique_email("fed", "example.test");
    app.federated_user(&fed).await;
    assert!(info_signup(&app).await);
    // …and is never claimed by a signup with its email.
    let (s, v) = signup(&app, &fed).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("email_taken")),
        "{v}"
    );

    // The first password account: 201 {token, user}.
    let first = unique_email("First", "Example.TEST");
    let (s, v) = signup(&app, &format!("  {first} ")).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert!(v["token"].as_str().unwrap().starts_with("cvs_"));
    assert_eq!(v["user"]["email"], first.to_lowercase());
    assert_eq!(v["user"]["display_name"], "New Person");
    assert_eq!(v["user"].as_object().unwrap().len(), 4);
    let (s, me) = app.get("/v1/auth/me", v["token"].as_str().unwrap()).await;
    assert_eq!(
        (s, me["via"]["kind"].as_str()),
        (StatusCode::OK, Some("session"))
    );
    assert_eq!(app.login(&first, PASSWORD).await.0, StatusCode::OK);

    // Now closed (allow_signup defaults to false).
    assert!(!info_signup(&app).await);
    let (s, v) = signup(&app, &unique_email("second", "example.test")).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("signup_closed"))
    );
    // Taken emails are a 409 only once the policy lets the caller sign up at all.
    let (s, v) = signup(&app, &first).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("signup_closed"))
    );

    // Open.
    set_setting(app.db(), "allow_signup", &json!(true))
        .await
        .unwrap();
    assert!(info_signup(&app).await);
    let (s, _) = signup(&app, &unique_email("second", "example.test")).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = signup(&app, &first.to_uppercase()).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("email_taken"))
    );

    // Narrowed to domains.
    set_setting(
        app.db(),
        "signup_domains",
        &json!(["@Corp.Test", "ok.test"]),
    )
    .await
    .unwrap();
    let (s, v) = signup(&app, &unique_email("third", "elsewhere.test")).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("signup_domain_not_allowed"))
    );
    let (s, _) = signup(&app, &unique_email("third", "sub.corp.test")).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "subdomains are not included");
    for domain in ["corp.test", "OK.test"] {
        let (s, v) = signup(&app, &unique_email("third", domain)).await;
        assert_eq!(s, StatusCode::CREATED, "{domain}: {v}");
    }

    // Validation (whatever the policy).
    for (body, what) in [
        (
            json!({ "email": "not-an-email", "password": PASSWORD }),
            "email",
        ),
        (
            json!({ "email": unique_email("v", "ok.test"), "password": "short" }),
            "short password",
        ),
        (
            json!({ "email": unique_email("v", "ok.test"), "password": "x".repeat(257) }),
            "long password",
        ),
        (
            json!({ "email": unique_email("v", "ok.test") }),
            "no password",
        ),
    ] {
        let (s, v) = app.post("/v1/auth/signup", None, body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {v}");
    }
    let email = unique_email("v", "ok.test");
    let (s, v) = app
        .post(
            "/v1/auth/signup",
            None,
            json!({ "email": email, "password": "x".repeat(256), "display_name": "   " }),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "256 characters is fine: {v}");
    // A blank display name falls back to the email's local part (core's default).
    assert_eq!(v["user"]["display_name"], email.split('@').next().unwrap());
}

#[tokio::test]
async fn concurrent_first_signups_admit_exactly_one() {
    let _serial = SERIAL.lock().await;
    let app = empty(|_| {}).await;
    let mut tasks = Vec::new();
    for i in 0..6 {
        let http = app.http.clone();
        let url = app.url("/v1/auth/signup");
        let email = unique_email(&format!("race{i}"), "example.test");
        tasks.push(tokio::spawn(async move {
            http.post(url)
                .json(&json!({ "email": email, "password": PASSWORD }))
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    let mut statuses = Vec::new();
    for t in tasks {
        statuses.push(t.await.unwrap());
    }
    let created = statuses
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();
    assert_eq!(created, 1, "{statuses:?}");
    assert!(statuses
        .iter()
        .all(|s| *s == StatusCode::CREATED || *s == StatusCode::FORBIDDEN));
}

#[tokio::test]
async fn oidc_signup_follows_the_settings_but_not_the_first_user_rule() {
    let _serial = SERIAL.lock().await;
    let idp = MockIdp::start().await;
    let provider: canvas_cloud_core::config::OidcProvider = serde_json::from_value(json!({
        "id": "pub",
        "issuer": idp.issuer,
        "client_id": common::idp::PUBLIC_CLIENT_ID,
    }))
    .unwrap();
    let app = empty(|c| c.oidc = vec![provider]).await;

    let sign_in = |email: String| {
        let app = &app;
        let idp = &idp;
        async move {
            let resp = app
                .http
                .get(app.url("/v1/auth/oidc/pub/start"))
                .query(&[("redirect_uri", app.url("/account"))])
                .send()
                .await
                .unwrap();
            let cookie = resp.headers()["set-cookie"]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            let authorize = url::Url::parse(resp.headers()["location"].to_str().unwrap()).unwrap();
            let state = authorize
                .query_pairs()
                .find(|(k, _)| k == "state")
                .unwrap()
                .1
                .into_owned();
            let code = idp.issue(&authorize, &json!({ "email": email }), Signer::Es256);
            let resp = app
                .http
                .get(app.url("/v1/auth/oidc/pub/callback"))
                .query(&[("code", code.as_str()), ("state", state.as_str())])
                .header("cookie", cookie)
                .send()
                .await
                .unwrap();
            let loc = url::Url::parse(resp.headers()["location"].to_str().unwrap()).unwrap();
            url::form_urlencoded::parse(loc.fragment().unwrap().as_bytes())
                .into_owned()
                .collect::<std::collections::HashMap<String, String>>()
        }
    };

    // Empty instance: OIDC does not get the first-account pass.
    let f = sign_in(unique_email("a", "example.test")).await;
    assert_eq!(f["error"], "signup_closed");
    set_setting(app.db(), "allow_signup", &json!(true))
        .await
        .unwrap();
    set_setting(app.db(), "signup_domains", &json!(["ok.test"]))
        .await
        .unwrap();
    let f = sign_in(unique_email("a", "example.test")).await;
    assert_eq!(f["error"], "signup_closed");
    let f = sign_in(unique_email("a", "ok.test")).await;
    assert!(f["token"].starts_with("cvs_"), "{f:?}");
    // That (password-less) OIDC account does not use up password signup's "first account".
    set_setting(app.db(), "allow_signup", &json!(false))
        .await
        .unwrap();
    assert!(info_signup(&app).await);
    let (s, v) = signup(&app, &unique_email("first", "anywhere.test")).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}
