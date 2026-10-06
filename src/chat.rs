//! Canvas chat (0.6.0) as the server sees it: a top-level `Y.Array` named `chat` in the canvas
//! document, owned and written by Copper. Each element is a plain object
//! `{id, authorId, authorName, text, mentions: [userId], at, editedAt?, deleted?}` (`at` and
//! `editedAt` are ms epoch). The server never writes it: rooms persist and relay it like any
//! other part of the document, and `GET /canvases/{id}/read` includes the most recent messages
//! (see [`read_chat`]). Mention notifications are in [`crate::mentions`].

use serde::Serialize;
use serde_json::Value;
use yrs::{Array as _, ReadTxn};

use crate::read::{ReadResult, READ_TEXT_LIMIT};
use crate::schema::{js_len, js_slice, json_f64, out_to_json, ser_num, ser_opt_num};

/// Name of the document root that holds the chat.
pub const CHAT: &str = "chat";
/// Messages `GET /canvases/{id}/read` returns (the most recent ones).
pub const READ_CHAT_LIMIT: usize = 50;

/// One chat message in the read format.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatSummary {
    pub id: String,
    #[serde(rename = "authorId")]
    pub author_id: String,
    #[serde(rename = "authorName")]
    pub author_name: String,
    /// Cut to 500 characters plus `…` unless `full`.
    pub text: String,
    /// User ids mentioned in the message.
    pub mentions: Vec<String>,
    /// When it was sent (ms epoch).
    #[serde(serialize_with = "ser_num")]
    pub at: f64,
    #[serde(
        rename = "editedAt",
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_num"
    )]
    pub edited_at: Option<f64>,
}

/// `GET /canvases/{id}/read`: the [`ReadResult`] plus `chat` (oldest first).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReadWithChat {
    #[serde(flatten)]
    pub read: ReadResult,
    pub chat: Vec<ChatSummary>,
}

fn string(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn clip(s: &str, full: bool) -> String {
    if full || js_len(s) <= READ_TEXT_LIMIT {
        s.to_owned()
    } else {
        format!("{}…", js_slice(s, READ_TEXT_LIMIT))
    }
}

/// One stored message, or `None` when it is deleted or not a message at all (readers are
/// tolerant: anything without a string `id` is skipped, missing fields read as empty).
fn summarize(v: &Value, full: bool) -> Option<ChatSummary> {
    let id = v.get("id")?.as_str().filter(|s| !s.is_empty())?;
    if v.get("deleted").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    Some(ChatSummary {
        id: id.to_owned(),
        author_id: string(v, "authorId"),
        author_name: string(v, "authorName"),
        text: clip(&string(v, "text"), full),
        mentions: v
            .get("mentions")
            .and_then(Value::as_array)
            .map(|m| {
                m.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        at: v.get("at").and_then(json_f64).unwrap_or(0.0),
        edited_at: v.get("editedAt").and_then(json_f64),
    })
}

/// The last `limit` messages of the document's `chat` array, oldest first, in array order
/// (the order every client shows). Deleted messages are left out; text is cut to 500
/// characters unless `full`. Empty when the document has no chat.
pub fn read_chat<T: ReadTxn>(txn: &T, limit: usize, full: bool) -> Vec<ChatSummary> {
    let Some(chat) = txn.get_array(CHAT) else {
        return Vec::new();
    };
    let all: Vec<Value> = chat.iter(txn).map(|o| out_to_json(txn, &o)).collect();
    let mut out: Vec<ChatSummary> = all
        .iter()
        .rev()
        .filter_map(|v| summarize(v, full))
        .take(limit)
        .collect();
    out.reverse();
    out
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::schema::json_to_any;
    use serde_json::json;
    use yrs::{Doc, Transact as _, WriteTxn as _};

    fn push(doc: &Doc, v: &Value) {
        let mut txn = doc.transact_mut();
        let chat = txn.get_or_insert_array(CHAT);
        chat.push_back(&mut txn, json_to_any(v));
    }

    #[test]
    fn reads_the_tail_and_skips_deleted_and_junk() {
        let doc = Doc::new();
        assert!(
            read_chat(&doc.transact(), 50, false).is_empty(),
            "no chat yet"
        );
        for i in 0..60 {
            push(
                &doc,
                &json!({"id": format!("m{i}"), "authorId": "u1", "authorName": "Ann",
                        "text": format!("hello {i}"), "mentions": ["u2"], "at": 1_000 + i}),
            );
        }
        push(
            &doc,
            &json!({"id": "gone", "text": "x", "at": 2000, "deleted": true}),
        );
        push(&doc, &json!("not a message"));
        push(&doc, &json!({"text": "no id"}));
        push(
            &doc,
            &json!({"id": "long", "authorName": "Ben", "text": "y".repeat(800), "at": 3000,
                    "editedAt": 3001.5}),
        );

        let txn = doc.transact();
        let got = read_chat(&txn, 50, false);
        assert_eq!(got.len(), 50);
        assert_eq!(got[0].id, "m11", "oldest of the last 50");
        assert_eq!(got[48].id, "m59");
        let last = &got[49];
        assert_eq!(last.id, "long");
        assert_eq!(js_len(&last.text), READ_TEXT_LIMIT + 1);
        assert_eq!(last.author_id, "", "missing fields read as empty");
        assert_eq!(last.mentions.len(), 0);
        assert_eq!(last.edited_at, Some(3001.5));
        assert_eq!(got[48].mentions, vec!["u2".to_owned()]);

        let full = read_chat(&txn, 2, true);
        assert_eq!(full.len(), 2);
        assert_eq!(full[1].text.len(), 800);

        let v = serde_json::to_value(&got[48]).unwrap();
        assert_eq!(
            v,
            json!({"id": "m59", "authorId": "u1", "authorName": "Ann", "text": "hello 59",
                   "mentions": ["u2"], "at": 1059})
        );
    }
}
