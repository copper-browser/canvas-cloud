//! The timeline card — the page's `canvas/timeline.ts`: a title, a date range with a step,
//! and items along it — notes (a date and a label), bubbles (a note with a markdown body)
//! and sections (a span of days, in a colour).
//!
//! Stored as a note (`type:"sticky"`, `view:"timeline"`) whose text is the timeline (see
//! [`crate::timeline_text`]). Nothing but `view` is a prop: the title, range and items are
//! all read from the text, so an older Copper that edits the note as text edits the
//! timeline.

use serde_json::{Map as JsonMap, Value};

use crate::jsstr::{quoted, TextBuf};
use crate::timeline_layout::{needed_height as layout_needed, LayoutItem};
use crate::timeline_text::{
    add_months, clean_iso, day_of, insert_item_lines, iso_of, parse_timeline_text, read_item_color,
    read_step, remove_item_lines, replace_items, resolve_item, write_item, write_range,
    write_title, ItemFields, ItemKind, ParsedTimeline, Step, TextItem, DEFAULT_STEP,
};

/// A new card's box (world px).
pub const TIMELINE_SIZE: (f64, f64) = (880.0, 300.0);
/// The least a card may be resized to; its content may need more height.
pub const TIMELINE_MIN: (f64, f64) = (360.0, 160.0);
/// Most items one card takes from an agent.
pub const MAX_ITEMS: usize = 200;

/// What a card draws from its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineData {
    pub start: String,
    pub end: String,
    pub step: Step,
    /// The text has its own range line (else the range is the items' span).
    pub ranged: bool,
    pub items: Vec<TextItem>,
}

/// The range (or the items' span, or the next three months from `today`) and the items.
pub fn timeline_of(parsed: &ParsedTimeline, today: &str) -> TimelineData {
    let items = parsed.items.clone();
    if let Some(r) = &parsed.range {
        return TimelineData {
            start: r.start.clone(),
            end: r.end.clone(),
            step: r.step,
            ranged: true,
            items,
        };
    }
    let mut span: Option<(i64, i64)> = None;
    for it in &items {
        let (Some(a), Some(b)) = (
            day_of(&it.date),
            day_of(it.end.as_deref().unwrap_or(&it.date)),
        ) else {
            continue;
        };
        span = Some(span.map_or((a, b), |(lo, hi)| (lo.min(a), hi.max(b))));
    }
    let Some((lo, hi)) = span else {
        return TimelineData {
            start: today.to_owned(),
            end: add_months(today, 3),
            step: DEFAULT_STEP,
            ranged: false,
            items,
        };
    };
    // A little room either side of the items' own span (`Math.round`, halves up).
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let pad = 1i64.max(((hi - lo) as f64 * 0.05 + 0.5).floor() as i64);
    TimelineData {
        start: iso_of(lo - pad),
        end: iso_of(hi + pad),
        step: DEFAULT_STEP,
        ranged: false,
        items,
    }
}

/// The title and timeline of a note in the timeline view.
pub fn read_note_timeline(text: &str, today: &str) -> (String, TimelineData) {
    let parsed = parse_timeline_text(text);
    let data = timeline_of(&parsed, today);
    (parsed.title, data)
}

/// The least height (rounded up) a card with this text needs at width `w`.
pub fn needed_height(text: &str, w: f64, today: &str) -> f64 {
    let (_, t) = read_note_timeline(text, today);
    let items: Vec<LayoutItem> = t
        .items
        .iter()
        .map(|it| LayoutItem {
            index: it.index,
            date: it.date.clone(),
            end: it.end.clone(),
            label: it.label.clone(),
            body: it.body.clone(),
            kind: it.kind,
        })
        .collect();
    layout_needed(&t.start, &t.end, t.step, &items, w).ceil()
}

/// An edit to one item: only the keys given change (`Some(None)` clears).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemPatch {
    pub date: Option<String>,
    pub end: Option<Option<String>>,
    pub label: Option<String>,
    pub color: Option<Option<&'static str>>,
    pub body: Option<Option<String>>,
}

