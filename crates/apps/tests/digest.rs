//! The §6.3 canonical digest: the import producers' reference vectors (byte for byte),
//! and randomized properties — independent of delivery order, merging, splitting, snapshot
//! compaction and root typing; sensitive to content.

#![allow(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements
)]

use canvas_cloud_apps::digest::{canonical_json, document_digest, EMPTY_DOCUMENT_DIGEST};
use rand::rngs::StdRng;
use rand::seq::SliceRandom as _;
use rand::{Rng as _, SeedableRng as _};
use yrs::types::text::Text as _;
use yrs::updates::decoder::Decode as _;
use yrs::{
    any, Any, Array as _, Doc, Map as _, MapPrelim, Out, ReadTxn as _, StateVector, Transact as _,
    Update,
};

fn doc_from(updates: &[&[u8]]) -> Doc {
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        for u in updates {
            txn.apply_update(Update::decode_v1(u).unwrap()).unwrap();
        }
    }
    doc
}

fn edit(doc: &Doc, f: impl FnOnce(&Doc)) -> Vec<u8> {
    let before = doc.transact().state_vector();
    f(doc);
    doc.transact().encode_state_as_update_v1(&before)
}

fn compacted(doc: &Doc) -> Vec<u8> {
    doc.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![Vec::new()];
    }
    let mut out = Vec::new();
    for p in permutations(n - 1) {
        for i in 0..=p.len() {
            let mut q = p.clone();
            q.insert(i, n - 1);
            out.push(q);
        }
    }
    out
}

/// The reference implementation's fixture: two concurrent clients over every root kind.
fn history() -> Vec<Vec<u8>> {
    let a = Doc::with_client_id(1);
    let b = Doc::with_client_id(2);
    let mut out = Vec::new();
    out.push(edit(&a, |d| {
        let meta = d.get_or_insert_map("meta");
        let shapes = d.get_or_insert_map("shapes");
        let mut t = d.transact_mut();
        meta.insert(&mut t, "name", "Board");
        shapes.insert(
            &mut t,
            "s1",
            MapPrelim::from([("x", Any::from(1)), ("y", Any::from(2.5))]),
        );
        shapes.insert(&mut t, "gone", any!({"type": "sticky"}));
    }));
    out.push(edit(&b, |d| {
        let chat = d.get_or_insert_array("chat");
        let notes = d.get_or_insert_text("notes");
        let mut t = d.transact_mut();
        chat.push_back(&mut t, any!({"id": "m1", "text": "hi \u{2028} </script>"}));
        notes.insert(&mut t, 0, "naïve ✓");
    }));
    {
        let mut t = b.transact_mut();
        t.apply_update(Update::decode_v1(&out[0]).unwrap()).unwrap();
    }
    out.push(edit(&b, |d| {
        let shapes = d.get_or_insert_map("shapes");
        let mut t = d.transact_mut();
        if let Some(Out::YMap(s1)) = shapes.get(&t, "s1") {
            s1.insert(&mut t, "x", 99);
        }
        shapes.remove(&mut t, "gone");
    }));
    out.push(edit(&a, |d| {
        let meta = d.get_or_insert_map("meta");
        let mut t = d.transact_mut();
        meta.insert(&mut t, "thumb", Any::Buffer(vec![1_u8, 2, 3].into()));
        meta.insert(&mut t, "big", Any::Number(yrs::Number::Int(7)));
    }));
    out
}

#[test]
fn reference_vectors_match_byte_for_byte() {
    let h = history();
    let refs: Vec<&[u8]> = h.iter().map(Vec::as_slice).collect();
    let doc = doc_from(&refs);
    let json = canonical_json(&doc.transact());
    assert_eq!(
        json,
        concat!(
            r#"{"chat":[{"id":"m1","text":"hi "#,
            "\u{2028}",
            r#" </script>"}],"#,
            r#""meta":{"big":7.0,"name":"Board","thumb":"AQID"},"#,
            r#""notes":"naïve ✓","#,
            r#""shapes":{"s1":{"x":99.0,"y":2.5}}}"#
        )
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["shapes"]["s1"]["x"], 99.0);
    // SHA-256 of exactly those bytes.
    use sha2::Digest as _;
    assert_eq!(
        document_digest(&doc),
        hex::encode(sha2::Sha256::digest(json.as_bytes()))
    );
}

