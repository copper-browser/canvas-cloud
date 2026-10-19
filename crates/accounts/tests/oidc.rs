//! OIDC sign-in end to end against an in-process mock provider: discovery, PKCE + state +
//! nonce, client authentication, `id_token` verification (RS256 / ES256 and the ways a token
//! can be wrong), user linking / creation rules and every error redirect.

#![allow(clippy::too_many_lines)]

mod common;

use std::collections::HashMap;
use std::path::PathBuf;

use canvas_cloud_core::config::{Config, OidcProvider};
use common::idp::{MockIdp, Signer, CLIENT_ID, CLIENT_SECRET, PUBLIC_CLIENT_ID};
use common::{unique_email, TestApp, PASSWORD};
use reqwest::StatusCode;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

fn secret_file() -> PathBuf {
    let path = std::env::temp_dir().join(format!("cvc-oidc-secret-{}", Uuid::now_v7().simple()));
    std::fs::write(&path, format!("{CLIENT_SECRET}\n")).unwrap();
    path
}

fn provider(id: &str, idp: &MockIdp, public: bool, domains: &[&str]) -> OidcProvider {
    let mut v = json!({
        "id": id,
        "name": format!("Test {id}"),
        "issuer": idp.issuer,
        "client_id": if public { PUBLIC_CLIENT_ID } else { CLIENT_ID },
        "allowed_domains": domains,
        "redirect_uris": ["cvcapp://signed-in"],
    });
    if !public {
        v["client_secret_file"] = json!(secret_file());
    }
    serde_json::from_value(v).unwrap()
}

/// A server with providers `corp` (confidential, `corp.test` only), `pub` (public client,
/// any domain) and `down` (unreachable issuer).
async fn setup() -> (TestApp, MockIdp) {
    let idp = MockIdp::start().await;
    let corp = provider("corp", &idp, false, &["corp.test"]);
    let public = provider("pub", &idp, true, &[]);
    let mut down: OidcProvider = provider("down", &idp, true, &[]);
    down.issuer = "http://127.0.0.1:9".into();
    let app = TestApp::with(|c: &mut Config| {
        c.oidc = vec![corp, public, down];
    })
    .await;
    (app, idp)
}

struct Started {
    authorize: Url,
    state: String,
    cookie: String,
}

async fn start(app: &TestApp, provider: &str, redirect_uri: &str) -> Started {
    let resp = app
        .http
        .get(app.url(&format!("/v1/auth/oidc/{provider}/start")))
        .query(&[("redirect_uri", redirect_uri)])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    let set_cookie = resp.headers()["set-cookie"].to_str().unwrap().to_owned();
    let authorize = Url::parse(resp.headers()["location"].to_str().unwrap()).unwrap();
    let q: HashMap<String, String> = authorize.query_pairs().into_owned().collect();
    let state = q["state"].clone();
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    assert_eq!(cookie, format!("cvc_oidc={state}"));
    Started {
        authorize,
        state,
        cookie,
    }
}

