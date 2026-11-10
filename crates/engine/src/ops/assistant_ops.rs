//! Assistants: agents that live on a canvas and that people can chat with (the `assistants`
//! root map). Unlike the `agents` presence map, an entry stays until it is removed.

use serde_json::{json, Map as JsonMap, Value};
use yrs::{Map, ReadTxn, TransactionMut, WriteTxn};

use super::{http_url, valid_shape_id, OpsCtx};
use crate::jsstr::{collapse_space, js_trim};
use crate::schema::{js_len, json_f64, json_to_any, out_to_json, ASSISTANTS};

/// Most assistants on one canvas.
pub const MAX_ASSISTANTS: u32 = 8;
/// Longest assistant name (UTF-16 units, as every length here).
pub const MAX_ASSISTANT_NAME: usize = 60;
/// Longest `about` line.
pub const MAX_ASSISTANT_ABOUT: usize = 140;
const MAX_URL: usize = 2048;

/// An optional `http(s)` URL (`null` = not given), normalised, at most [`MAX_URL`] long.
fn url_prop(op: &JsonMap<String, Value>, key: &str) -> Result<Option<String>, String> {
    match op.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .and_then(http_url)
            .filter(|u| js_len(u) <= MAX_URL)
            .map(Some)
            .ok_or_else(|| {
                format!("`{key}` must be an http(s) URL of at most {MAX_URL} characters")
            }),
    }
}

/// `{op:"assistant", id, name, chat, avatar?, about?}` writes the whole entry (a known id
/// keeps its `addedAt`); `{op:"assistant", id, remove:true}` deletes it. Returns the id.
pub(super) fn op_assistant(
    txn: &mut TransactionMut,
    op: &JsonMap<String, Value>,
    ctx: &OpsCtx,
) -> Result<String, String> {
    let id = match op.get("id") {
        Some(Value::String(s)) if valid_shape_id(s) => s.as_str(),
        _ => return Err("`id` must be 1–64 of A–Z a–z 0–9 _ - : .".into()),
    };
    match op.get("remove") {
        None | Some(Value::Null | Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            // Removing one that is not there succeeds.
            if let Some(map) = txn.get_map(ASSISTANTS) {
                map.remove(txn, id);
            }
            return Ok(id.to_owned());
        }
        Some(_) => return Err("`remove` must be true or false".into()),
    }
    let name = op
        .get("name")
        .and_then(Value::as_str)
        .map(js_trim)
        .filter(|n| (1..=MAX_ASSISTANT_NAME).contains(&js_len(n)))
        .ok_or_else(|| format!("`name` must be 1–{MAX_ASSISTANT_NAME} characters"))?;
    let chat = url_prop(op, "chat")?
        .ok_or("`assistant` needs `chat`: the http(s) URL where a chat with it opens")?;
    let avatar = url_prop(op, "avatar")?;
    // One line; empty = none.
    let about = match op.get("about") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => js_trim(&collapse_space(s)).to_owned(),
        Some(_) => return Err("`about` must be a string".into()),
    };
    if js_len(&about) > MAX_ASSISTANT_ABOUT {
        return Err(format!(
            "`about` must be at most {MAX_ASSISTANT_ABOUT} characters"
        ));
    }

    let map = txn.get_or_insert_map(ASSISTANTS);
    let prev = map.get(txn, id).map(|o| out_to_json(txn, &o));
    if prev.is_none() && map.len(txn) >= MAX_ASSISTANTS {
        return Err(format!(
            "at most {MAX_ASSISTANTS} assistants per canvas (remove one first)"
        ));
    }
    let added_at = prev
        .as_ref()
        .and_then(|p| p.get("addedAt"))
        .filter(|v| json_f64(v).is_some())
        .cloned()
        .unwrap_or_else(|| json!(ctx.now_ms));
    let mut value = json!({ "name": name, "chat": chat, "addedAt": added_at });
    if let Some(avatar) = avatar {
        value["avatar"] = json!(avatar);
    }
    if !about.is_empty() {
        value["about"] = json!(about);
    }
    map.insert(txn, id, json_to_any(&value));
    Ok(id.to_owned())
}