/// The item's fields after `patch` (a section's end never before its start).
pub fn patched(it: &TextItem, patch: &ItemPatch) -> ItemFields {
    let mut date = patch.date.clone().unwrap_or_else(|| it.date.clone());
    let mut end = patch.end.clone().unwrap_or_else(|| it.end.clone());
    if let Some(e) = end.as_mut() {
        if *e < date {
            std::mem::swap(e, &mut date);
        }
    }
    let body = if end.is_some() {
        None
    } else {
        patch.body.clone().unwrap_or_else(|| it.body.clone())
    };
    ItemFields {
        date,
        end,
        label: patch.label.clone().unwrap_or_else(|| it.label.clone()),
        color: patch.color.unwrap_or(it.color),
        body,
    }
}

fn present(o: &JsonMap<String, Value>, k: &str) -> bool {
    o.contains_key(k)
}

/// `v === undefined || v === null || v === ''`.
fn blank(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    }
}

fn iso_value(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).and_then(clean_iso)
}

/// `o[a] ?? o[b]`.
fn either<'a>(o: &'a JsonMap<String, Value>, a: &str, b: &str) -> Option<&'a Value> {
    match o.get(a) {
        None | Some(Value::Null) => o.get(b),
        v => v,
    }
}

/// One item from an agent: `{date, end?, label?, text?, color?, kind?}`.
pub fn read_item_input(v: &Value, at: &str) -> Result<ItemFields, String> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("{at} must be an object with a date"))?;
    let date = iso_value(either(o, "date", "start"))
        .ok_or_else(|| format!("{at}.date must be a YYYY-MM-DD date"))?;
    let mut end = None;
    if !blank(o.get("end")) {
        end = Some(
            iso_value(o.get("end")).ok_or_else(|| format!("{at}.end must be a YYYY-MM-DD date"))?,
        );
    }
    let mut lo = date;
    if let Some(e) = end.as_mut() {
        if *e < lo {
            std::mem::swap(e, &mut lo);
        }
    }
    let label = match (o.get("label"), o.get("title")) {
        (Some(Value::String(s)), _) | (_, Some(Value::String(s))) => s.clone(),
        _ => String::new(),
    };
    let color = if blank(o.get("color")) {
        None
    } else {
        Some(
            o.get("color")
                .and_then(Value::as_str)
                .and_then(read_item_color)
                .ok_or_else(|| {
                    format!("{at}.color must be yellow, pink, blue, green, purple or gray")
                })?,
        )
    };
    let said = either(o, "text", "body");
    let body = if end.is_some() {
        None
    } else if let Some(Value::String(s)) = said {
        Some(s.clone())
    } else if o.get("kind").and_then(Value::as_str) == Some("bubble") {
        Some(String::new())
    } else {
        None
    };
    Ok(ItemFields {
        date: lo,
        end,
        label,
        color,
        body,
    })
}

pub fn read_items_input(v: &Value, key: &str) -> Result<Vec<ItemFields>, String> {
    let list = v
        .as_array()
        .ok_or_else(|| format!("`{key}` must be a list of items"))?;
    if list.len() > MAX_ITEMS {
        return Err(format!("`{key}` takes at most {MAX_ITEMS} items"));
    }
    list.iter()
        .enumerate()
        .map(|(i, it)| read_item_input(it, &format!("{key}[{i}]")))
        .collect()
}

