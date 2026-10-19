//! Server-side Yjs documents and verified snapshot compaction.
//!
//! Every document the server builds from stored or imported updates is a [`server_doc`]: it
//! never garbage-collects in memory. A snapshot is written only through [`compact_state`],
//! which keeps a candidate encoding only after re-applying it to a fresh document and
//! getting the very same document back (state vector and [digest](crate::digest), nothing
//! pending). When no candidate survives that check there is no snapshot: the update log
//! stays the record, and it replays to the same document.
//!
//! Why: yrs 0.28 splits a string item at a UTF-16 offset that falls inside a surrogate pair
//! (a UTF-16 client — any Yjs/JavaScript peer — deleting or inserting between the two halves
//! of an emoji) by rounding to a whole character, so the item's clock length and its
//! string's length disagree from then on. The in-memory document stays right, but encoding
//! it can be wrong: depending on the edit, the garbage-collected encoding writes the
//! collected run one clock too long (every later struct of that client lands one clock off;
//! re-applying fails with `block parent … must be deleted or shared ref type` and most of the
//! document is gone), the uncollected one carries a character too many or too few, or both.
//! Yjs replaces the two halves with U+FFFD instead; yrs has a TODO for it
//! (y-crdt/y-crdt#714 is the same family). Until yrs is fixed, nothing unverified is stored.
//!
//! Collected snapshots stay the norm (deleted images are most of some canvases' history);
//! the uncollected state is the second candidate.

use yrs::updates::decoder::Decode as _;
use yrs::{Doc, Options, ReadTxn, StateVector, Transact as _, Update};

use crate::digest::digest_txn;

/// A document for server-side use: never garbage-collected in memory (see the module docs).
pub fn server_doc() -> Doc {
    Doc::with_options(Options {
        skip_gc: true,
        ..Options::default()
    })
}

/// A verified snapshot: the whole document as one lib0-v1 update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// The snapshot.
    pub state: Vec<u8>,
    /// `true` when deleted content was garbage-collected out of it.
    pub collected: bool,
}

/// The document read by `txn` as one update that provably re-applies to the same document:
/// garbage-collected when that round-trips and is smaller, else uncollected when that
/// round-trips; `None` when neither does (or structs are pending) — then do not compact.
/// `txn` should read a [`server_doc`].
pub fn compact_state<T: ReadTxn>(txn: &T) -> Option<Compacted> {
    if txn.has_missing_updates() {
        return None;
    }
    let sv = txn.state_vector();
    let digest = digest_txn(txn);
    let full = txn.encode_state_as_update_v1(&StateVector::default());
    if let Some(collected) = collect(&full) {
        if collected.len() < full.len() && round_trips(&collected, &sv, &digest) {
            return Some(Compacted {
                state: collected,
                collected: true,
            });
        }
    }
    if round_trips(&full, &sv, &digest) {
        return Some(Compacted {
            state: full,
            collected: false,
        });
    }
    tracing::warn!(
        bytes = full.len(),
        "no snapshot encoding of this document re-applies to the same document (yrs); \
         keeping the update log"
    );
    None
}

/// `full` applied to a garbage-collecting document (collected when the transaction commits),
/// re-encoded. `None` when it does not apply.
fn collect(full: &[u8]) -> Option<Vec<u8>> {
    let update = Update::decode_v1(full).ok()?;
    let doc = Doc::new();
    doc.transact_mut().apply_update(update).ok()?;
    let txn = doc.transact();
    Some(txn.encode_state_as_update_v1(&StateVector::default()))
}

