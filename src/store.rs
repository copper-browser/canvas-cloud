//! Durable canvas document storage: sealed update log + compacted snapshot.

use copper_cloud_core::crypto::Crypto;
use copper_cloud_core::error::ApiError;
use sqlx::PgPool;
use uuid::Uuid;

/// What a room needs to rebuild its document.
pub(crate) struct Stored {
    pub key: [u8; 32],
    /// `(seq, state)` of the compacted snapshot, if any.
    pub snapshot: Option<(i64, Vec<u8>)>,
    /// `(seq, update)` of every update newer than the snapshot, in order.
    pub updates: Vec<(i64, Vec<u8>)>,
}

/// AAD binding a sealed update/snapshot to its canvas.
pub(crate) fn aad(canvas_id: Uuid) -> [u8; 16] {
    *canvas_id.as_bytes()
}

/// Unwraps a canvas's doc key.
pub(crate) async fn doc_key(
    db: &PgPool,
    crypto: &Crypto,
    canvas_id: Uuid,
) -> Result<[u8; 32], ApiError> {
    let wrapped: Vec<u8> = sqlx::query_scalar("SELECT doc_key_wrapped FROM canvases WHERE id = $1")
        .bind(canvas_id)
        .fetch_optional(db)
        .await?
        .ok_or(ApiError::NotFound)?;
    crypto.unwrap_key(&wrapped)
}

/// Loads and decrypts the snapshot and the update tail of a canvas.
pub(crate) async fn load(
    db: &PgPool,
    crypto: &Crypto,
    canvas_id: Uuid,
) -> Result<Stored, ApiError> {
    let key = doc_key(db, crypto, canvas_id).await?;
    let aad = aad(canvas_id);
    let snapshot: Option<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT seq, state FROM canvas_snapshots WHERE canvas_id = $1")
            .bind(canvas_id)
            .fetch_optional(db)
            .await?;
    let since = snapshot.as_ref().map_or(0, |s| s.0);
    let snapshot = match snapshot {
        Some((seq, sealed)) => Some((seq, Crypto::open(&key, &aad, &sealed)?)),
        None => None,
    };
    let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        r#"SELECT seq, "update" FROM canvas_updates
           WHERE canvas_id = $1 AND seq > $2 ORDER BY seq"#,
    )
    .bind(canvas_id)
    .bind(since)
    .fetch_all(db)
    .await?;
    let mut updates = Vec::with_capacity(rows.len());
    for (seq, sealed) in rows {
        updates.push((seq, Crypto::open(&key, &aad, &sealed)?));
    }
    Ok(Stored {
        key,
        snapshot,
        updates,
    })
}

/// Appends one sealed update; returns its sequence number.
pub(crate) async fn append(
    db: &PgPool,
    key: &[u8; 32],
    canvas_id: Uuid,
    update: &[u8],
) -> Result<i64, ApiError> {
    let sealed = Crypto::seal(key, &aad(canvas_id), update);
    let seq: i64 = sqlx::query_scalar(
        r#"INSERT INTO canvas_updates (canvas_id, "update") VALUES ($1, $2) RETURNING seq"#,
    )
    .bind(canvas_id)
    .bind(&sealed)
    .fetch_one(db)
    .await?;
    metrics::counter!("canvas_updates_persisted_total").increment(1);
    metrics::counter!("canvas_update_bytes_total").increment(update.len() as u64);
    Ok(seq)
}

/// Replaces the snapshot with `state` (covering every update up to `upto_seq`) and deletes
/// the covered updates, in one transaction.
pub(crate) async fn compact(
    db: &PgPool,
    key: &[u8; 32],
    canvas_id: Uuid,
    upto_seq: i64,
    state: &[u8],
) -> Result<u64, ApiError> {
    let sealed = Crypto::seal(key, &aad(canvas_id), state);
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO canvas_snapshots (canvas_id, seq, state, updated_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (canvas_id) DO UPDATE
           SET seq = EXCLUDED.seq, state = EXCLUDED.state, updated_at = now()",
    )
    .bind(canvas_id)
    .bind(upto_seq)
    .bind(&sealed)
    .execute(&mut *tx)
    .await?;
    let deleted = sqlx::query("DELETE FROM canvas_updates WHERE canvas_id = $1 AND seq <= $2")
        .bind(canvas_id)
        .bind(upto_seq)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    metrics::counter!("canvas_compactions_total").increment(1);
    Ok(deleted)
}
