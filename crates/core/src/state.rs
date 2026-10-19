//! Shared application state handed to every handler.

use std::sync::Arc;
use std::time::Instant;

use crate::config::Config;
use crate::crypto::Crypto;
use crate::events::Events;

pub struct AppState {
    pub db: sqlx::PgPool,
    pub cfg: Arc<Config>,
    pub crypto: Crypto,
    pub events: Events,
    pub started_at: Instant,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    /// Build the shared state: derives the key-encryption key from `cfg.master_key`.
    pub fn new(db: sqlx::PgPool, cfg: Config) -> SharedState {
        let crypto = Crypto::from_master_key(&cfg.master_key);
        Arc::new(Self {
            db,
            cfg: Arc::new(cfg),
            crypto,
            events: Events::new(),
            started_at: Instant::now(),
        })
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("cfg", &self.cfg)
            .field("events", &self.events)
            .finish_non_exhaustive()
    }
}