/// The callback; returns `(status, location fragment as a map, body)`.
async fn callback(
    app: &TestApp,
    provider: &str,
    query: &[(&str, &str)],
    cookie: Option<&str>,
) -> (StatusCode, HashMap<String, String>, String, Option<Url>) {
    let mut req = app
        .http
        .get(app.url(&format!("/v1/auth/oidc/{provider}/callback")))
        .query(query);
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get("location")
        .map(|l| Url::parse(l.to_str().unwrap()).unwrap());
    let fragment: HashMap<String, String> = location
        .as_ref()
        .and_then(|l| l.fragment())
        .map(|f| {
            url::form_urlencoded::parse(f.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    (status, fragment, resp.text().await.unwrap(), location)
}

/// Start → provider issues a code with `claims` → callback. Returns the fragment.
async fn sign_in(
    app: &TestApp,
    idp: &MockIdp,
    provider: &str,
    claims: &Value,
    signer: Signer,
) -> HashMap<String, String> {
    let redirect = app.url("/account");
    let s = start(app, provider, &redirect).await;
    let code = idp.issue(&s.authorize, claims, signer);
    let (status, fragment, _, location) = callback(
        app,
        provider,
        &[("code", &code), ("state", &s.state)],
        Some(&s.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    let location = location.unwrap();
    assert_eq!(location.path(), "/account", "{location}");
    fragment
}

#[tokio::test]
async fn providers_are_listed() {
    let (app, _idp) = setup().await;
    let (s, v) = app
        .call(reqwest::Method::GET, "/v1/auth/oidc", None, None)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["providers"][0],
        json!({ "id": "corp", "name": "Test corp", "start": "/v1/auth/oidc/corp/start" })
    );
    assert_eq!(v["providers"].as_array().unwrap().len(), 3);
    let (_, info) = app.call(reqwest::Method::GET, "/v1/info", None, None).await;
    assert!(info["auth"]["oidc"].to_string().contains("corp"), "{info}");
}

#[tokio::test]
async fn start_checks_the_request() {
    let (app, _idp) = setup().await;
    let (s, v) = app
        .call(
            reqwest::Method::GET,
            "/v1/auth/oidc/nope/start?redirect_uri=x",
            None,
            None,
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("unknown_provider"))
    );
    for bad in [
        "",
        "https://evil.example/cb",
        "cvcapp://signed-in/extra",
        &format!("{}/account#frag", app.base),
        "javascript:alert(1)",
    ] {
        let resp = app
            .http
            .get(app.url("/v1/auth/oidc/corp/start"))
            .query(&[("redirect_uri", bad)])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad}");
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["error"], "redirect_uri_not_allowed");
    }
    let resp = app
        .http
        .get(app.url("/v1/auth/oidc/down/start"))
        .query(&[("redirect_uri", app.url("/account"))])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"], "oidc_unavailable");

    // A registered (non-http) redirect works; the authorization request is complete.
    let s = start(&app, "corp", "cvcapp://signed-in").await;
    let resp = app
        .http
        .get(app.url("/v1/auth/oidc/corp/start"))
        .query(&[("redirect_uri", "cvcapp://signed-in")])
        .send()
        .await
        .unwrap();
    let cookie = resp.headers()["set-cookie"].to_str().unwrap();
    for attr in [
        "HttpOnly",
        "SameSite=Lax",
        "Path=/v1/auth/oidc/",
        "Max-Age=600",
    ] {
        assert!(cookie.contains(attr), "{cookie}");
    }
    assert!(!cookie.contains("Secure"), "http public_url");
    let q: HashMap<String, String> = s.authorize.query_pairs().into_owned().collect();
    assert_eq!(s.authorize.path(), "/authorize");
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["redirect_uri"], app.url("/v1/auth/oidc/corp/callback"));
    assert_eq!(q["scope"], "openid email profile");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["code_challenge"].len(), 43);
    assert_eq!(q["nonce"].len(), 43);
    assert_ne!(q["nonce"], q["state"]);
    // Only a digest of the state is stored.
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM oidc_states WHERE state_sha256 = sha256(convert_to($1, 'UTF8'))",
    )
    .bind(&s.state)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(stored, 1);
}

#[tokio::test]
async fn confidential_client_es256_creates_a_user_in_an_allowed_domain() {
    let (app, idp) = setup().await;
    let email = unique_email("Olga", "Corp.Test");
    let claims = json!({ "email": email, "name": "Olga O." });
    let f = sign_in(&app, &idp, "corp", &claims, Signer::Es256).await;
    let token = f.get("token").unwrap_or_else(|| panic!("{f:?}"));
    assert!(token.starts_with("cvs_"));
    let (s, me) = app.get("/v1/auth/me", token).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["user"]["email"], email.to_lowercase());
    assert_eq!(me["user"]["display_name"], "Olga O.");
    assert_eq!(me["via"]["kind"], "session");
    let id = Uuid::parse_str(me["user"]["id"].as_str().unwrap()).unwrap();
    let hash: Option<String> = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
        .bind(id)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert!(hash.is_none(), "OIDC users have no password");
    assert_eq!(app.login(&email, "").await.0, StatusCode::UNAUTHORIZED);
    assert!(idp.auth_seen().contains(&"basic".to_owned()));

    // Again: the same user, a new session.
    let f = sign_in(&app, &idp, "corp", &claims, Signer::Es256).await;
    let (_, me2) = app.get("/v1/auth/me", &f["token"]).await;
    assert_eq!(me2["user"]["id"], me["user"]["id"]);
    // States are single use and swept.
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM oidc_states WHERE expires_at < now()")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn public_client_rs256_links_an_existing_password_account() {
    let (app, idp) = setup().await;
    let pia = app.user_in("pia", "anywhere.test").await;
    let f = sign_in(
        &app,
        &idp,
        "pub",
        &json!({ "email": pia.email.to_uppercase(), "email_verified": "true" }),
        Signer::Rs256,
    )
    .await;
    let (s, me) = app
        .get(
            "/v1/auth/me",
            f.get("token").unwrap_or_else(|| panic!("{f:?}")),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["user"]["id"], json!(pia.id), "same users row");
    assert_eq!(me["user"]["display_name"], "pia", "existing name kept");
    assert!(idp.auth_seen().contains(&"none".to_owned()));
    // The password still works too.
    assert_eq!(app.login(&pia.email, PASSWORD).await.0, StatusCode::OK);
}

