//! Instance-admin operations used by the admin API (`/admin/api/*`, in the binary crate).
//! They bypass membership checks — callers must have authenticated an admin.

use copper_cloud_core::error::ApiError;
use copper_cloud_core::state::AppState;
use uuid::Uuid;

use crate::rest::{member_ids, publish};
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
