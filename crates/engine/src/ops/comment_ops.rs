//! Comment threads (the page's `opComment` / `opReply` / `opResolve` in `ops.ts`), all
//! attributed to the caller (`ctx.by` / `ctx.by_id`).

use serde_json::{Map as JsonMap, Value};
use yrs::{MapRef, TransactionMut};

use super::{anchor_of, valid_shape_id, OpsCtx};
use crate::comments::{
    clean_comment_text, create_thread, get_thread, has_entry, reply, set_resolved, shape_label,
    CommentAuthor, CommentThread,
};
use crate::geometry::Point;
use crate::jsstr::js_trim;
use crate::shape::{bare_id, live_shape};

fn author(ctx: &OpsCtx) -> CommentAuthor {
    CommentAuthor {
        id: ctx.by_id.clone(),
        name: ctx.by.clone(),
    }
}

/// `o[a] ?? o[b] ?? …`
fn first<'a>(o: &'a JsonMap<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().filter_map(|k| o.get(*k)).find(|v| !v.is_null())
}

/// `{op:"comment", on, text, id?}`: starts a thread on a shape; returns its id.
pub(super) fn op_comment(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    op: &JsonMap<String, Value>,
    ctx: &OpsCtx,
) -> Result<(String, Point), String> {
    let on = first(op, &["on", "shape", "anchor"])
        .and_then(Value::as_str)
        .filter(|s| !js_trim(s).is_empty())
        .ok_or("`comment` needs `on`: the id of the shape to comment on")?;
    let shape =
        live_shape(txn, shapes, bare_id(js_trim(on))).ok_or_else(|| format!("no shape {on}"))?;
    let id = match op.get("id") {
        None => None,
        Some(Value::String(s)) if valid_shape_id(s) => {
            if has_entry(txn, s) {
                return Err(format!("a comment thread with id {s} already exists"));
            }
            Some(s.clone())
        }
        Some(_) => return Err("`id` must be 1–64 of A–Z a–z 0–9 _ - : .".into()),
    };
    let text = clean_comment_text(op.get("text"))?;
    let thread = create_thread(
        txn,
        &shape.id,
        &text,
        &author(ctx),
        id.as_deref(),
        &shape_label(&shape),
        ctx.now_ms,
    );
    Ok((thread, anchor_of(txn, shapes, &shape.id, ctx)))
}

/// `thread` (or `id`): a thread id, `comment:` prefix allowed.
fn thread_ref(v: Option<&Value>) -> Result<String, String> {
    let t = v
        .and_then(Value::as_str)
        .map(js_trim)
        .filter(|s| !s.is_empty())
        .ok_or("`thread` must be a thread id (from canvas_read comments)")?;
    Ok(t.strip_prefix("comment:").unwrap_or(t).to_owned())
}

fn need_thread(txn: &TransactionMut, op: &JsonMap<String, Value>) -> Result<CommentThread, String> {
    let id = thread_ref(first(op, &["thread", "id"]))?;
    get_thread(txn, &id)
        .filter(CommentThread::is_shown)
        .ok_or_else(|| format!("no comment thread {id}"))
}

/// `{op:"reply", thread, text}`: adds onto a thread (a resolved one opens again).
pub(super) fn op_reply(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    op: &JsonMap<String, Value>,
    ctx: &OpsCtx,
) -> Result<String, String> {
    let t = need_thread(txn, op)?;
    let text = clean_comment_text(op.get("text"))?;
    let name = live_shape(txn, shapes, &t.anchor).map(|s| shape_label(&s));
    reply(txn, &t.id, &text, &author(ctx), name.as_deref(), ctx.now_ms)?;
    Ok(t.id)
}

/// `{op:"resolve", thread, resolved?}`: resolves (default) or reopens a thread.
pub(super) fn op_resolve(
    txn: &mut TransactionMut,
    _shapes: &MapRef,
    op: &JsonMap<String, Value>,
    ctx: &OpsCtx,
) -> Result<String, String> {
    let t = need_thread(txn, op)?;
    let resolved = match op.get("resolved") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("`resolved` must be true or false".into()),
    };
    set_resolved(txn, &t.id, resolved, &author(ctx), ctx.now_ms);
    Ok(t.id)
}