#[tokio::test]
async fn bad_identities_are_refused() {
    let (app, idp) = setup().await;
    let email = unique_email("q", "corp.test");
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let cases: Vec<(&str, Value, Signer, &str)> = vec![
        (
            "nonce",
            json!({ "email": email, "nonce": "x".repeat(43) }),
            Signer::Es256,
            "nonce_mismatch",
        ),
        (
            "no nonce",
            json!({ "email": email, "nonce": null }),
            Signer::Es256,
            "nonce_mismatch",
        ),
        (
            "aud",
            json!({ "email": email, "aud": "someone-else" }),
            Signer::Es256,
            "invalid_id_token",
        ),
        (
            "azp",
            json!({ "email": email, "aud": [CLIENT_ID, "other"], "azp": "other" }),
            Signer::Es256,
            "invalid_id_token",
        ),
        (
            "multi-aud",
            json!({ "email": email, "aud": [CLIENT_ID, "other"] }),
            Signer::Es256,
            "invalid_id_token",
        ),
        (
            "iss",
            json!({ "email": email, "iss": "https://evil.example" }),
            Signer::Es256,
            "invalid_id_token",
        ),
        (
            "exp",
            json!({ "email": email, "exp": now - 600 }),
            Signer::Es256,
            "invalid_id_token",
        ),
        (
            "nbf",
            json!({ "email": email, "nbf": now + 600 }),
            Signer::Es256,
            "invalid_id_token",
        ),
        (
            "iat",
            json!({ "email": email, "iat": now + 3600 }),
            Signer::Es256,
            "invalid_id_token",
        ),
        (
            "rogue key",
            json!({ "email": email }),
            Signer::Rogue,
            "invalid_id_token",
        ),
        (
            "hs256",
            json!({ "email": email }),
            Signer::Hs256,
            "invalid_id_token",
        ),
        (
            "unverified",
            json!({ "email": email, "email_verified": false }),
            Signer::Es256,
            "email_not_verified",
        ),
        (
            "unverified str",
            json!({ "email": email, "email_verified": "false" }),
            Signer::Es256,
            "email_not_verified",
        ),
        (
            "no verified",
            json!({ "email": email, "email_verified": null }),
            Signer::Es256,
            "email_not_verified",
        ),
        ("no email", json!({}), Signer::Es256, "email_missing"),
        (
            "domain",
            json!({ "email": unique_email("q", "elsewhere.test") }),
            Signer::Es256,
            "domain_not_allowed",
        ),
        (
            "subdomain",
            json!({ "email": unique_email("q", "x.corp.test") }),
            Signer::Es256,
            "domain_not_allowed",
        ),
    ];
    for (what, claims, signer, want) in cases {
        let f = sign_in(&app, &idp, "corp", &claims, signer).await;
        assert_eq!(
            f.get("error").map(String::as_str),
            Some(want),
            "{what}: {f:?}"
        );
        assert!(!f.contains_key("token"), "{what}");
        assert_ne!(f["error_description"], "");
    }
    // Nothing was created along the way.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email = $1")
        .bind(&email)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(n, 0);
    // The azp check accepts a matching azp.
    let f = sign_in(
        &app,
        &idp,
        "corp",
        &json!({ "email": email, "aud": [CLIENT_ID, "other"], "azp": CLIENT_ID }),
        Signer::Es256,
    )
    .await;
    assert!(f.contains_key("token"), "{f:?}");
}

#[tokio::test]
async fn account_rules() {
    let (app, idp) = setup().await;
    // Self-signup is off (the default) and `pub` names no domains → no new accounts.
    let f = sign_in(
        &app,
        &idp,
        "pub",
        &json!({ "email": unique_email("r", "x.test") }),
        Signer::Es256,
    )
    .await;
    assert_eq!(f["error"], "signup_closed");
    // Disabled accounts cannot sign in.
    let rex = app.user_in("rex", "corp.test").await;
    sqlx::query("UPDATE users SET disabled = true WHERE id = $1")
        .bind(rex.id)
        .execute(app.db())
        .await
        .unwrap();
    let f = sign_in(
        &app,
        &idp,
        "corp",
        &json!({ "email": rex.email }),
        Signer::Es256,
    )
    .await;
    assert_eq!(f["error"], "account_disabled");
    // An app-provisioned (password-less) user in an allowed domain signs in as themselves.
    let email = unique_email("sal", "corp.test");
    let sal = app.federated_user(&email).await;
    let f = sign_in(
        &app,
        &idp,
        "corp",
        &json!({ "email": email, "name": "ignored" }),
        Signer::Es256,
    )
    .await;
    let (_, me) = app.get("/v1/auth/me", &f["token"]).await;
    assert_eq!(me["user"]["id"], json!(sal));
}

