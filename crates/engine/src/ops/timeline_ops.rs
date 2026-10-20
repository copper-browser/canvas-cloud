//! The timeline card: a note in the timeline view whose text is the timeline (the page's
//! `addTimelineNote` / `updateTimelineNote` in `ops.ts`).
//!
//! Item refs are tried on a copy of the text first: an op writes all of its edits or none.

use serde_json::{Map as JsonMap, Value};
use yrs::{Map, MapRef, Out, TextPrelim, TextRef, TransactionMut};

use super::{clean_props, create, need_shape_map, num, place_props, store_update, OpsCtx};
use crate::checklist::js_entries;
use crate::geometry::{center, BoxF, Point};
use crate::jsstr::{quoted, YText};
use crate::schema::{js_slice, num_any, ShapeType, MAX_TITLE};
use crate::shape::{store_value, Shape, View};
use crate::timeline::{
    apply_timeline_edits, missing_refs_message, needed_height, read_item_patch, read_items_input,
    read_step_input, step_for, TimelineEdits, TIMELINE_MIN, TIMELINE_SIZE,
};
use crate::timeline_text::{
    add_months, clean_iso, day_of, timeline_markdown, ItemFields, VIEW_TIMELINE,
};

const TIMELINE_KEYS: [&str; 9] = [
    "view",
    "type",
    "title",
    "start",
    "end",
    "step",
    "items",
    "addItems",
    "editItems",
];

struct TimelineInput {
    props: JsonMap<String, Value>,
    /// `Some(true)`: `view:"timeline"`; `Some(false)`: back to a plain note.
    view: Option<bool>,
    edits: TimelineEdits,
}

fn clean_date(v: &Value, key: &str) -> Result<String, String> {
    v.as_str()
        .and_then(clean_iso)
        .ok_or_else(|| format!("`{key}` must be a YYYY-MM-DD date"))
}

/// A timeline's fields and the rest (a note's box, colour, text), checked.
fn clean_timeline(
    raw: &JsonMap<String, Value>,
    txn: &TransactionMut,
    shapes: &MapRef,
    self_id: Option<&str>,
    ctx: &OpsCtx,
) -> Result<TimelineInput, String> {
    let rest: JsonMap<String, Value> = raw
        .iter()
        .filter(|(k, _)| !TIMELINE_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut out = TimelineInput {
        props: clean_props(ShapeType::Sticky, &rest, txn, shapes, self_id, ctx)?,
        view: None,
        edits: TimelineEdits::default(),
    };
    if let Some(v) = raw.get("view") {
        out.view = Some(match v {
            Value::String(s) if s == VIEW_TIMELINE => true,
            Value::Null | Value::Bool(false) => false,
            Value::String(s) if s.is_empty() || s == "note" => false,
            _ => return Err("`view` must be \"timeline\", \"checklist\" or null".into()),
        });
    }
    let e = &mut out.edits;
    if let Some(t) = raw.get("title") {
        let t = t.as_str().ok_or("`title` must be a string")?;
        e.title = Some(js_slice(t, MAX_TITLE).to_owned());
    }
    if let Some(v) = raw.get("start") {
        e.start = Some(clean_date(v, "start")?);
    }
    if let Some(v) = raw.get("end") {
        e.end = Some(clean_date(v, "end")?);
    }
    if let Some(v) = raw.get("step") {
        e.step = Some(read_step_input(v, "step")?);
    }
    if let Some(v) = raw.get("items") {
        e.items = Some(read_items_input(v, "items")?);
    }
    if let Some(v) = raw.get("addItems") {
        e.add = Some(read_items_input(v, "addItems")?);
    }
    if let Some(v) = raw.get("editItems") {
        let o = v.as_object().ok_or(
            "`editItems` must be {ref: patch | null}, a ref being \"YYYY-MM-DD · label\", a label, a date or an index",
        )?;
        let mut edit = Vec::new();
        for (reference, patch) in js_entries(o) {
            let patch = match patch {
                Value::Null => None,
                p => Some(read_item_patch(
                    p,
                    &format!("editItems[{}]", quoted(reference)),
                )?),
            };
            edit.push((reference.clone(), patch));
        }
        e.edit = Some(edit);
    }
    Ok(out)
}

/// The refs that name no item of `text`, tried on a copy; an op with any fails whole.
fn dry_run(text: &str, edits: &TimelineEdits, today: &str) -> Result<(), String> {
    if edits.edit.as_ref().is_none_or(Vec::is_empty) {
        return Ok(());
    }
    let mut copy = text.to_owned();
    let trial = TimelineEdits {
        add: None,
        ..edits.clone()
    };
    let missing = apply_timeline_edits(&mut copy, &trial, today);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing_refs_message(&missing))
    }
}