/// Whether `state` re-applies to a fresh document with nothing pending, the state vector `sv`
/// and the digest `digest`.
fn round_trips(state: &[u8], sv: &StateVector, digest: &str) -> bool {
    let Ok(update) = Update::decode_v1(state) else {
        return false;
    };
    let doc = server_doc();
    if doc.transact_mut().apply_update(update).is_err() {
        return false;
    }
    let txn = doc.transact();
    !txn.has_missing_updates() && txn.state_vector() == *sv && digest_txn(&txn) == digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{GetString as _, Map as _, Out, Text as _, TextPrelim, WriteTxn as _};

    fn reload(state: &[u8]) -> Doc {
        let doc = server_doc();
        doc.transact_mut()
            .apply_update(Update::decode_v1(state).unwrap())
            .unwrap();
        doc
    }

    fn text(doc: &Doc) -> String {
        let txn = doc.transact();
        let shapes = txn.get_map("shapes").expect("shapes root");
        match shapes.get(&txn, "s") {
            Some(Out::YText(t)) => t.get_string(&txn),
            other => panic!("not a text: {other:?}"),
        }
    }

    fn var(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }

    /// A lib0-v1 update with no structs that deletes `ranges` (`(clock, len)`) of `client`.
    fn deletes(client: u64, ranges: &[(u64, u64)]) -> Vec<u8> {
        let mut out = Vec::new();
        var(&mut out, 0); // no structs
        var(&mut out, 1); // delete set: one client
        var(&mut out, client);
        var(&mut out, ranges.len() as u64);
        for (clock, len) in ranges {
            var(&mut out, *clock);
            var(&mut out, *len);
        }
        out
    }

    /// `s` as the Y.Text `shapes.s` of a fresh peer (its text starts at clock 1), then the
    /// UTF-16 `ranges` of the text deleted the way a Yjs peer would send them, applied to a
    /// server document. Returns the server document.
    fn with_deletes(s: &str, ranges: &[(u64, u64)]) -> Doc {
        let peer = server_doc();
        {
            let mut txn = peer.transact_mut();
            let shapes = txn.get_or_insert_map("shapes");
            shapes.insert(&mut txn, "s", TextPrelim::new(s));
            shapes.insert(&mut txn, "z", "after");
        }
        let full = peer
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let clocks: Vec<(u64, u64)> = ranges.iter().map(|(at, len)| (at + 1, *len)).collect();
        let doc = server_doc();
        for update in [full, deletes(peer.client_id().get(), &clocks)] {
            doc.transact_mut()
                .apply_update(Update::decode_v1(&update).unwrap())
                .unwrap();
        }
        doc
    }

    #[test]
    fn collects_deleted_content() {
        let doc = server_doc();
        {
            let mut txn = doc.transact_mut();
            let shapes = txn.get_or_insert_map("shapes");
            shapes.insert(&mut txn, "big", "x".repeat(10_000));
            shapes.insert(&mut txn, "keep", "y");
        }
        {
            let mut txn = doc.transact_mut();
            let shapes = txn.get_or_insert_map("shapes");
            shapes.remove(&mut txn, "big");
        }
        let full = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let c = compact_state(&doc.transact()).expect("compactable");
        assert!(c.collected, "deleted content is collected");
        assert!(
            c.state.len() < full.len() / 10,
            "{} vs {}",
            c.state.len(),
            full.len()
        );
        assert_eq!(
            digest_txn(&reload(&c.state).transact()),
            digest_txn(&doc.transact())
        );
    }

    #[test]
    fn nothing_to_collect_keeps_the_state() {
        let doc = server_doc();
        {
            let mut txn = doc.transact_mut();
            let shapes = txn.get_or_insert_map("shapes");
            let t = shapes.insert(&mut txn, "s", TextPrelim::new("hello 🔴 world"));
            t.push(&mut txn, "!");
        }
        let c = compact_state(&doc.transact()).expect("compactable");
        assert!(!c.collected);
        assert_eq!(text(&reload(&c.state)), "hello 🔴 world!");
    }

    /// Every deletion of a text with emoji, including the ones starting or ending inside a
    /// surrogate pair: whatever `compact_state` returns re-applies to the same document, and
    /// when it returns nothing the plain encodings really are broken.
    #[test]
    fn surrogate_splits_never_produce_a_wrong_snapshot() {
        let (mut compacted, mut refused) = (0, 0);
        for s in ["🔴 The end it.", "a🔴b c🟡d", "🔴🟡x"] {
            let n = s.encode_utf16().count() as u64;
            let mut cases: Vec<Vec<(u64, u64)>> = Vec::new();
            for at in 0..n {
                for len in 1..=n - at {
                    cases.push(vec![(at, len)]);
                    for at2 in at + len + 1..n {
                        cases.push(vec![(at, len), (at2, 1)]);
                    }
                }
            }
            for ranges in cases {
                let doc = with_deletes(s, &ranges);
                let txn = doc.transact();
                let want = digest_txn(&txn);
                if let Some(c) = compact_state(&txn) {
                    compacted += 1;
                    let back = reload(&c.state);
                    assert_eq!(digest_txn(&back.transact()), want, "{s:?} {ranges:?}");
                    assert_eq!(text(&back), text(&doc), "{s:?} {ranges:?}");
                } else {
                    refused += 1;
                    let full = txn.encode_state_as_update_v1(&StateVector::default());
                    assert!(
                        !round_trips(&full, &txn.state_vector(), &want),
                        "{s:?} {ranges:?}: refused a state that round-trips"
                    );
                }
            }
        }
        assert!(
            compacted > 0 && refused > 0,
            "{compacted} compacted, {refused} refused"
        );
    }

    /// The everyday case is unaffected: a surrogate pair deleted whole.
    #[test]
    fn whole_emoji_deletes_compact() {
        let doc = with_deletes("a🔴b", &[(1, 2)]);
        let c = compact_state(&doc.transact()).expect("compactable");
        assert_eq!(text(&reload(&c.state)), "ab");
    }
}