#[tokio::test]
async fn callback_failures() {
    let (app, idp) = setup().await;
    let redirect = app.url("/account");

    // The provider says no.
    let s = start(&app, "corp", &redirect).await;
    let (st, f, _, _) = callback(
        &app,
        "corp",
        &[("error", "access_denied"), ("state", &s.state)],
        Some(&s.cookie),
    )
    .await;
    assert_eq!(st, StatusCode::FOUND);
    assert_eq!(f["error"], "access_denied");

    // The token endpoint refuses the code.
    let s = start(&app, "corp", &redirect).await;
    let code = idp.issue_failing(&s.authorize);
    let (_, f, _, _) = callback(
        &app,
        "corp",
        &[("code", &code), ("state", &s.state)],
        Some(&s.cookie),
    )
    .await;
    assert_eq!(f["error"], "token_exchange_failed");

    // No code.
    let s = start(&app, "corp", &redirect).await;
    let (_, f, _, _) = callback(&app, "corp", &[("state", &s.state)], Some(&s.cookie)).await;
    assert_eq!(f["error"], "invalid_request");

    // Another browser (no / wrong cookie) — the state is burned either way.
    let s = start(&app, "corp", &redirect).await;
    let code = idp.issue(
        &s.authorize,
        &json!({ "email": unique_email("t", "corp.test") }),
        Signer::Es256,
    );
    let (_, f, _, _) = callback(&app, "corp", &[("code", &code), ("state", &s.state)], None).await;
    assert_eq!(f["error"], "state_mismatch");
    let (st, f, body, loc) = callback(
        &app,
        "corp",
        &[("code", &code), ("state", &s.state)],
        Some(&s.cookie),
    )
    .await;
    assert_eq!(
        (st, f.len(), loc),
        (StatusCode::BAD_REQUEST, 0, None),
        "replayed state"
    );
    assert!(body.contains("Sign-in failed"));
    let s2 = start(&app, "corp", &redirect).await;
    let (_, f, _, _) = callback(
        &app,
        "corp",
        &[("code", "c"), ("state", &s2.state)],
        Some(&s.cookie),
    )
    .await;
    assert_eq!(f["error"], "state_mismatch");

    // The state belongs to another provider.
    let s = start(&app, "corp", &redirect).await;
    let (st, _, _, _) = callback(
        &app,
        "pub",
        &[("code", "c"), ("state", &s.state)],
        Some(&s.cookie),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Unknown / malformed state: an HTML page under a strict CSP (no redirect target known).
    for state in ["", "short", &"A".repeat(43)] {
        let resp = app
            .http
            .get(app.url("/v1/auth/oidc/corp/callback"))
            .query(&[("code", "c"), ("state", state)])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{state}");
        assert!(resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        assert!(resp.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .starts_with("default-src 'none'"));
    }

    // Expired.
    let s = start(&app, "corp", &redirect).await;
    sqlx::query("UPDATE oidc_states SET expires_at = now() - interval '1 second' WHERE state_sha256 = sha256(convert_to($1, 'UTF8'))")
        .bind(&s.state)
        .execute(app.db())
        .await
        .unwrap();
    let (_, f, _, _) = callback(
        &app,
        "corp",
        &[("code", "c"), ("state", &s.state)],
        Some(&s.cookie),
    )
    .await;
    assert_eq!(f["error"], "state_expired");

    // Unknown provider on the callback.
    let (st, _, _, _) = callback(&app, "nope", &[("state", &s.state)], None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Results go to a registered custom-scheme redirect as well.
    let s = start(&app, "corp", "cvcapp://signed-in").await;
    let code = idp.issue(
        &s.authorize,
        &json!({ "email": unique_email("u", "corp.test") }),
        Signer::Es256,
    );
    let (_, f, _, loc) = callback(
        &app,
        "corp",
        &[("code", &code), ("state", &s.state)],
        Some(&s.cookie),
    )
    .await;
    assert_eq!(loc.unwrap().scheme(), "cvcapp");
    assert!(f["token"].starts_with("cvs_"));
}
