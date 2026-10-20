//! Router composition, `GET /v1/info`, the static web root, the auth rate limiter and the
//! CORS layer.

use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use serde_json::{json, Map, Value};
use tower::ServiceExt as _;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::error::ApiError;
use crate::state::SharedState;

/// Optional capabilities core itself advertises in `GET /v1/info` `features` (crates add
/// theirs through [`Mounts::features`]). Only ever grows.
pub const CORE_FEATURES: &[&str] = &[
    // `X-Canvas-User`: linked apps act for their users.
    "app_users",
];

/// Everything the crates contribute to the app, composed by [`app_with`].
///
/// Routers must not set their own fallback (core owns the JSON 404s).
#[derive(Default)]
pub struct Mounts {
    /// Merged under `/v1` (paths relative to `/v1`): engine, apps, accounts, …
    pub v1: Vec<Router<SharedState>>,
    /// Merged at the root: `/mcp`, `/llms.txt`, the `/join/{token}` landing page, …
    pub root: Vec<Router<SharedState>>,
    /// Extra `GET /v1/info` `features`.
    pub features: Vec<&'static str>,
    /// Extra `GET /v1/info` `limits` entries.
    pub limits: Map<String, Value>,
}

impl Mounts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a router under `/v1`.
    #[must_use]
    pub fn v1(mut self, router: Router<SharedState>) -> Self {
        self.v1.push(router);
        self
    }

    /// Add a router at the root.
    #[must_use]
    pub fn root(mut self, router: Router<SharedState>) -> Self {
        self.root.push(router);
        self
    }

    /// Advertise features in `GET /v1/info`.
    #[must_use]
    pub fn features(mut self, features: &[&'static str]) -> Self {
        self.features.extend_from_slice(features);
        self
    }

    /// Advertise a limit in `GET /v1/info` `limits`.
    #[must_use]
    pub fn limit(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.limits.insert(key.to_owned(), value.into());
        self
    }
}

struct InfoExtras {
    features: Vec<&'static str>,
    limits: Map<String, Value>,
}

/// The complete HTTP app:
///
/// * `GET /healthz` → `ok`;
/// * `/v1`: core `GET /v1/info` plus every [`Mounts::v1`] router; unknown `/v1` paths are a
///   JSON 404;
/// * every [`Mounts::root`] router at the root;
/// * with `web_root` configured, its files for any other GET/HEAD (see [`serve_web_root`]);
///   anything else is a JSON 404;
///
/// wrapped (innermost first) in the per-IP auth rate limiter, the CORS layer (only when
/// `cors.allowed_origins` is non-empty) and request tracing/metrics (outermost).
pub fn app_with(state: SharedState, mounts: Mounts) -> Router {
    let mut features: Vec<&'static str> = CORE_FEATURES.to_vec();
    for f in mounts.features {
        if !features.contains(&f) {
            features.push(f);
        }
    }
    let extras = Arc::new(InfoExtras {
        features,
        limits: mounts.limits,
    });
    let mut v1 = Router::new().route(
        "/info",
        get(move |State(state): State<SharedState>| info(state, extras.clone())),
    );
    for r in mounts.v1 {
        v1 = v1.merge(r);
    }
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .nest("/v1", v1.fallback(not_found));
    for r in mounts.root {
        router = router.merge(r);
    }
    let limiter = Arc::new(AuthRateLimiter::new(
        state.cfg.limits.auth_per_minute,
        state.cfg.trust_proxy,
    ));
    let router = match &state.cfg.web_root {
        Some(root) => {
            let dir = ServeDir::new(root).append_index_html_on_directories(true);
            router.fallback(move |req: Request| serve_web_root(dir.clone(), req))
        }
        None => router.fallback(not_found),
    };
    let mut router = router.layer(from_fn_with_state(limiter, auth_rate_limit));
    if let Some(cors) = cors_layer(&state.cfg.cors.allowed_origins) {
        router = router.layer(cors);
    }
    router
        .layer(from_fn_with_state(state.clone(), crate::observe::track))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// JSON `404 {"error":"not_found"}`.
pub async fn not_found() -> ApiError {
    ApiError::NotFound
}

/// Paths the web root never serves (they belong to the API, the agent seams and probes).
fn reserved_path(path: &str) -> bool {
    ["/v1", "/mcp", "/healthz"].iter().any(|p| {
        path == *p
            || path
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The root fallback when `web_root` is set: GET/HEAD of any path no route claimed is served
/// from the directory (`/` and directories → their `index.html`; never a listing; `..` and
/// other escapes are refused by [`ServeDir`]; content types by extension). HTML answers carry
/// `Cache-Control: no-cache` so a redeployed app is picked up at once. Other methods, the
/// reserved prefixes and missing files are a JSON 404.
pub async fn serve_web_root(dir: ServeDir, req: Request) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD) || reserved_path(req.uri().path()) {
        return ApiError::NotFound.into_response();
    }
    let res: Result<_, Infallible> = dir.oneshot(req).await;
    let Ok(res) = res;
    if matches!(
        res.status(),
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ) {
        return ApiError::NotFound.into_response();
    }
    let mut res = res.map(Body::new);
    let is_html = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    let headers = res.headers_mut();
    if is_html {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}

/// The sign-in buttons a client offers, as `GET /v1/info` → `auth.oidc` and
/// `GET /v1/auth/oidc` → `providers` list them: every `[[oidc]]` provider
/// (`kind: "oidc"`), then every `[[delegated_sign_in]]` one (`kind: "delegated"`, plus its
/// `start_url`). `start` is always this server's `/v1/auth/oidc/{id}/start`; the client adds
/// `?redirect_uri=` and gets the result back in the fragment either way.
pub fn sign_in_providers(cfg: &Config) -> Vec<Value> {
    let start = |id: &str| format!("/v1/auth/oidc/{id}/start");
    cfg.oidc
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.label(),
                "start": start(&p.id),
                "kind": "oidc",
            })
        })
        .chain(cfg.delegated_sign_in.iter().map(|d| {
            json!({
                "id": d.id,
                "name": d.label(),
                "start": start(&d.id),
                "kind": "delegated",
                "start_url": d.start_url,
            })
        }))
        .collect()
}

