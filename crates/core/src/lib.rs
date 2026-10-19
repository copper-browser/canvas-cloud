//! Canvas Cloud core: configuration, database pool + migrations, crypto, errors, request
//! extractors, identity (users, sessions, access tokens, linked apps), events, TLS,
//! observability, rate limits, CORS, and the router composition ([`app::app_with`]) the
//! binary and every crate's tests build on.

pub mod app;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod events;
pub mod extract;
pub mod identity;
pub mod ids;
pub mod observe;
pub mod shutdown;
pub mod state;
pub mod tls;

pub use app::{app_with, Mounts};
