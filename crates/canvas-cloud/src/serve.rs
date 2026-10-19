//! `canvas-cloud serve`: bootstrap, background tasks, graceful shutdown.

use std::net::SocketAddr;
use std::time::Duration;

use axum_server::Handle;
use canvas_cloud_core::config::Config;
use canvas_cloud_core::state::AppState;
use canvas_cloud_core::{db, observe, shutdown, tls};

/// How long in-flight requests get after SIGTERM before connections are dropped.
const GRACE: Duration = Duration::from_secs(10);
/// How long `serve` keeps retrying an unreachable database at startup.
const DB_STARTUP_WAIT: Duration = Duration::from_secs(60);
/// Expired sessions and long-expired access tokens are purged this often.
const PURGE_EVERY: Duration = Duration::from_secs(3600);

/// Config → tracing → metrics listener → database (with retries) → migrations → state →
/// hourly purge → serve until SIGINT/SIGTERM, then close canvas rooms (1001) and drain.
pub async fn run(cfg: Config, migrate: bool) -> anyhow::Result<()> {
    observe::init_tracing(cfg.log_format, &cfg.log_level);
    tracing::info!(version = %crate::version(), config = ?cfg, "starting canvas-cloud");
    cfg.check_web_root()?;

    let metrics = match cfg.metrics_listen {
        Some(_) => Some(observe::install_metrics()?),
        None => None,
    };

    let pool = db::connect_with_retries(&cfg, DB_STARTUP_WAIT).await?;
    if migrate {
        db::migrate(&pool).await?;
        tracing::info!("migrations up to date");
    } else {
        let (_, pending) = db::pending_migrations(&pool).await?;
        if !pending.is_empty() {
            tracing::warn!(?pending, "pending migrations (started with --no-migrate)");
        }
    }

    let state = AppState::new(pool.clone(), cfg);
    let cfg = state.cfg.clone();

    if let (Some(bind), Some(handle)) = (cfg.metrics_listen, metrics) {
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(err) = observe::serve_metrics(bind, handle, st).await {
                tracing::error!(error = format!("{err:#}"), "metrics listener failed");
            }
        });
    }
    if let Some(root) = &cfg.web_root {
        tracing::info!(web_root = %root.display(), "serving the web app at /");
    }

    {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PURGE_EVERY);
            loop {
                tick.tick().await;
                match db::purge_expired_sessions(&pool).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(removed = n, "purged expired sessions and tokens"),
                    Err(err) => tracing::warn!(error = %err, "session purge failed"),
                }
            }
        });
    }

    let handle: Handle<SocketAddr> = Handle::new();
    tokio::spawn(shutdown_on_signal(handle.clone()));

    let result = tls::serve(crate::app(state), &cfg, handle).await;
    shutdown::trigger();
    pool.close().await;
    match &result {
        Ok(()) => tracing::info!("stopped"),
        Err(err) => tracing::error!(error = format!("{err:#}"), "server error"),
    }
    result
}

async fn shutdown_on_signal(handle: Handle<SocketAddr>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!(
        grace_secs = GRACE.as_secs(),
        "shutdown signal received; draining"
    );
    // End event streams and tell canvas peers we are going away (close 1001) so the drain
    // does not wait on long-lived connections.
    shutdown::trigger();
    canvas_cloud_engine::close_all_rooms();
    handle.graceful_shutdown(Some(GRACE));
}
