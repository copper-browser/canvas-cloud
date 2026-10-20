//! Comment threads on shapes — the page's `canvas/comments.ts`, kept in their own top-level
//! map of the canvas document, never in a shape:
//!
//! ```text
//! comments: Y.Map<threadId, Y.Map{
//!   id, anchor (shape id), anchorName (what the shape was called, for when it is gone),
//!   by, byId, at,                                  // who started it, when (ms)
//!   resolved: boolean, resolvedBy?, resolvedById?, resolvedAt?,
//!   messages: Y.Array<Y.Map{id, by, byId, at, text, editedAt?, deleted?}>
//! }>
//! ```
//!
//! Every write sets only its own keys: a reply is one push onto `messages`, resolving sets
//! three keys on the thread. A deleted message is a tombstone; a thread whose every message
//! is deleted stops showing. A thread outlives its shape (read as `anchorDeleted`).

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use yrs::{
    Any, Array, ArrayPrelim, ArrayRef, Map, MapPrelim, MapRef, Out, ReadTxn, TransactionMut,
    WriteTxn,
};

use crate::jsstr::{js_regex, js_trim, js_trim_end};
use crate::schema::{js_len, js_slice, out_to_json, ser_num, ShapeType};
use crate::shape::{Shape, View};

/// The document's root map of comment threads.
pub const COMMENTS_KEY: &str = "comments";
/// Characters (UTF-16 units) in one message.
pub const MAX_COMMENT: usize = 10_000;

/// Who wrote a message (or resolved a thread).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentAuthor {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommentMessage {
    pub id: String,
    pub by: String,
    pub by_id: String,
    pub at: f64,
    pub text: String,
    pub edited_at: Option<f64>,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommentThread {
    pub id: String,
    pub anchor: String,
    pub anchor_name: String,
    pub by: String,
    pub by_id: String,
    pub at: f64,
    pub resolved: bool,
    pub resolved_by: Option<String>,
    pub resolved_by_id: Option<String>,
    pub resolved_at: Option<f64>,
    /// In order, tombstones included.
    pub messages: Vec<CommentMessage>,
}

impl CommentThread {
    /// A thread shows while any of its messages does.
    pub fn is_shown(&self) -> bool {
        self.messages.iter().any(|m| !m.deleted)
    }
}

fn finite(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|f| f.is_finite())
}

fn str_of(v: Option<&Value>) -> &str {
    v.and_then(Value::as_str).unwrap_or("")
}

fn named(by: &str) -> String {
    js_slice(js_trim(by), 120).to_owned()
}

/// Tolerant read of one message (another client, an agent, an older page).
pub fn read_comment_message(v: &Value) -> Option<CommentMessage> {
    let o = v.as_object()?;
    let id = o
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let by = named(str_of(o.get("by")));
    let mut msg = CommentMessage {
        id: id.to_owned(),
        by: if by.is_empty() { "Someone".into() } else { by },
        by_id: str_of(o.get("byId")).to_owned(),
        at: finite(o.get("at")).unwrap_or(0.0),
        text: js_slice(str_of(o.get("text")), MAX_COMMENT).to_owned(),
        edited_at: finite(o.get("editedAt")),
        deleted: false,
    };
    if o.get("deleted") == Some(&Value::Bool(true)) {
        msg.deleted = true;
        msg.text = String::new();
    }
    Some(msg)
}

fn any_str<T: ReadTxn>(txn: &T, m: &MapRef, k: &str) -> Option<String> {
    match m.get(txn, k) {
        Some(Out::Any(Any::String(s))) => Some(s.to_string()),
        _ => None,
    }
}

fn any_num<T: ReadTxn>(txn: &T, m: &MapRef, k: &str) -> Option<f64> {
    match m.get(txn, k) {
        Some(Out::Any(Any::Number(n))) => n.as_f64().filter(|f| f.is_finite()),
        _ => None,
    }
}

fn raw_messages<T: ReadTxn>(txn: &T, m: &MapRef) -> Vec<Value> {
    match m.get(txn, "messages") {
        Some(Out::YArray(a)) => a.iter(txn).map(|o| out_to_json(txn, &o)).collect(),
        Some(Out::Any(Any::Array(items))) => items.iter().map(crate::schema::any_to_json).collect(),
        _ => Vec::new(),
    }
}