#[test]
fn empty_documents_and_empty_roots() {
    let empty = Doc::new();
    assert_eq!(canonical_json(&empty.transact()), "{}");
    // e3b0… is SHA-256(""); the digest is over "{}".
    assert_eq!(
        document_digest(&empty),
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
    assert_eq!(document_digest(&empty), EMPTY_DOCUMENT_DIGEST);
    // A root whose only entry was deleted renders like no root at all.
    let doc = Doc::new();
    let m = doc.get_or_insert_map("shapes");
    {
        let mut t = doc.transact_mut();
        m.insert(&mut t, "a", 1);
    }
    {
        let mut t = doc.transact_mut();
        m.remove(&mut t, "a");
    }
    assert_eq!(document_digest(&doc), EMPTY_DOCUMENT_DIGEST);
    assert_eq!(
        document_digest(&doc_from(&[&compacted(&doc)])),
        EMPTY_DOCUMENT_DIGEST
    );
}

#[test]
fn reference_order_compaction_and_typing() {
    let h = history();
    let refs: Vec<&[u8]> = h.iter().map(Vec::as_slice).collect();
    let forward = doc_from(&refs);
    let expected = document_digest(&forward);
    let mut complete = 0;
    for order in permutations(4) {
        let us: Vec<&[u8]> = order.iter().map(|i| refs[*i]).collect();
        let doc = doc_from(&us);
        let pos = |u: usize| order.iter().position(|x| *x == u).unwrap();
        let causal = pos(0) < pos(2) && pos(1) < pos(2) && pos(0) < pos(3);
        if doc.transact().has_missing_updates() {
            assert!(!causal, "a causal order {order:?} left updates pending");
            let again = doc_from(&[us.clone(), refs.clone()].concat());
            assert_eq!(document_digest(&again), expected, "{order:?} re-delivered");
        } else {
            complete += 1;
            assert_eq!(document_digest(&doc), expected, "order {order:?}");
        }
    }
    assert!(complete >= 6, "{complete}");
    assert_eq!(
        document_digest(&doc_from(&[&compacted(&forward)])),
        expected
    );
    let prefix = compacted(&doc_from(&refs[..2]));
    assert_eq!(
        document_digest(&doc_from(&[&prefix, refs[2], refs[3]])),
        expected
    );
    let typed = Doc::new();
    typed.get_or_insert_map("meta");
    typed.get_or_insert_map("shapes");
    typed.get_or_insert_array("chat");
    typed.get_or_insert_text("notes");
    typed.get_or_insert_map("never-used");
    {
        let mut t = typed.transact_mut();
        for u in &refs {
            t.apply_update(Update::decode_v1(u).unwrap()).unwrap();
        }
    }
    assert_eq!(document_digest(&typed), expected);
}

/// A random edit by one client on its replica: maps (nested and flat), the chat array, a
/// text root, deletions, numbers of both kinds, binary.
fn random_edit(rng: &mut StdRng, doc: &Doc) -> Vec<u8> {
    let choice = rng.gen_range(0..7);
    let key = format!("k{}", rng.gen_range(0..6));
    edit(doc, |d| {
        let shapes = d.get_or_insert_map("shapes");
        let meta = d.get_or_insert_map("meta");
        let chat = d.get_or_insert_array("chat");
        let notes = d.get_or_insert_text("notes");
        let mut t = d.transact_mut();
        match choice {
            0 => {
                shapes.insert(
                    &mut t,
                    key.as_str(),
                    MapPrelim::from([
                        ("x", Any::from(rng.gen_range(-1000..1000))),
                        ("y", Any::from(rng.gen::<f64>() * 100.0)),
                        ("text", Any::from(format!("note {}", rng.gen::<u16>()))),
                    ]),
                );
            }
            1 => {
                if let Some(Out::YMap(m)) = shapes.get(&t, &key) {
                    m.insert(&mut t, "x", rng.gen_range(0_i64..10));
                }
            }
            2 => {
                shapes.remove(&mut t, &key);
            }
            3 => {
                meta.insert(
                    &mut t,
                    key.as_str(),
                    Any::Buffer(vec![rng.gen::<u8>(); 3].into()),
                );
            }
            4 => {
                chat.push_back(
                    &mut t,
                    any!({"id": rng.gen::<u32>().to_string(), "text": "hé \"quoted\""}),
                );
            }
            5 => {
                // ASCII only: yrs text offsets count UTF-8 bytes, and splitting a multi-byte
                // character panics (the fixed `history()` covers non-ASCII text).
                let len = notes.len(&t);
                let at = rng.gen_range(0..=len);
                notes.insert(&mut t, at, "abc");
            }
            _ => {
                let len = notes.len(&t);
                if len > 0 {
                    notes.remove_range(&mut t, 0, len.min(2));
                }
            }
        }
    })
}

/// A random history of `clients` replicas editing concurrently, syncing now and then.
/// Returns the updates in the order they were made (a causal order).
fn random_history(rng: &mut StdRng, clients: u64, steps: usize) -> Vec<Vec<u8>> {
    let replicas: Vec<Doc> = (1..=clients).map(Doc::with_client_id).collect();
    let mut all: Vec<Vec<u8>> = Vec::new();
    for _ in 0..steps {
        let who = rng.gen_range(0..replicas.len());
        if rng.gen_bool(0.3) {
            // Catch this replica up with everything so far.
            let mut t = replicas[who].transact_mut();
            for u in &all {
                t.apply_update(Update::decode_v1(u).unwrap()).unwrap();
            }
        }
        let u = random_edit(rng, &replicas[who]);
        all.push(u);
    }
    all
}

#[test]
fn random_histories_digest_the_same_however_delivered() {
    let mut complete = 0;
    for seed in 0..40_u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let h = random_history(&mut rng, 3, 25);
        let refs: Vec<&[u8]> = h.iter().map(Vec::as_slice).collect();
        let forward = doc_from(&refs);
        assert!(!forward.transact().has_missing_updates(), "seed {seed}");
        let expected = document_digest(&forward);

        // Merged into one update; compacted; snapshot of a prefix + the tail.
        let merged = yrs::merge_updates_v1(&refs).unwrap();
        assert_eq!(
            document_digest(&doc_from(&[&merged])),
            expected,
            "merged, seed {seed}"
        );
        assert_eq!(
            document_digest(&doc_from(&[&compacted(&forward)])),
            expected,
            "compacted, seed {seed}"
        );
        let cut = rng.gen_range(0..refs.len());
        let mut split: Vec<Vec<u8>> = vec![compacted(&doc_from(&refs[..cut]))];
        split.extend(refs[cut..].iter().map(|u| u.to_vec()));
        let split_refs: Vec<&[u8]> = split.iter().map(Vec::as_slice).collect();
        assert_eq!(
            document_digest(&doc_from(&split_refs)),
            expected,
            "split, seed {seed}"
        );

        // Shuffled deliveries: every one that integrates completely agrees. (yrs 0.28 can
        // leave structs pending for some non-causal orders, even after re-delivery; such a
        // document really is different, and an import rejects it as incomplete_history.)
        for _ in 0..5 {
            let mut order: Vec<&[u8]> = refs.clone();
            order.shuffle(&mut rng);
            for doc in [
                doc_from(&order),
                doc_from(&[order.clone(), refs.clone()].concat()),
            ] {
                if !doc.transact().has_missing_updates() {
                    complete += 1;
                    assert_eq!(document_digest(&doc), expected, "shuffled, seed {seed}");
                }
            }
        }
        // Duplicated delivery changes nothing.
        let twice = doc_from(&[refs.clone(), refs.clone()].concat());
        assert_eq!(document_digest(&twice), expected, "twice, seed {seed}");

        // A further edit changes the digest exactly when it changes the content.
        let before_json = canonical_json(&forward.transact());
        let more = random_edit(&mut rng, &forward);
        let changed = document_digest(&forward);
        assert_eq!(
            before_json == canonical_json(&forward.transact()),
            changed == expected,
            "seed {seed}"
        );
        assert_eq!(
            document_digest(&doc_from(&[&compacted(&doc_from(&refs)), &more])),
            changed,
            "seed {seed}"
        );
    }
    assert!(
        complete >= 100,
        "only {complete} shuffled deliveries integrated completely"
    );
}

#[test]
fn different_content_different_digest() {
    let mut seen = std::collections::HashSet::new();
    for i in 0..200 {
        let doc = Doc::new();
        let m = doc.get_or_insert_map("shapes");
        {
            let mut t = doc.transact_mut();
            m.insert(&mut t, "s", i);
        }
        assert!(seen.insert(document_digest(&doc)), "collision at {i}");
    }
    // Same value as int and float renders the same (numbers are normalised to f64).
    let a = Doc::new();
    let b = Doc::new();
    {
        let ma = a.get_or_insert_map("m");
        let mb = b.get_or_insert_map("m");
        ma.insert(&mut a.transact_mut(), "n", Any::Number(yrs::Number::Int(1)));
        mb.insert(&mut b.transact_mut(), "n", 1.0_f64);
    }
    assert_eq!(document_digest(&a), document_digest(&b));
    assert_eq!(canonical_json(&a.transact()), r#"{"m":{"n":1.0}}"#);
}
