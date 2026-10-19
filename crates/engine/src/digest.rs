//! The canonical document digest of CONTRACT §6.3.
//!
//! An import answers with the digest of what it stored, and `GET
//! /v1/apps/self/canvases/{id}/digest` recomputes it from the database, so a migrator can
//! prove that a canvas arrived intact and recognise one that already did. The algorithm is
//! byte-for-byte the normative reference implementation that import producers test
//! against (CONTRACT §6.3); any change here breaks every recorded migration digest.
//!
//! ```text
//! document_digest(doc) = lower-case hex( SHA-256( canonical_json(doc) as UTF-8 bytes ) )
//! ```
//!
//! `canonical_json(doc)` is one JSON object with a member per **root type** of the document
//! (`txn.root_refs()`), written with no whitespace at all:
//!
//! 1. **Roots.** For every root `(name, value)`:
//!    * a typed root (`get_or_insert_map/array/text/xml_*` was called on this `Doc`) renders
//!      by its type;
//!    * an *undefined* root (the `Doc` only received updates — Yjs updates carry no root
//!      types) is inferred from its content, so a server that never touched the root and one
//!      that did agree: map entries present → **map**; else a non-empty
//!      `TextRef::get_string` → **text**; else list content (`ArrayRef::len > 0`) →
//!      **array**; else **empty**;
//!    * roots whose rendering is empty (`{}`, `[]` or `""`) are **omitted** — a deleted-only
//!      root may or may not survive snapshot compaction, and an empty root means nothing to a
//!      client;
//!    * members are ordered by root name (byte-wise UTF-8).
//! 2. **Values.**
//!    * `Y.Map` and `Any::Map` → object, members sorted by key (byte-wise UTF-8); deleted
//!      entries are not present;
//!    * `Y.Array` and `Any::Array` → array, document order;
//!    * `Y.Text` → its plain string (`get_string`: formatting attributes and embeds ignored);
//!    * `Y.XmlElement`, `Y.XmlFragment`, `Y.XmlText` → their `get_string` (XML string);
//!    * sub-document → `{"guid":"<guid>"}`;
//!    * `Any::Null`, `Any::Undefined` → `null`; `Any::Bool` → `true`/`false`;
//!    * `Any::Number` (both `Int(i64)` and `Float(f64)`) → the value as `f64`, rendered by
//!      `serde_json` (`serde_json::Number::from_f64(v).to_string()`: shortest round trip,
//!      always with a fraction or exponent — `1` → `1.0`, `0.5` → `0.5`, `1e21` → `1e21`);
//!      NaN/±∞ → `null`;
//!    * `Any::String` → JSON string, escaped exactly as `serde_json::to_string(&str)` does;
//!    * `Any::Buffer` → JSON string of standard base64 **with** padding (RFC 4648 §4);
//!    * anything else (weak links) → `null`.
//!
//! An empty document is `{}`, so its digest is SHA-256 of those two bytes
//! ([`EMPTY_DOCUMENT_DIGEST`]), not of the empty string.
//!
//! Properties (tested): the same set of updates, applied **completely** (no pending structs),
//! yields the same digest regardless of delivery order, of snapshot compaction
//! (`encode_state_as_update_v1` of the merged state) and of whether the reader typed its
//! roots; any change of content changes it.

use std::fmt::Write as _;

use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use yrs::types::xml::XmlTextRef;
use yrs::types::GetString as _;
use yrs::{Any, Array as _, ArrayRef, Doc, Map as _, MapRef, Out, ReadTxn, TextRef, Transact as _};

/// Digest of an empty document (SHA-256 of `{}`).
pub const EMPTY_DOCUMENT_DIGEST: &str =
    "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";

/// Lower-case hex SHA-256 of [`canonical_json`] of the whole document (see the module docs).
pub fn document_digest(doc: &Doc) -> String {
    let txn = doc.transact();
    digest_txn(&txn)
}

/// [`document_digest`] inside an existing transaction.
pub fn digest_txn<T: ReadTxn>(txn: &T) -> String {
    hex::encode(Sha256::digest(canonical_json(txn).as_bytes()))
}