/// A new timeline: a note in the timeline view whose text is the timeline.
pub(super) fn add_timeline_note(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    raw: &JsonMap<String, Value>,
    ctx: &OpsCtx,
    id: Option<String>,
) -> Result<(String, Point), String> {
    let input = clean_timeline(raw, txn, shapes, id.as_deref(), ctx)?;
    if input.view == Some(false) {
        return Err("a timeline note needs `view:\"timeline\"`".into());
    }
    let TimelineInput {
        mut props, edits, ..
    } = input;
    if edits.edit.is_some() {
        return Err("`editItems` is for an update; give a new timeline its `items`".into());
    }
    let listed = edits.any();
    let given = props.get("text").and_then(Value::as_str).map(str::to_owned);
    let text = match given {
        Some(t) if !listed => t,
        Some(_) => {
            return Err(
                "give the timeline `text`, or `title`/`start`/`end`/`step`/`items`, not both"
                    .into(),
            )
        }
        None => {
            let items: Vec<ItemFields> = edits
                .items
                .iter()
                .flatten()
                .chain(edits.add.iter().flatten())
                .cloned()
                .collect();
            let mut days: Vec<String> = items
                .iter()
                .flat_map(|it| {
                    [
                        it.date.clone(),
                        it.end.clone().unwrap_or_else(|| it.date.clone()),
                    ]
                })
                .collect();
            days.sort();
            let mut start = edits
                .start
                .clone()
                .or_else(|| days.first().cloned())
                .unwrap_or_else(|| ctx.today());
            let mut end = edits.end.clone().unwrap_or_else(|| {
                days.last()
                    .cloned()
                    .unwrap_or_else(|| add_months(&start, 3))
            });
            if end < start {
                std::mem::swap(&mut start, &mut end);
            }
            if end == start && edits.end.is_none() {
                end = add_months(&start, 1);
            }
            let step = edits.step.unwrap_or_else(|| {
                step_for(day_of(&end).unwrap_or(0) - day_of(&start).unwrap_or(0) + 1)
            });
            timeline_markdown(
                edits.title.as_deref().unwrap_or("Timeline"),
                &start,
                &end,
                step,
                &items,
            )
        }
    };
    props.insert("view".into(), VIEW_TIMELINE.into());
    if !props.contains_key("color") {
        props.insert("color".into(), "white".into());
    }
    props.insert("by".into(), ctx.by.clone().into());
    let w = num(&props, "w")
        .unwrap_or(TIMELINE_SIZE.0)
        .max(TIMELINE_MIN.0);
    let h = num(&props, "h")
        .unwrap_or(TIMELINE_SIZE.1)
        .max(TIMELINE_MIN.1)
        .max(needed_height(&text, w, &ctx.today()));
    props.insert("text".into(), text.into());
    props.insert("w".into(), w.into());
    props.insert("h".into(), h.into());
    let at = place_props(txn, shapes, &mut props, w, h, ctx);
    let id = id.unwrap_or_else(super::new_shape_id);
    create(txn, shapes, ShapeType::Sticky, &id, &props, ctx);
    Ok((
        id,
        center(&BoxF {
            x: at.x,
            y: at.y,
            w,
            h,
        }),
    ))
}

fn note_text(txn: &mut TransactionMut, m: &MapRef) -> TextRef {
    match m.get(txn, "text") {
        Some(Out::YText(t)) => t,
        other => {
            let s = match other {
                Some(Out::Any(yrs::Any::String(s))) => s.to_string(),
                _ => String::new(),
            };
            m.insert(txn, "text", TextPrelim::new(s))
        }
    }
}

/// An update to a note that is (or is becoming) a timeline: box props and `text` as for
/// any note, then `view` and the timeline's fields — each an edit of the note's own lines.
/// Turning the view on leaves the text alone.
pub(super) fn update_timeline_note(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    shape: &Shape,
    raw: &JsonMap<String, Value>,
    ctx: &OpsCtx,
) -> Result<String, String> {
    let m = need_shape_map(txn, shapes, &shape.id)?;
    let was = shape.view == Some(View::Timeline);
    let input = clean_timeline(raw, txn, shapes, Some(&shape.id), ctx)?;
    let TimelineInput {
        mut props,
        view,
        edits,
    } = input;
    let on = view == Some(true) || (view.is_none() && was);
    if !on {
        // Back to a plain note: what is left is a plain note's update.
        for k in super::TIMELINE_ONLY {
            if raw.contains_key(k) {
                return Err(format!("`{k}` is for a timeline: set `view:\"timeline\"`"));
            }
        }
        if let Some(t) = &edits.title {
            props.insert("text".into(), t.clone().into());
        }
        super::clamp_min(ShapeType::Sticky, &mut props);
        store_update(txn, &m, ShapeType::Sticky, &props, ctx);
        if view == Some(false) && m.contains_key(txn, "view") {
            m.remove(txn, "view");
        }
        return Ok(shape.id.clone());
    }
    if props.contains_key("text") && edits.any() {
        return Err("give the timeline `text`, or its fields, not both".into());
    }
    let today = ctx.today();
    let text = props
        .get("text")
        .and_then(Value::as_str)
        .map_or_else(|| shape.text.clone(), str::to_owned);
    dry_run(&text, &edits, &today)?;
    if let Some(w) = num(&props, "w") {
        props.insert("w".into(), w.max(TIMELINE_MIN.0).into());
    }
    if let Some(h) = num(&props, "h") {
        props.insert("h".into(), h.max(TIMELINE_MIN.1).into());
    }
    store_update(txn, &m, ShapeType::Sticky, &props, ctx);
    if !was {
        m.insert(txn, "view", VIEW_TIMELINE);
        if !props.contains_key("w") && shape.w < TIMELINE_SIZE.0 {
            m.insert(txn, "w", num_any(TIMELINE_SIZE.0));
        }
    }
    // The text alone (a conversion, a new body) still sizes the card to fit.
    let t = note_text(txn, &m);
    apply_timeline_edits(&mut YText { txn, t: &t }, &edits, &today);
    m.insert(txn, "updatedAt", yrs::Any::from(ctx.now_ms));
    fit_height(txn, shapes, &shape.id, &today);
    Ok(shape.id.clone())
}

/// Grows the card (never shrinks it) to what its text now needs; `updatedAt` untouched.
fn fit_height(txn: &mut TransactionMut, shapes: &MapRef, id: &str, today: &str) {
    let Some(s) = crate::shape::live_shape(txn, shapes, id) else {
        return;
    };
    let need = needed_height(&s.text, s.w, today);
    if need > s.h {
        if let (Some(m), Some(v)) = (
            super::shape_map(txn, shapes, id),
            store_value("h", &need.into()),
        ) {
            m.insert(txn, "h", v);
        }
    }
}