/// An agent's patch for one item (`{date?, end?, label?, text?, color?, kind?}`).
pub fn read_item_patch(v: &Value, at: &str) -> Result<ItemPatch, String> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("{at} must be an object, or null to remove the item"))?;
    let mut p = ItemPatch::default();
    if present(o, "date") {
        p.date = Some(
            iso_value(o.get("date"))
                .ok_or_else(|| format!("{at}.date must be a YYYY-MM-DD date"))?,
        );
    }
    if present(o, "end") {
        p.end = Some(if blank(o.get("end")) {
            None
        } else {
            Some(
                iso_value(o.get("end"))
                    .ok_or_else(|| format!("{at}.end must be a YYYY-MM-DD date, or null"))?,
            )
        });
    }
    if let Some(l) = o.get("label") {
        p.label = Some(
            l.as_str()
                .ok_or_else(|| format!("{at}.label must be text"))?
                .to_owned(),
        );
    }
    if present(o, "color") {
        p.color = Some(if blank(o.get("color")) {
            None
        } else {
            Some(
                o.get("color")
                    .and_then(Value::as_str)
                    .and_then(read_item_color)
                    .ok_or_else(|| {
                        format!(
                            "{at}.color must be yellow, pink, blue, green, purple, gray or null"
                        )
                    })?,
            )
        });
    }
    let said = if present(o, "text") {
        o.get("text")
    } else {
        o.get("body")
    };
    if let Some(s) = said {
        p.body = Some(match s {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            _ => return Err(format!("{at}.text must be text, or null")),
        });
    }
    match o.get("kind").and_then(Value::as_str) {
        Some("bubble") if p.body.is_none() => p.body = Some(Some(String::new())),
        Some("note") => p.body = Some(None),
        _ => {}
    }
    Ok(p)
}

pub fn read_step_input(v: &Value, key: &str) -> Result<Step, String> {
    v.as_str()
        .and_then(read_step)
        .ok_or_else(|| format!("`{key}` must be day, week, month, quarter or year"))
}

/// An agent's edits on a timeline's text.
#[derive(Debug, Clone, Default)]
pub struct TimelineEdits {
    pub title: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub step: Option<Step>,
    pub items: Option<Vec<ItemFields>>,
    pub add: Option<Vec<ItemFields>>,
    pub edit: Option<Vec<(String, Option<ItemPatch>)>>,
}

impl TimelineEdits {
    pub fn any(&self) -> bool {
        self.title.is_some()
            || self.start.is_some()
            || self.end.is_some()
            || self.step.is_some()
            || self.items.is_some()
            || self.add.is_some()
            || self.edit.is_some()
    }
}

/// Title, range, then the whole list or single items (changes and removals first, then
/// additions). The refs that name no item, in order; the rest still apply.
pub fn apply_timeline_edits<B: TextBuf + ?Sized>(
    t: &mut B,
    e: &TimelineEdits,
    today: &str,
) -> Vec<String> {
    let mut missing = Vec::new();
    if let Some(title) = &e.title {
        let p = parse_timeline_text(&t.text());
        write_title(t, &p, title);
    }
    if e.start.is_some() || e.end.is_some() || e.step.is_some() {
        let p = parse_timeline_text(&t.text());
        let now = timeline_of(&p, today);
        let mut start = e.start.clone().unwrap_or(now.start);
        let mut end = e.end.clone().unwrap_or(now.end);
        if end < start {
            std::mem::swap(&mut start, &mut end);
        }
        write_range(t, &p, &start, &end, e.step.unwrap_or(now.step));
    }
    if let Some(items) = &e.items {
        let p = parse_timeline_text(&t.text());
        replace_items(t, &p, items);
    }
    for (reference, patch) in e.edit.iter().flatten() {
        let p = parse_timeline_text(&t.text());
        let Some(it) = resolve_item(&p, reference).cloned() else {
            missing.push(reference.clone());
            continue;
        };
        match patch {
            None => remove_item_lines(t, &it),
            Some(patch) => {
                write_item(t, &p, &it, &patched(&it, patch));
            }
        }
    }
    for it in e.add.iter().flatten() {
        let p = parse_timeline_text(&t.text());
        insert_item_lines(t, &p, it);
    }
    missing
}

/// The page's `dryRun` complaint when refs name no item.
pub fn missing_refs_message(missing: &[String]) -> String {
    format!(
        "no item {} on this timeline (name one by \"YYYY-MM-DD · label\", its label, its date or its index)",
        missing.iter().map(|m| quoted(m)).collect::<Vec<_>>().join(", ")
    )
}

/// A step that suits a span of days, when none is given.
pub fn step_for(days: i64) -> Step {
    if days <= 31 {
        Step::Day
    } else if days <= 270 {
        Step::Week
    } else if days <= 3 * 366 {
        Step::Month
    } else if days <= 8 * 366 {
        Step::Quarter
    } else {
        Step::Year
    }
}