/// Tolerant read of one thread; `None` when it is not one.
pub fn read_thread<T: ReadTxn>(txn: &T, id: &str, out: &Out) -> Option<CommentThread> {
    let Out::YMap(m) = out else { return None };
    let anchor = any_str(txn, m, "anchor").filter(|a| !a.is_empty())?;
    let mut messages: Vec<CommentMessage> = Vec::new();
    for raw in raw_messages(txn, m) {
        let Some(msg) = read_comment_message(&raw) else {
            continue;
        };
        if messages.iter().any(|x| x.id == msg.id) {
            continue;
        }
        messages.push(msg);
    }
    let by = named(&any_str(txn, m, "by").unwrap_or_default());
    let by = if by.is_empty() {
        messages
            .first()
            .map_or_else(|| "Someone".to_owned(), |x| x.by.clone())
    } else {
        by
    };
    let by_id = any_str(txn, m, "byId")
        .filter(|s| !s.is_empty())
        .or_else(|| messages.first().map(|x| x.by_id.clone()))
        .unwrap_or_default();
    let resolved = matches!(m.get(txn, "resolved"), Some(Out::Any(Any::Bool(true))));
    let mut t = CommentThread {
        id: id.to_owned(),
        anchor,
        anchor_name: js_slice(&any_str(txn, m, "anchorName").unwrap_or_default(), 200).to_owned(),
        by,
        by_id,
        at: any_num(txn, m, "at").unwrap_or_else(|| messages.first().map_or(0.0, |x| x.at)),
        resolved,
        resolved_by: None,
        resolved_by_id: None,
        resolved_at: None,
        messages,
    };
    if resolved {
        t.resolved_by = any_str(txn, m, "resolvedBy").filter(|s| !s.is_empty());
        t.resolved_by_id = any_str(txn, m, "resolvedById").filter(|s| !s.is_empty());
        t.resolved_at = any_num(txn, m, "resolvedAt");
    }
    Some(t)
}

/// One thread straight from the document.
pub fn get_thread<T: ReadTxn>(txn: &T, id: &str) -> Option<CommentThread> {
    let root = txn.get_map(COMMENTS_KEY)?;
    let out = root.get(txn, id)?;
    read_thread(txn, id, &out)
}

/// Whether the comments map has an entry (thread or not) under `id`.
pub fn has_entry<T: ReadTxn>(txn: &T, id: &str) -> bool {
    txn.get_map(COMMENTS_KEY)
        .is_some_and(|root| root.contains_key(txn, id))
}