/// Canonical JSON rendering of the whole document (see the module docs).
pub fn canonical_json<T: ReadTxn>(txn: &T) -> String {
    let mut roots: Vec<(String, String)> = txn
        .root_refs()
        .map(|(name, value)| (name.to_owned(), render_root(txn, &value)))
        .filter(|(_, v)| !matches!(v.as_str(), "{}" | "[]" | "\"\""))
        .collect();
    roots.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut out = String::from("{");
    for (i, (name, value)) in roots.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(name));
        out.push(':');
        out.push_str(value);
    }
    out.push('}');
    out
}

fn render_root<T: ReadTxn>(txn: &T, value: &Out) -> String {
    let Out::UndefinedRef(ptr) = value else {
        return render_out(txn, value);
    };
    let ptr = *ptr;
    let map = MapRef::from(ptr);
    if map.len(txn) > 0 {
        return render_map(txn, &map);
    }
    let text = TextRef::from(ptr).get_string(txn);
    if !text.is_empty() {
        return json_string(&text);
    }
    let array = ArrayRef::from(ptr);
    if array.len(txn) > 0 {
        return render_array(txn, &array);
    }
    "{}".to_owned()
}

fn render_out<T: ReadTxn>(txn: &T, value: &Out) -> String {
    #[allow(unreachable_patterns)] // `Out::YWeakLink` exists only with yrs' `weak` feature
    match value {
        Out::Any(any) => render_any(any),
        Out::YMap(map) => render_map(txn, map),
        Out::YArray(array) => render_array(txn, array),
        Out::YText(text) => json_string(&text.get_string(txn)),
        Out::YXmlElement(x) => json_string(&x.get_string(txn)),
        Out::YXmlFragment(x) => json_string(&x.get_string(txn)),
        Out::YXmlText(x) => json_string(&XmlTextRef::get_string(x, txn)),
        Out::YDoc(doc) => {
            let guid = doc.guid();
            format!("{{\"guid\":{}}}", json_string(guid.as_ref()))
        }
        Out::UndefinedRef(_) => render_root(txn, value),
        _ => "null".to_owned(),
    }
}

fn render_map<T: ReadTxn>(txn: &T, map: &MapRef) -> String {
    let mut entries: Vec<(String, String)> = map
        .iter(txn)
        .map(|(k, v)| (k.to_owned(), render_out(txn, &v)))
        .collect();
    object(&mut entries)
}

fn render_array<T: ReadTxn>(txn: &T, array: &ArrayRef) -> String {
    let items: Vec<String> = array.iter(txn).map(|v| render_out(txn, &v)).collect();
    format!("[{}]", items.join(","))
}

fn render_any(any: &Any) -> String {
    match any {
        Any::Null | Any::Undefined => "null".to_owned(),
        Any::Bool(b) => b.to_string(),
        Any::Number(n) => {
            #[allow(clippy::cast_precision_loss)] // the contract normalises every number to f64
            let v = match *n {
                yrs::Number::Int(i) => i as f64,
                yrs::Number::Float(f) => f,
            };
            serde_json::Number::from_f64(v).map_or_else(|| "null".to_owned(), |n| n.to_string())
        }
        Any::String(s) => json_string(s),
        Any::Buffer(b) => json_string(&base64::engine::general_purpose::STANDARD.encode(b)),
        Any::Array(items) => {
            let items: Vec<String> = items.iter().map(render_any).collect();
            format!("[{}]", items.join(","))
        }
        Any::Map(m) => {
            let mut entries: Vec<(String, String)> =
                m.iter().map(|(k, v)| (k.clone(), render_any(v))).collect();
            object(&mut entries)
        }
    }
}

fn object(entries: &mut [(String, String)]) -> String {
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut out = String::from("{");
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}:{v}", json_string(k));
    }
    out.push('}');
    out
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).expect("a str always serialises")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document() {
        let doc = Doc::new();
        assert_eq!(canonical_json(&doc.transact()), "{}");
        assert_eq!(document_digest(&doc), EMPTY_DOCUMENT_DIGEST);
        assert_eq!(
            EMPTY_DOCUMENT_DIGEST,
            hex::encode(Sha256::digest(b"{}")),
            "the constant is SHA-256 of \"{{}}\""
        );
    }
}
