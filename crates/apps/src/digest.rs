//! The canonical document digest of CONTRACT §6.3, re-exported from
//! [`canvas_cloud_engine::digest`] (the engine also uses it to verify snapshot compaction).
//!
//! An import answers with the digest of what it stored, and `GET
//! /v1/apps/self/canvases/{id}/digest` recomputes it from the database, so a migrator can
//! prove that a canvas arrived intact and recognise one that already did.

pub use canvas_cloud_engine::digest::{
    canonical_json, digest_txn, document_digest, EMPTY_DOCUMENT_DIGEST,
};
