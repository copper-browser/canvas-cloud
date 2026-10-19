//! Postgres pool + embedded migrations.
//!
//! Migrations live in `crates/canvas-cloud/migrations` (core identity owns `0001`–`0099`, the
//! engine `0100`–`0199`, apps `0200`–`0299`, human-auth extras `0300`–`0399`) and are embedded
//! here so every crate and test shares one migrator.
//!
//! Concurrency: [`migrate`] is safe to run from several processes at once. sqlx's migrator
//! takes a PostgreSQL advisory lock (`pg_advisory_lock` keyed on the database name) for the
//! whole run, so a second `canvas-cloud migrate`/`serve` waits for the first and then finds
//! nothing left to apply. [`MIGRATOR`] keeps that locking on (it is sqlx's default).

use std::str::FromStr as _;
use std::time::Duration;

use anyhow::Context as _;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions as _, PgPool};

use crate::config::Config;

/// All schema migrations (identity + engine + apps + auth), embedded at compile time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../canvas-cloud/migrations");

/// Open the connection pool (verifies with one round trip).
pub async fn connect(cfg: &Config) -> anyhow::Result<PgPool> {
    let pool = pool_options(cfg)
        .connect_with(connect_options(&cfg.database_url)?)
        .await
        .with_context(|| {
            format!(
                "connecting to {}",
                crate::config::redact_database_url(&cfg.database_url)
            )
        })?;
    Ok(pool)
}

/// Connect, retrying with backoff (1 s → 10 s) for up to `max_wait` — for `serve` starting
/// next to a database that is still coming up.
pub async fn connect_with_retries(cfg: &Config, max_wait: Duration) -> anyhow::Result<PgPool> {
    let started = std::time::Instant::now();
    let mut delay = Duration::from_secs(1);
    loop {
        match connect(cfg).await {
            Ok(pool) => return Ok(pool),
            Err(e) if started.elapsed() + delay < max_wait => {
                tracing::warn!(error = %format!("{e:#}"), retry_in_s = delay.as_secs(), "database not reachable yet");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(10));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Pool that connects on first use (for commands that should not fail fast).
pub fn connect_lazy(cfg: &Config) -> anyhow::Result<PgPool> {
    Ok(pool_options(cfg).connect_lazy_with(connect_options(&cfg.database_url)?))
}

fn pool_options(cfg: &Config) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(cfg.db_max_connections)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .idle_timeout(Some(Duration::from_secs(600)))
        .max_lifetime(Some(Duration::from_secs(1800)))
}

fn connect_options(url: &str) -> anyhow::Result<PgConnectOptions> {
    Ok(PgConnectOptions::from_str(url)
        .context("parsing database_url")?
        .application_name("canvas-cloud")
        // Never log statement text with bound values at info.
        .log_statements(log::LevelFilter::Debug)
        .log_slow_statements(log::LevelFilter::Warn, Duration::from_millis(500)))
}

/// Apply all pending migrations under sqlx's advisory lock (see the module docs).
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    debug_assert!(
        MIGRATOR.locking,
        "migrations must run under the advisory lock"
    );
    MIGRATOR.run(pool).await.context("running migrations")?;
    Ok(())
}

/// Migration status: (applied versions, embedded versions not yet applied).
pub async fn pending_migrations(pool: &PgPool) -> anyhow::Result<(usize, Vec<String>)> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await?;
    let applied: Vec<i64> = if exists {
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(pool)
            .await?
    } else {
        Vec::new()
    };
    let pending = MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration() && !applied.contains(&m.version))
        .map(|m| format!("{:04}_{}", m.version, m.description.replace(' ', "_")))
        .collect();
    Ok((applied.len(), pending))
}

/// Delete expired sessions and access tokens that expired more than a day ago; returns rows
/// removed.
pub async fn purge_expired_sessions(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let sessions = sqlx::query("DELETE FROM sessions WHERE expires_at < now()")
        .execute(pool)
        .await?
        .rows_affected();
    let tokens =
        sqlx::query("DELETE FROM access_tokens WHERE expires_at < now() - interval '1 day'")
            .execute(pool)
            .await?
            .rows_affected();
    Ok(sessions + tokens)
}