/// Every readable thread, oldest first (then by id), tombstoned ones included.
pub fn all_threads<T: ReadTxn>(txn: &T) -> Vec<CommentThread> {
    let Some(root) = txn.get_map(COMMENTS_KEY) else {
        return Vec::new();
    };
    let mut out: Vec<CommentThread> = root
        .iter(txn)
        .filter_map(|(id, v)| read_thread(txn, id, &v))
        .collect();
    out.sort_by(|a, b| a.at.total_cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    out
}

static CRLF: LazyLock<Regex> = LazyLock::new(|| js_regex(r"\r\n?"));

/// Text as it is stored: line endings normalised, ends trimmed. Errs when empty or too long.
pub fn clean_comment_text(raw: Option<&Value>) -> Result<String, String> {
    let s = raw
        .and_then(Value::as_str)
        .ok_or("`text` must be a string")?;
    let normal = CRLF.replace_all(s, "\n");
    let text = js_trim(&normal);
    if text.is_empty() {
        return Err("`text` is empty".into());
    }
    if js_len(text) > MAX_COMMENT {
        return Err(format!("a comment holds at most {MAX_COMMENT} characters"));
    }
    Ok(text.to_owned())
}

static LABEL_LEAD: LazyLock<Regex> =
    LazyLock::new(|| js_regex(r"^\s*(?:#{1,6}\s+|[-*]\s+(?:\[[ xX]\]\s+)?|>\s?)"));
static LABEL_MARKS: LazyLock<Regex> = LazyLock::new(|| js_regex(r"[*_`~]"));

/// What a shape is called in a comment's heading: a frame's title, the first line of a
/// note, a checklist's or link's title, an arrow's label.
pub fn shape_label(s: &Shape) -> String {
    let first_line = |t: &str| -> String {
        t.split('\n')
            .map(|l| {
                let l = LABEL_LEAD.replace(l, "");
                js_trim(&LABEL_MARKS.replace_all(&l, "")).to_owned()
            })
            .find(|l| !l.is_empty())
            .unwrap_or_default()
    };
    let clip = |t: &str| -> String {
        if js_len(t) > 80 {
            format!("{}…", js_trim_end(js_slice(t, 79)))
        } else {
            t.to_owned()
        }
    };
    let or = |s: String, d: &str| if s.is_empty() { d.to_owned() } else { s };
    match s.kind {
        ShapeType::Frame => or(clip(js_trim(&s.title)), "Frame"),
        ShapeType::Sticky => match s.view {
            Some(View::Checklist) => or(clip(js_trim(&s.title)), "Checklist"),
            Some(View::Timeline) => or(clip(js_trim(&s.title)), "Timeline"),
            None => or(clip(&first_line(&s.text)), "Note"),
        },
        ShapeType::Text => or(clip(&first_line(&s.text)), "Text"),
        ShapeType::Checklist => or(clip(js_trim(&s.title)), "Checklist"),
        ShapeType::Link => {
            let t = js_trim(&s.title);
            or(clip(if t.is_empty() { &s.url } else { t }), "Link")
        }
        ShapeType::Image => "Image".into(),
        ShapeType::Arrow => or(clip(js_trim(&s.label)), "Arrow"),
    }
}

/// A new comment or message id (`crypto.randomUUID()` on the page).
pub fn new_comment_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn comments_root(txn: &mut TransactionMut) -> MapRef {
    txn.get_or_insert_map(COMMENTS_KEY)
}

fn write_message(
    txn: &mut TransactionMut,
    arr: &ArrayRef,
    id: &str,
    text: &str,
    by: &CommentAuthor,
    at: f64,
) -> MapRef {
    let m = arr.push_back(txn, MapPrelim::default());
    m.insert(txn, "id", id);
    m.insert(txn, "by", by.name.as_str());
    m.insert(txn, "byId", by.id.as_str());
    m.insert(txn, "at", crate::schema::num_any(at));
    m.insert(txn, "text", text);
    m
}

/// Starts a thread on `anchor` with its first message; returns the thread id. `text` must
/// already be clean ([`clean_comment_text`]).
pub fn create_thread(
    txn: &mut TransactionMut,
    anchor: &str,
    text: &str,
    by: &CommentAuthor,
    id: Option<&str>,
    anchor_name: &str,
    now: i64,
) -> String {
    let root = comments_root(txn);
    let thread_id = match id {
        Some(id) if !root.contains_key(txn, id) => id.to_owned(),
        _ => new_comment_id(),
    };
    #[allow(clippy::cast_precision_loss)]
    let at = now as f64;
    let t = root.insert(txn, thread_id.as_str(), MapPrelim::default());
    t.insert(txn, "id", thread_id.as_str());
    t.insert(txn, "anchor", anchor);
    t.insert(txn, "anchorName", js_slice(anchor_name, 200));
    t.insert(txn, "by", by.name.as_str());
    t.insert(txn, "byId", by.id.as_str());
    t.insert(txn, "at", crate::schema::num_any(at));
    t.insert(txn, "resolved", false);
    let messages = t.insert(txn, "messages", ArrayPrelim::default());
    write_message(txn, &messages, &new_comment_id(), text, by, at);
    thread_id
}

fn write_resolved(
    txn: &mut TransactionMut,
    t: &MapRef,
    resolved: bool,
    by: &CommentAuthor,
    at: f64,
) {
    t.insert(txn, "resolved", resolved);
    if resolved {
        t.insert(txn, "resolvedBy", by.name.as_str());
        t.insert(txn, "resolvedById", by.id.as_str());
        t.insert(txn, "resolvedAt", crate::schema::num_any(at));
    } else {
        for k in ["resolvedBy", "resolvedById", "resolvedAt"] {
            if t.contains_key(txn, k) {
                t.remove(txn, k);
            }
        }
    }
}

fn thread_map(txn: &mut TransactionMut, thread_id: &str) -> Option<MapRef> {
    match txn.get_map(COMMENTS_KEY)?.get(txn, thread_id) {
        Some(Out::YMap(m)) => Some(m),
        _ => None,
    }
}

/// Adds onto a thread; returns the new message's id. A resolved thread opens again.
pub fn reply(
    txn: &mut TransactionMut,
    thread_id: &str,
    text: &str,
    by: &CommentAuthor,
    anchor_name: Option<&str>,
    now: i64,
) -> Result<String, String> {
    let t = thread_map(txn, thread_id).ok_or_else(|| format!("no comment thread {thread_id}"))?;
    #[allow(clippy::cast_precision_loss)]
    let at = now as f64;
    let id = new_comment_id();
    let messages = if let Some(Out::YArray(a)) = t.get(txn, "messages") {
        a
    } else {
        {
            // Written as a plain list by someone else: carried over once, in order.
            let before: Vec<CommentMessage> = raw_messages(txn, &t)
                .iter()
                .filter_map(read_comment_message)
                .collect();
            let arr = t.insert(txn, "messages", ArrayPrelim::default());
            for m in before {
                let mm = write_message(
                    txn,
                    &arr,
                    &m.id,
                    &m.text,
                    &CommentAuthor {
                        id: m.by_id.clone(),
                        name: m.by.clone(),
                    },
                    m.at,
                );
                if let Some(e) = m.edited_at.filter(|e| *e != 0.0) {
                    mm.insert(txn, "editedAt", crate::schema::num_any(e));
                }
                if m.deleted {
                    mm.insert(txn, "deleted", true);
                }
            }
            arr
        }
    };
    write_message(txn, &messages, &id, text, by, at);
    if let Some(name) = anchor_name.filter(|n| !n.is_empty()) {
        t.insert(txn, "anchorName", js_slice(name, 200));
    }
    if matches!(t.get(txn, "resolved"), Some(Out::Any(Any::Bool(true)))) {
        write_resolved(txn, &t, false, by, at);
    }
    Ok(id)
}

/// Resolves (true) or reopens (false). False when there is no such thread.
pub fn set_resolved(
    txn: &mut TransactionMut,
    thread_id: &str,
    resolved: bool,
    by: &CommentAuthor,
    now: i64,
) -> bool {
    let Some(t) = thread_map(txn, thread_id) else {
        return false;
    };
    let was = matches!(t.get(txn, "resolved"), Some(Out::Any(Any::Bool(true))));
    if was != resolved {
        #[allow(clippy::cast_precision_loss)]
        write_resolved(txn, &t, resolved, by, now as f64);
    }
    true
}

/// One message as an agent reads it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MessageSummary {
    pub id: String,
    pub by: String,
    #[serde(serialize_with = "ser_num")]
    pub at: f64,
    pub text: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
}

