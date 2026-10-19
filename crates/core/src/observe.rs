//! Logging (tracing → stdout, JSON or pretty), request spans + HTTP metrics, and the
//! Prometheus `/metrics` listener.
//!
//! Request spans carry method, path (never the query string — it may hold `?token=`), the
//! matched route, status, latency, client ip and `user_id` once authenticated. Error
//! responses add `error` (the machine-readable code) and, for `400`s, `error_message` (the
//! message the client got, with quoted input values redacted). Tokens, passwords, keys and
//! payloads are never logged.

use std::borrow::Cow;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::Router;
use futures::FutureExt as _;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tracing::field::Empty;
use tracing::Instrument as _;
use tracing_subscriber::EnvFilter;

use crate::config::LogFormat;
use crate::state::SharedState;

/// Install the global tracing subscriber (idempotent). `RUST_LOG` overrides `level`.
pub fn init_tracing(format: LogFormat, level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "{level},sqlx=warn,hyper=warn,h2=warn,rustls=warn,tower=warn,rustls_acme=info"
        ))
    });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stdout);
    let _ = match format {
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .try_init(),
        LogFormat::Pretty => builder.compact().try_init(),
    };
}

/// Install the Prometheus recorder and describe all metrics. Call once per process.
pub fn install_metrics() -> anyhow::Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("http_request_duration_seconds".into()),
            &[
                0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ],
        )?
        .install_recorder()
        .context("installing Prometheus recorder")?;
    metrics::describe_counter!("http_requests_total", "HTTP requests by route and status");
    metrics::describe_histogram!(
        "http_request_duration_seconds",
        metrics::Unit::Seconds,
        "Time to response headers by route"
    );
    metrics::describe_gauge!("sse_subscribers", "Open server-sent event streams");
    metrics::describe_gauge!("canvas_rooms", "Canvas rooms loaded in memory");
    metrics::describe_gauge!("canvas_peers", "WebSocket peers connected to canvas rooms");
    metrics::describe_counter!(
        "canvas_updates_persisted_total",
        "Yjs updates written to the canvas update log"
    );
    metrics::describe_counter!(
        "canvas_update_bytes_total",
        metrics::Unit::Bytes,
        "Bytes of Yjs updates persisted"
    );
    metrics::describe_counter!("canvas_compactions_total", "Canvas update-log compactions");
    metrics::describe_counter!(
        "canvas_ws_bytes_in_total",
        metrics::Unit::Bytes,
        "WebSocket bytes received from canvas peers"
    );
    metrics::describe_counter!(
        "canvas_ws_bytes_out_total",
        metrics::Unit::Bytes,
        "WebSocket bytes sent to canvas peers"
    );
    metrics::describe_counter!("canvas_mentions_total", "Chat @mentions recorded");
    metrics::describe_counter!(
        "canvas_mentions_rate_limited_total",
        "Mention posts refused by the per-user rate limit"
    );
    metrics::describe_gauge!("db_pool_size", "Postgres pool connections (open)");
    metrics::describe_gauge!("db_pool_idle", "Postgres pool connections (idle)");
    metrics::describe_counter!(
        "auth_rate_limited_total",
        "Requests rejected by the auth rate limiter"
    );
    metrics::describe_gauge!("process_uptime_seconds", metrics::Unit::Seconds, "Uptime");
    Ok(handle)
}

/// Serve `GET /metrics` on `bind` until shutdown. Also refreshes the pool gauges on scrape
/// and runs histogram upkeep.
pub async fn serve_metrics(
    bind: SocketAddr,
    handle: PrometheusHandle,
    state: SharedState,
) -> anyhow::Result<()> {
    let upkeep = handle.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            upkeep.run_upkeep();
        }
    });
    let app = Router::new().route(
        "/metrics",
        get(move || {
            let handle = handle.clone();
            let state = state.clone();
            async move {
                record_pool_gauges(&state);
                handle.render()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding metrics listener {bind}"))?;
    tracing::info!(%bind, "metrics listening");
    let mut shutdown = crate::shutdown::subscribe();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { crate::shutdown::wait(&mut shutdown).await })
        .await
        .context("metrics server")
}

fn record_pool_gauges(state: &SharedState) {
    #[allow(clippy::cast_precision_loss)]
    {
        metrics::gauge!("db_pool_size").set(f64::from(state.db.size()));
        metrics::gauge!("db_pool_idle").set(state.db.num_idle() as f64);
        metrics::gauge!("process_uptime_seconds").set(state.started_at.elapsed().as_secs_f64());
    }
}

/// Client IP: the TCP peer, or (only when `trust_proxy`) the right-most `X-Forwarded-For`
/// entry / `X-Real-IP` set by the proxy in front of us.
pub fn client_ip(req: &Request, trust_proxy: bool) -> Option<IpAddr> {
    if trust_proxy {
        let headers = req.headers();
        let forwarded = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|s| s.trim().parse::<IpAddr>().ok())
            .next_back();
        if let Some(ip) = forwarded {
            return Some(ip);
        }
        if let Some(ip) = headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse().ok())
        {
            return Some(ip);
        }
    }
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
}

