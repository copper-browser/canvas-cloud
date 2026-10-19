//! Process-wide shutdown signal so long-lived streams (SSE, `WebSockets`) can end promptly
//! when the server begins a graceful shutdown.

use std::sync::OnceLock;

use tokio::sync::watch;

fn channel() -> &'static watch::Sender<bool> {
    static TX: OnceLock<watch::Sender<bool>> = OnceLock::new();
    TX.get_or_init(|| watch::channel(false).0)
}

/// Begin shutdown: every [`wait`]er wakes up.
pub fn trigger() {
    channel().send_replace(true);
}

/// True once [`trigger`] has been called.
pub fn is_shutting_down() -> bool {
    *channel().borrow()
}

pub fn subscribe() -> watch::Receiver<bool> {
    channel().subscribe()
}

/// Resolves when shutdown has been triggered (immediately if it already was).
pub async fn wait(rx: &mut watch::Receiver<bool>) {
    // An error means the sender is gone, which only happens at process exit.
    let _ = rx.wait_for(|v| *v).await;
}
