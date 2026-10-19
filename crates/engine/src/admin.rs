//! Instance-admin operations (the admin CLI, an app's admin APIs). They bypass membership
//! checks — callers must have authorized the action themselves.

use canvas_cloud_core::error::ApiError;
use canvas_cloud_core::state::AppState;
use uuid::Uuid;

use crate::rest::{member_ids, publish};
pub use crate::room::open_rooms;
use crate::room::rooms;

/// Delete any canvas (Personal ones too — the owner gets a fresh empty one on next use):
/// its updates, snapshot, members and invites go with it (FK cascade), connected peers are
/// disconnected (close 4404) and members get a `canvas` `deleted` event. Returns the
/// deleted canvas's name, `None` if it did not exist.
pub async fn delete_canvas(state: &AppState, canvas_id: Uuid) -> Result<Option<String>, ApiError> {
    let members = member_ids(state, canvas_id).await?;
    let name: Option<String> =
        sqlx::query_scalar("DELETE FROM canvases WHERE id = $1 RETURNING name")
            .bind(canvas_id)
            .fetch_optional(&state.db)
            .await?;
    if name.is_some() {
        canvases_deleted(state, &[(canvas_id, members)]);
        tracing::info!(%canvas_id, "canvas deleted by admin");
    }
    Ok(name)
}

/// The canvases `user_id` owns, each with its member ids — collect this **before** deleting
/// the user, then pass it to [`canvases_deleted`] after the delete commits.
pub async fn owned_canvases(
    state: &AppState,
    user_id: Uuid,
) -> Result<Vec<(Uuid, Vec<Uuid>)>, ApiError> {
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM canvases WHERE owner_id = $1")
        .bind(user_id)
        .fetch_all(&state.db)
        .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push((id, member_ids(state, id).await?));
    }
    Ok(out)
}

/// After canvases were deleted in the database: close their rooms and notify members.
pub fn canvases_deleted(state: &AppState, deleted: &[(Uuid, Vec<Uuid>)]) {
    for (canvas_id, members) in deleted {
        rooms().close(*canvas_id);
        publish(state, members, *canvas_id, "deleted");
    }
}

/// Disconnect every live canvas peer of `user_id` (account disabled or deleted).
pub fn disconnect_user(user_id: Uuid) {
    rooms().kick_user_everywhere(user_id);
}

/// Close the room of a canvas that is gone (deleted in the database by the caller): its
/// peers are disconnected with close 4404. No event is published. A no-op when the room is
/// not in memory.
pub fn close_room(canvas_id: Uuid) {
    rooms().close(canvas_id);
}

/// Runs `f` — typically a transaction that deletes canvas `canvas_id` and stores a new
/// document under the **same id** (an import replacing it) — with the canvas's room shut:
///
/// * connected peers are disconnected with close 1001 ("going away"), not 4404: the canvas
///   still exists, so clients reconnect and sync the new document;
/// * a write already in progress finishes first (under the old document); every later read
///   or write of the room waits until `f` is done and then fails with 404, so nothing is ever
///   persisted against the replaced document's key;
/// * afterwards the room is dropped from memory, so the next use loads the new document from
///   the database.
pub async fn with_room_shut<F: std::future::Future>(canvas_id: Uuid, f: F) -> F::Output {
    rooms().shut_while(canvas_id, f).await
}