fn status_label(status: StatusCode) -> Cow<'static, str> {
    Cow::Borrowed(match status.as_u16() {
        200 => "200",
        201 => "201",
        204 => "204",
        304 => "304",
        400 => "400",
        401 => "401",
        403 => "403",
        404 => "404",
        405 => "405",
        409 => "409",
        413 => "413",
        429 => "429",
        500 => "500",
        502 => "502",
        503 => "503",
        _ => return Cow::Owned(status.as_str().to_owned()),
    })
}

/// Middleware: request span + access log + `http_requests_total` /
/// `http_request_duration_seconds`.
pub async fn track(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    let start = Instant::now();
    let route: Cow<'static, str> = req
        .extensions()
        .get::<MatchedPath>()
        .map_or(Cow::Borrowed("unmatched"), |m| {
            Cow::Owned(m.as_str().to_owned())
        });
    let ip = client_ip(&req, state.cfg.trust_proxy);
    let span = tracing::info_span!(
        "request",
        method = %req.method(),
        path = %req.uri().path(),
        route = %route,
        ip = ip.map(tracing::field::display),
        user_id = Empty,
        app_id = Empty,
        status = Empty,
        latency_ms = Empty,
        error = Empty,
        error_message = Empty,
    );
    let quiet = route == "/healthz";
    // A panicking handler becomes a logged 500 instead of a dropped connection.
    let response = match std::panic::AssertUnwindSafe(next.run(req))
        .catch_unwind()
        .instrument(span.clone())
        .await
    {
        Ok(response) => response,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic".to_owned());
            span.in_scope(|| tracing::error!(panic = %msg, "handler panicked"));
            crate::error::ApiError::internal("handler panicked").into_response()
        }
    };
    let elapsed = start.elapsed();
    let status = response.status();
    span.record("status", status.as_u16());
    if let Some(err) = response.extensions().get::<crate::error::ErrorLog>() {
        span.record("error", err.code);
        if let Some(message) = &err.message {
            span.record("error_message", message.as_str());
        }
    }
    #[allow(clippy::cast_possible_truncation)]
    span.record("latency_ms", elapsed.as_secs_f64() * 1000.0);
    span.in_scope(|| {
        if status.is_server_error() {
            tracing::error!("request failed");
        } else if quiet {
            tracing::debug!("request");
        } else {
            tracing::info!("request");
        }
    });
    metrics::counter!(
        "http_requests_total",
        "route" => route.clone(),
        "status" => status_label(status)
    )
    .increment(1);
    metrics::histogram!("http_request_duration_seconds", "route" => route)
        .record(elapsed.as_secs_f64());
    response
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use tower::ServiceExt as _;

    use super::*;

    /// Collects everything the subscriber writes.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Self;

        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    #[tokio::test]
    async fn request_log_line_says_why_a_request_failed() {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_writer(captured.clone())
            .finish();
        let _default = tracing::subscriber::set_default(subscriber);
        let url = "postgres://localhost/unused";
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy(url)
            .unwrap();
        let state = crate::state::AppState::new(pool, crate::config::Config::for_tests(url));
        let app = Router::new()
            .route(
                "/bad",
                get(|| async {
                    crate::error::ApiError::bad_request(
                        "entry 2: invalid type: string \"secret\", expected u64",
                    )
                }),
            )
            .route("/gone", get(|| async { crate::error::ApiError::NotFound }))
            .layer(axum::middleware::from_fn_with_state(state.clone(), track))
            .with_state(state);
        for path in ["/bad", "/gone"] {
            let req = Request::get(path).body(Body::empty()).unwrap();
            app.clone().oneshot(req).await.unwrap();
        }

        let out = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|v: &serde_json::Value| v["message"] == "request")
            .collect();
        assert_eq!(lines.len(), 2, "{out}");
        let bad = &lines[0]["span"];
        assert_eq!(bad["status"], 400);
        assert_eq!(bad["error"], "bad_request");
        assert_eq!(
            bad["error_message"],
            "entry 2: invalid type: string \"…\", expected u64"
        );
        assert!(!out.contains("secret"));
        let gone = &lines[1]["span"];
        assert_eq!(gone["status"], 404);
        assert_eq!(gone["error"], "not_found");
        assert!(gone.get("error_message").is_none());
    }
}