/// What `canvas_read` says of a timeline's items; `clip` shortens a body.
pub fn summarize_items(items: &[TextItem], clip: &dyn Fn(&str) -> String) -> Vec<Value> {
    items
        .iter()
        .map(|it| {
            let mut o = JsonMap::new();
            o.insert("date".into(), it.date.clone().into());
            if let Some(end) = &it.end {
                o.insert("end".into(), end.clone().into());
            }
            o.insert("label".into(), it.label.clone().into());
            if let Some(body) = &it.body {
                o.insert("text".into(), clip(body).into());
            }
            if let Some(c) = it.color {
                o.insert("color".into(), c.into());
            }
            o.insert("kind".into(), it.kind.as_str().into());
            Value::Object(o)
        })
        .collect()
}

/// Whether an item is a section, a bubble or a note.
pub fn kind_name(k: ItemKind) -> &'static str {
    k.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spans_and_inputs() {
        let p = parse_timeline_text("- 2026-01-01 · A\n- 2026-01-21 · B");
        let t = timeline_of(&p, "2030-01-01");
        assert_eq!(
            (t.start.as_str(), t.end.as_str(), t.ranged),
            ("2025-12-31", "2026-01-22", false)
        );
        let t = timeline_of(&parse_timeline_text(""), "2026-01-31");
        assert_eq!(
            (t.start.as_str(), t.end.as_str()),
            ("2026-01-31", "2026-04-30")
        );
        let it = read_item_input(&json!({"start": "2026-02-01", "end": "2026-01-01", "title": "S", "color": "Grey", "text": "x"}), "items[0]").unwrap();
        assert_eq!(it.date, "2026-01-01");
        assert_eq!(it.end.as_deref(), Some("2026-02-01"));
        assert_eq!(
            (it.label.as_str(), it.color, it.body),
            ("S", Some("gray"), None)
        );
        assert_eq!(
            read_item_input(&json!({"date": "2026-02-30"}), "items[0]").unwrap_err(),
            "items[0].date must be a YYYY-MM-DD date"
        );
        assert_eq!(
            read_item_input(&json!({"date": "2026-02-03", "color": "red"}), "x").unwrap_err(),
            "x.color must be yellow, pink, blue, green, purple or gray"
        );
        let b = read_item_input(&json!({"date": "2026-02-03", "kind": "bubble"}), "x").unwrap();
        assert_eq!(b.body.as_deref(), Some(""));
        let p = read_item_patch(&json!({"end": null, "kind": "note", "color": ""}), "e").unwrap();
        assert_eq!(p.end, Some(None));
        assert_eq!(p.body, Some(None));
        assert_eq!(p.color, Some(None));
        assert_eq!(
            read_items_input(&json!({}), "items").unwrap_err(),
            "`items` must be a list of items"
        );
        assert_eq!(step_for(31), Step::Day);
        assert_eq!(step_for(400), Step::Month);
    }

    #[test]
    fn edits() {
        let mut t = String::from(
            "### Plan\n2026-01-01 → 2026-03-01 · weekly\n\n- 2026-01-05 · A\n- 2026-02-01 · B",
        );
        let e = TimelineEdits {
            title: Some("Q1".into()),
            step: Some(Step::Month),
            edit: Some(vec![
                (
                    "A".into(),
                    Some(ItemPatch {
                        date: Some("2026-02-10".into()),
                        ..ItemPatch::default()
                    }),
                ),
                ("nope".into(), None),
                ("2026-02-01 · b".into(), None),
            ]),
            add: Some(vec![ItemFields {
                date: "2026-01-02".into(),
                label: "New".into(),
                ..ItemFields::default()
            }]),
            ..TimelineEdits::default()
        };
        let missing = apply_timeline_edits(&mut t, &e, "2026-01-01");
        assert_eq!(missing, ["nope"]);
        assert_eq!(
            t,
            "### Q1\n2026-01-01 → 2026-03-01 · monthly\n\n- 2026-01-02 · New\n- 2026-02-10 · A"
        );
        assert!(missing_refs_message(&missing).starts_with("no item \"nope\" on this timeline"));
        assert!(needed_height(&t, 880.0, "2026-01-01") >= 153.0);
    }
}