/// One comment thread as an agent reads it: deleted messages left out.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommentSummary {
    pub id: String,
    /// The shape it is on.
    pub anchor: String,
    /// Set when that shape has been deleted (the thread stays).
    #[serde(rename = "anchorDeleted", skip_serializing_if = "std::ops::Not::not")]
    pub anchor_deleted: bool,
    pub resolved: bool,
    #[serde(rename = "resolvedBy", skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    pub messages: Vec<MessageSummary>,
}

/// The read format of one thread; `clip` shortens texts.
pub fn summarize_thread(
    t: &CommentThread,
    anchor_exists: bool,
    clip: &dyn Fn(&str) -> String,
) -> CommentSummary {
    CommentSummary {
        id: t.id.clone(),
        anchor: t.anchor.clone(),
        anchor_deleted: !anchor_exists,
        resolved: t.resolved,
        resolved_by: if t.resolved {
            t.resolved_by.clone()
        } else {
            None
        },
        messages: t
            .messages
            .iter()
            .filter(|m| !m.deleted)
            .map(|m| MessageSummary {
                id: m.id.clone(),
                by: m.by.clone(),
                at: m.at,
                text: clip(&m.text),
                edited: m.edited_at.is_some_and(|e| e != 0.0),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use yrs::{Doc, Transact};

    fn ann() -> CommentAuthor {
        CommentAuthor {
            id: "u-ann".into(),
            name: "Ann".into(),
        }
    }

    #[test]
    fn text() {
        assert_eq!(
            clean_comment_text(Some(&json!(" a\r\nb\rc "))).unwrap(),
            "a\nb\nc"
        );
        assert_eq!(
            clean_comment_text(Some(&json!("  "))).unwrap_err(),
            "`text` is empty"
        );
        assert_eq!(
            clean_comment_text(Some(&json!(1))).unwrap_err(),
            "`text` must be a string"
        );
        assert_eq!(
            clean_comment_text(None).unwrap_err(),
            "`text` must be a string"
        );
        assert!(clean_comment_text(Some(&json!("x".repeat(10_001)))).is_err());
    }

    #[test]
    fn threads() {
        let doc = Doc::new();
        let id = {
            let mut txn = doc.transact_mut();
            let id = create_thread(&mut txn, "s1", "hello", &ann(), Some("t1"), "Note", 1000);
            assert_eq!(id, "t1");
            let again = create_thread(&mut txn, "s1", "x", &ann(), Some("t1"), "Note", 1000);
            assert_ne!(again, "t1");
            assert!(set_resolved(&mut txn, "t1", true, &ann(), 2000));
            id
        };
        {
            let txn = doc.transact();
            let t = get_thread(&txn, &id).unwrap();
            assert!(t.resolved);
            assert_eq!(t.resolved_by.as_deref(), Some("Ann"));
            assert_eq!(t.messages.len(), 1);
            assert_eq!(all_threads(&txn).len(), 2);
        }
        {
            let mut txn = doc.transact_mut();
            reply(&mut txn, "t1", "again", &ann(), Some("Renamed"), 3000).unwrap();
        }
        let txn = doc.transact();
        let t = get_thread(&txn, "t1").unwrap();
        assert!(!t.resolved);
        assert_eq!(t.resolved_by, None);
        assert_eq!(t.anchor_name, "Renamed");
        assert_eq!(t.messages.len(), 2);
        let s = summarize_thread(&t, false, &|s| s.to_owned());
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["anchorDeleted"], json!(true));
        assert_eq!(v["messages"][1]["text"], "again");
        assert!(v["messages"][1].get("edited").is_none());
    }
}
