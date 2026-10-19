//! Per-app `user_id → subject` maps for the event streams.
//!
//! Every `/v1/apps/self/events` stream of one app shares one [`Directory`]: a hash map read
//! once per event under a read lock, so many concurrent subscribers stay cheap. It is loaded
//! from `app_users` when the first stream of the app opens, kept current incrementally by
//! the provisioning routes of this crate ([`linked`] / [`unlinked`]), and reloaded at most
//! every [`REFRESH_EVERY`] to pick up changes made elsewhere (e.g. `canvas-cloud admin users
//! delete`). It is dropped when the app's last stream ends.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use canvas_cloud_core::error::ApiError;
use sqlx::PgPool;
use uuid::Uuid;

/// How often a live directory is reloaded from the database.
pub const REFRESH_EVERY: Duration = Duration::from_secs(30);

pub struct Directory {
    app_id: Uuid,
    users: RwLock<HashMap<Uuid, Arc<str>>>,
    /// Bumped by every incremental change; a reload that raced one merges instead of
    /// replacing.
    generation: AtomicU64,
    loaded_at: Mutex<Instant>,
    refreshing: AtomicBool,
    /// The app was revoked (seen by a reload): its streams end at once.
    revoked: tokio::sync::watch::Sender<bool>,
    ready: tokio::sync::OnceCell<()>,
}

fn registry() -> &'static Mutex<HashMap<Uuid, Weak<Directory>>> {
    static REGISTRY: LazyLock<Mutex<HashMap<Uuid, Weak<Directory>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &REGISTRY
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The live directory of `app_id`, if a stream holds one.
fn live(app_id: Uuid) -> Option<Arc<Directory>> {
    lock(registry()).get(&app_id).and_then(Weak::upgrade)
}

/// The directory of `app_id`, loaded (shared with the app's other streams).
pub async fn open(db: &PgPool, app_id: Uuid) -> Result<Arc<Directory>, ApiError> {
    let dir = {
        let mut reg = lock(registry());
        reg.retain(|_, w| w.strong_count() > 0);
        if let Some(d) = reg.get(&app_id).and_then(Weak::upgrade) {
            d
        } else {
            let d = Arc::new(Directory {
                app_id,
                users: RwLock::new(HashMap::new()),
                generation: AtomicU64::new(0),
                loaded_at: Mutex::new(Instant::now()),
                refreshing: AtomicBool::new(false),
                revoked: tokio::sync::watch::Sender::new(false),
                ready: tokio::sync::OnceCell::new(),
            });
            reg.insert(app_id, Arc::downgrade(&d));
            d
        }
    };
    dir.ready
        .get_or_try_init(|| async { dir.reload(db).await })
        .await?;
    if dir.revoked() {
        // The caller just authenticated as this app, so it is live again (key rotated).
        dir.reload(db).await?;
    }
    Ok(dir)
}

/// `user_id` is now `subject` of `app_id` (provisioned or re-linked).
pub fn linked(app_id: Uuid, user_id: Uuid, subject: &str) {
    if let Some(d) = live(app_id) {
        d.generation.fetch_add(1, Ordering::AcqRel);
        d.write().insert(user_id, Arc::from(subject));
    }
}

/// Reloads the live directory of `app_id` now (users and revocation), e.g. after an
/// out-of-band change; a no-op when no stream of the app is open.
pub async fn reload_now(db: &PgPool, app_id: Uuid) -> Result<(), ApiError> {
    match live(app_id) {
        Some(d) => d.reload(db).await,
        None => Ok(()),
    }
}

/// `user_id` is no longer linked to `app_id` (unlinked or deleted).
pub fn unlinked(app_id: Uuid, user_id: Uuid) {
    if let Some(d) = live(app_id) {
        d.generation.fetch_add(1, Ordering::AcqRel);
        d.write().remove(&user_id);
    }
}

impl Directory {
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<Uuid, Arc<str>>> {
        self.users
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The subject of `user_id` if it is one of this app's users.
    pub fn subject(&self, user_id: Uuid) -> Option<Arc<str>> {
        self.users
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&user_id)
            .cloned()
    }

    pub fn revoked(&self) -> bool {
        *self.revoked.borrow()
    }

    /// Resolves once the app is seen revoked.
    pub fn on_revoked(&self) -> tokio::sync::watch::Receiver<bool> {
        self.revoked.subscribe()
    }

    /// Number of users in the map (tests/diagnostics).
    pub fn len(&self) -> usize {
        self.users
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    async fn reload(&self, db: &PgPool) -> Result<(), ApiError> {
        let generation = self.generation.load(Ordering::Acquire);
        let live: Option<bool> =
            sqlx::query_scalar("SELECT revoked_at IS NULL FROM apps WHERE id = $1")
                .bind(self.app_id)
                .fetch_optional(db)
                .await?;
        // Re-enabled by a key rotation (`revoked_at` cleared): not revoked any more.
        let revoked = live != Some(true);
        self.revoked
            .send_if_modified(|r| std::mem::replace(r, revoked) != revoked);
        let rows: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT user_id, subject FROM app_users WHERE app_id = $1")
                .bind(self.app_id)
                .fetch_all(db)
                .await?;
        let fresh: HashMap<Uuid, Arc<str>> = rows
            .into_iter()
            .map(|(id, subject)| (id, Arc::from(subject)))
            .collect();
        let mut users = self.write();
        if self.generation.load(Ordering::Acquire) == generation {
            *users = fresh;
        } else {
            // An incremental change landed while we read: keep it (it is newer).
            for (id, subject) in fresh {
                users.entry(id).or_insert(subject);
            }
        }
        drop(users);
        *lock(&self.loaded_at) = Instant::now();
        Ok(())
    }

    /// Reloads in the background if the map is older than [`REFRESH_EVERY`] (one reload at a
    /// time per app).
    pub fn refresh_if_stale(self: &Arc<Self>, db: &PgPool) {
        if lock(&self.loaded_at).elapsed() < REFRESH_EVERY {
            return;
        }
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let dir = Arc::clone(self);
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(e) = dir.reload(&db).await {
                tracing::warn!(app_id = %dir.app_id, error = %e, "reloading an app's user directory failed");
                // Try again after another interval rather than on every event.
                *lock(&dir.loaded_at) = Instant::now();
            }
            dir.refreshing.store(false, Ordering::Release);
        });
    }
}
