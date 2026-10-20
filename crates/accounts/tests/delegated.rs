//! `[[delegated_sign_in]]` providers: listed after the `[[oidc]]` ones (with `kind`), and
//! `GET /v1/auth/oidc/{id}/start` checks `redirect_uri` and hands the browser to the
//! delegate's `start_url` (which mints the session through a linked app; see the apps and
//! binary tests for that half).

mod common;

use canvas_cloud_core::config::{Config, DelegatedSignIn, OidcProvider};
use common::TestApp;
use reqwest::StatusCode;
use serde_json::{json, Value};
use url::Url;

const START_URL: &str = "https://portal.example.test/canvas-signin?from=canvas";

async fn setup() -> TestApp {
    let oidc: OidcProvider = serde_json::from_value(json!({
        "id": "corp",
        "name": "Corp",
        "issuer": "https://sso.example.test",
        "client_id": "c",
    }))
    .unwrap();
    let portal: DelegatedSignIn = serde_json::from_value(json!({
        "id": "portal",
        "name": "Team portal",
        "start_url": START_URL,
    }))
    .unwrap();
    let bare: DelegatedSignIn = serde_json::from_value(json!({
        "id": "bare",
        "start_url": "http://127.0.0.1:9/signin",
    }))
    .unwrap();
    TestApp::with(|c: &mut Config| {
        c.oidc = vec![oidc];
        c.delegated_sign_in = vec![portal, bare];
    })
    .await
}

#[tokio::test]
async fn delegated_providers_are_listed_after_oidc() {
    let app = setup().await;
    let want = json!([
        { "id": "corp", "name": "Corp", "start": "/v1/auth/oidc/corp/start", "kind": "oidc" },
        {
            "id": "portal",
            "name": "Team portal",
            "start": "/v1/auth/oidc/portal/start",
            "kind": "delegated",
            "start_url": START_URL,
        },
        {
            "id": "bare",
            "name": "bare",
            "start": "/v1/auth/oidc/bare/start",
            "kind": "delegated",
            "start_url": "http://127.0.0.1:9/signin",
        },
    ]);
    let (s, v) = app
        .call(reqwest::Method::GET, "/v1/auth/oidc", None, None)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["providers"], want);
    let (s, info) = app.call(reqwest::Method::GET, "/v1/info", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(info["auth"]["oidc"], want);
}

async fn start(app: &TestApp, id: &str, redirect_uri: Option<&str>) -> reqwest::Response {
    let mut req = app.http.get(app.url(&format!("/v1/auth/oidc/{id}/start")));
    if let Some(r) = redirect_uri {
        req = req.query(&[("redirect_uri", r)]);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn start_hands_the_browser_to_the_delegate() {
    let app = setup().await;
    let back = app.url("/account");
    let resp = start(&app, "portal", Some(&back)).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let h = resp.headers();
    assert_eq!(h["cache-control"], "no-store");
    assert_eq!(h["referrer-policy"], "no-referrer");
    assert!(h.get("set-cookie").is_none(), "no state is kept");
    let to = Url::parse(h["location"].to_str().unwrap()).unwrap();
    assert_eq!(
        (to.scheme(), to.host_str(), to.path()),
        ("https", Some("portal.example.test"), "/canvas-signin")
    );
    let q: Vec<(String, String)> = to.query_pairs().into_owned().collect();
    assert_eq!(
        q,
        [
            ("from".to_owned(), "canvas".to_owned()),
            ("redirect_uri".to_owned(), back.clone()),
        ],
        "the start_url's own query is kept"
    );
    assert!(to.fragment().is_none());

    // The web app at the root is a fine place to come back to.
    let root = app.url("/");
    let resp = start(&app, "bare", Some(&root)).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let to = Url::parse(resp.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(
        to.as_str(),
        format!(
            "http://127.0.0.1:9/signin?redirect_uri={}",
            url::form_urlencoded::byte_serialize(root.as_bytes()).collect::<String>()
        )
    );
}

#[tokio::test]
async fn start_only_returns_to_this_server() {
    let app = setup().await;
    let other_port = format!("http://127.0.0.1:{}/account", app.addr.port() ^ 1);
    let with_creds = app.base.replace("http://", "http://u:p@") + "/account";
    for bad in [
        String::new(),
        "https://evil.example/cb".to_owned(),
        "cvcapp://signed-in".to_owned(),
        app.url("/account#token=x"),
        other_port,
        with_creds,
        "javascript:alert(1)".to_owned(),
    ] {
        let resp = start(&app, "portal", Some(&bad)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad}");
        assert!(resp.headers().get("location").is_none());
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["error"], "redirect_uri_not_allowed", "{bad}");
    }
    let resp = start(&app, "portal", None).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delegated_providers_have_no_callback() {
    let app = setup().await;
    let (s, v) = app
        .call(
            reqwest::Method::GET,
            "/v1/auth/oidc/portal/callback?code=x&state=y",
            None,
            None,
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("unknown_provider"))
    );
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
}