/// `GET /v1/info` — public. Clients (and a web app probing its own origin) learn what this
/// server is and offers.
async fn info(state: SharedState, extras: Arc<InfoExtras>) -> Result<Json<Value>, ApiError> {
    let signup = crate::identity::signup_allowed(&state.db).await?;
    let oidc = sign_in_providers(&state.cfg);
    let mut limits = Map::new();
    limits.insert(
        "auth_per_minute".into(),
        state.cfg.limits.auth_per_minute.into(),
    );
    limits.insert(
        "import_max_bytes".into(),
        state.cfg.limits.import_max_bytes.into(),
    );
    limits.insert(
        "import_max_line_bytes".into(),
        state.cfg.limits.import_max_line_bytes.into(),
    );
    limits.insert(
        "app_sessions_per_minute".into(),
        state.cfg.limits.app_sessions_per_minute.into(),
    );
    for (k, v) in &extras.limits {
        limits.insert(k.clone(), v.clone());
    }
    Ok(Json(json!({
        "name": "canvas-cloud",
        "version": env!("CARGO_PKG_VERSION"),
        "public_url": state.cfg.public_url,
        "signup": signup,
        "auth": { "password": true, "oidc": oidc },
        "web": {
            "root": state.cfg.web_root.is_some(),
            "app_url": state.cfg.effective_web_app_url(),
        },
        "features": extras.features,
        "limits": limits,
    })))
}

// ---------------------------------------------------------------------------------------------
// CORS

/// Request headers browsers may send cross-origin.
pub const CORS_ALLOWED_HEADERS: &[&str] = &[
    "authorization",
    "content-type",
    "x-canvas-user",
    "last-event-id",
    // MCP Streamable HTTP clients running in a browser (`POST /mcp`).
    "mcp-protocol-version",
    "mcp-session-id",
];
/// How long browsers may cache a preflight answer.
pub const CORS_MAX_AGE: Duration = Duration::from_secs(600);

/// Exact-match origin allow-list (bearer tokens, so no credentials mode); `None` when the
/// list is empty (no CORS headers at all).
pub fn cors_layer(allowed_origins: &[String]) -> Option<CorsLayer> {
    let origins: Vec<HeaderValue> = allowed_origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    if origins.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([
                Method::GET,
                Method::HEAD,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
            ])
            .allow_headers(
                CORS_ALLOWED_HEADERS
                    .iter()
                    .map(|h| HeaderName::from_static(h))
                    .collect::<Vec<_>>(),
            )
            .expose_headers([header::RETRY_AFTER])
            .max_age(CORS_MAX_AGE),
    )
}

// ---------------------------------------------------------------------------------------------
// Rate limiting

/// Keyed GCRA limiter for `/v1/auth/*`: `per_minute` requests per client IP (IPv6 keyed by
/// /64 so one host cannot rotate through its prefix).
pub struct AuthRateLimiter {
    limiter: DefaultKeyedRateLimiter<IpAddr>,
    trust_proxy: bool,
    calls: AtomicU64,
}

impl AuthRateLimiter {
    pub fn new(per_minute: u32, trust_proxy: bool) -> Self {
        let n = std::num::NonZeroU32::new(per_minute.max(1)).expect("non-zero");
        Self {
            limiter: RateLimiter::keyed(Quota::per_minute(n)),
            trust_proxy,
            calls: AtomicU64::new(0),
        }
    }

    /// `true` if the request may proceed.
    pub fn check(&self, ip: IpAddr) -> bool {
        if self.calls.fetch_add(1, Ordering::Relaxed) % 4096 == 4095 {
            self.limiter.retain_recent();
            self.limiter.shrink_to_fit();
        }
        self.limiter.check_key(&rate_key(ip)).is_ok()
    }
}

fn rate_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                IpAddr::V4(v4)
            } else {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        }
    }
}

/// Whether a request counts against the per-IP auth bucket: everything under `/v1/auth/`
/// (signup, login, password, OIDC) except the bearer-authenticated status call
/// `GET /v1/auth/me`.
pub fn is_rate_limited_auth_path(method: &Method, path: &str) -> bool {
    path.starts_with("/v1/auth/") && !(path == "/v1/auth/me" && method == Method::GET)
}

async fn auth_rate_limit(
    State(limiter): State<Arc<AuthRateLimiter>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req
        .extensions()
        .get::<OriginalUri>()
        .map_or_else(|| req.uri().path(), |u| u.0.path());
    if !is_rate_limited_auth_path(req.method(), path) || req.method() == Method::OPTIONS {
        return next.run(req).await;
    }
    let ip = crate::observe::client_ip(&req, limiter.trust_proxy)
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    if limiter.check(ip) {
        next.run(req).await
    } else {
        metrics::counter!("auth_rate_limited_total").increment(1);
        tracing::warn!(%ip, "auth rate limit exceeded");
        ApiError::RateLimited.into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::post;
    use http_body_util::BodyExt as _;

    use super::*;

    #[test]
    fn limiter_counts_per_ip_and_v6_prefix() {
        let l = AuthRateLimiter::new(3, false);
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        assert!(l.check(a) && l.check(a) && l.check(a));
        assert!(!l.check(a));
        assert!(l.check(b));
        let v6a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let v6b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        assert!(l.check(v6a) && l.check(v6b) && l.check(v6a));
        assert!(!l.check(v6b), "same /64 shares a bucket");
    }

    #[test]
    fn rate_limited_paths() {
        assert!(is_rate_limited_auth_path(&Method::POST, "/v1/auth/login"));
        assert!(is_rate_limited_auth_path(&Method::POST, "/v1/auth/signup"));
        assert!(is_rate_limited_auth_path(
            &Method::GET,
            "/v1/auth/oidc/google/start"
        ));
        assert!(is_rate_limited_auth_path(&Method::PATCH, "/v1/auth/me"));
        assert!(!is_rate_limited_auth_path(&Method::GET, "/v1/auth/me"));
        assert!(!is_rate_limited_auth_path(&Method::GET, "/v1/canvases"));
        assert!(!is_rate_limited_auth_path(&Method::POST, "/v1/authx"));
    }

    fn test_state(origins: &[&str], auth_per_minute: u32) -> SharedState {
        let url = "postgres://localhost/unused";
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy(url)
            .unwrap();
        let mut cfg = crate::config::Config::for_tests(url);
        cfg.cors.allowed_origins = origins.iter().map(|s| (*s).to_owned()).collect();
        cfg.limits.auth_per_minute = auth_per_minute;
        crate::state::AppState::new(pool, cfg)
    }

    fn mounts() -> Mounts {
        Mounts::new()
            .v1(Router::new().route("/auth/login", post(|| async { "in" })))
            .root(Router::new().route("/llms.txt", get(|| async { "guide" })))
    }

    async fn body(resp: Response) -> String {
        String::from_utf8(
            resp.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn composition_healthz_root_and_json_404s() {
        let app = app_with(test_state(&[], 10), mounts());
        let r = app
            .clone()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body(r).await, "ok");
        let r = app
            .clone()
            .oneshot(Request::get("/llms.txt").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(body(r).await, "guide");
        for path in ["/v1/nope", "/nope", "/v1"] {
            let r = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
            let v: Value = serde_json::from_str(&body(r).await).unwrap();
            assert_eq!(v["error"], "not_found", "{path}");
        }
        // No CORS configured → no CORS headers.
        let r = app
            .oneshot(
                Request::get("/healthz")
                    .header("origin", "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(r.headers().get("access-control-allow-origin").is_none());
    }

    #[tokio::test]
    async fn web_root_serves_files_as_the_root_fallback() {
        let dir = std::env::temp_dir().join(format!("cvc-web-{}", crate::ids::uuid_v7()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), "<!doctype html><title>app</title>").unwrap();
        std::fs::write(dir.join("assets/app.js"), "console.log(1)").unwrap();
        std::fs::write(dir.join("assets/style.css"), "body{}").unwrap();
        let secret = dir.with_extension("secret");
        std::fs::write(&secret, "outside").unwrap();
        let state = test_state(&[], 10);
        let mut cfg = crate::config::Config::clone(&state.cfg);
        cfg.web_root = Some(dir.clone());
        let state = crate::state::AppState::new(state.db.clone(), cfg);
        let app = app_with(state, mounts());
        let get = |path: &str| {
            let app = app.clone();
            let req = Request::get(path).body(Body::empty()).unwrap();
            async move { app.oneshot(req).await.unwrap() }
        };

        for path in ["/", "/index.html"] {
            let r = get(path).await;
            assert_eq!(r.status(), StatusCode::OK, "{path}");
            assert!(r.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html"));
            assert_eq!(r.headers()[header::CACHE_CONTROL], "no-cache");
            assert_eq!(body(r).await, "<!doctype html><title>app</title>");
        }
        let r = get("/assets/app.js").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .contains("javascript"));
        assert!(r.headers().get(header::CACHE_CONTROL).is_none());
        let r = get("/assets/style.css").await;
        assert!(r.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/css"));
        let r = app
            .clone()
            .oneshot(Request::head("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // Unknown files, a directory without index.html (no listing), escapes, other
        // methods and the reserved prefixes are JSON 404s; routes still win.
        let escape = format!("/../{}", secret.file_name().unwrap().to_string_lossy());
        for path in [
            "/nope.js",
            "/assets/",
            escape.as_str(),
            "/%2e%2e/etc/passwd",
            "/mcp/x",
            "/healthz/x",
            "/v1/nope",
        ] {
            let r = get(path).await;
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
            let v: Value = serde_json::from_str(&body(r).await).unwrap();
            assert_eq!(v["error"], "not_found", "{path}");
        }
        // A directory without its slash redirects to it (then index.html or 404).
        let r = get("/assets").await;
        assert_eq!(r.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(r.headers()[header::LOCATION], "/assets/");
        let r = app
            .clone()
            .oneshot(Request::post("/index.html").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(body(get("/llms.txt").await).await, "guide");
        assert_eq!(body(get("/healthz").await).await, "ok");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_file(&secret).unwrap();
    }

    #[test]
    fn reserved_paths() {
        for p in ["/v1", "/v1/x", "/mcp", "/mcp/", "/healthz"] {
            assert!(reserved_path(p), "{p}");
        }
        for p in ["/", "/v10", "/mcpx", "/index.html", "/healthzz"] {
            assert!(!reserved_path(p), "{p}");
        }
    }

    #[tokio::test]
    async fn auth_routes_are_rate_limited_per_ip() {
        let app = app_with(test_state(&[], 2), mounts());
        let mut statuses = Vec::new();
        for _ in 0..3 {
            let r = app
                .clone()
                .oneshot(Request::post("/v1/auth/login").body(Body::empty()).unwrap())
                .await
                .unwrap();
            statuses.push(r.status());
        }
        assert_eq!(
            statuses,
            [
                StatusCode::OK,
                StatusCode::OK,
                StatusCode::TOO_MANY_REQUESTS
            ]
        );
    }

    #[tokio::test]
    async fn cors_allows_listed_origins_only() {
        let app = app_with(test_state(&["https://app.example.com"], 10), mounts());
        let preflight = |origin: &str| {
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/v1/auth/login")
                .header("origin", origin)
                .header("access-control-request-method", "POST")
                .header(
                    "access-control-request-headers",
                    "authorization,content-type,x-canvas-user",
                )
                .body(Body::empty())
                .unwrap()
        };
        let r = app
            .clone()
            .oneshot(preflight("https://app.example.com"))
            .await
            .unwrap();
        let h = r.headers();
        assert_eq!(h["access-control-allow-origin"], "https://app.example.com");
        assert_eq!(h["access-control-max-age"], "600");
        let allowed = h["access-control-allow-headers"].to_str().unwrap();
        for want in [
            "authorization",
            "content-type",
            "x-canvas-user",
            "mcp-protocol-version",
        ] {
            assert!(allowed.contains(want), "{allowed}");
        }
        assert!(h.get("access-control-allow-credentials").is_none());
        let r = app
            .clone()
            .oneshot(preflight("https://evil.example.com"))
            .await
            .unwrap();
        assert!(r.headers().get("access-control-allow-origin").is_none());
        let r = app
            .oneshot(
                Request::get("/healthz")
                    .header("origin", "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.headers()["access-control-allow-origin"],
            "https://app.example.com"
        );
    }
}
