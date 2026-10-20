//! The checklist card: a note in the checklist view whose text is the list (the page's
//! `addChecklistNote` / `updateChecklistNote` in `ops.ts`).
//!
//! Everything an op says is checked against the list as it will be before anything is
//! written, so a bad pick, comment or assignee fails the whole op and changes nothing.

use serde_json::{Map as JsonMap, Value};
use yrs::{Map, MapRef, Out, TextPrelim, TextRef, TransactionMut};

use super::{
    clean_props, columns_of, create, need_shape_map, num, picker, place_props, rows_of,
    store_update, OpsCtx,
};
use crate::checklist::{
    assignees_key, checklist_height, checklist_width, clean_assignees, clean_columns, clean_notes,
    clean_picks, clean_rows, list_rows, map_json, merge_assignees, note_assignees, note_by_key,
    people_any, read_note_columns, string_list, write_note_assignees, write_note_columns,
    write_note_note, write_note_pick, Assignee, ASSIGNEES_PREFIX, CHECKLIST_MIN, DEFAULT_COLUMNS,
    NOTE_BY_PREFIX, NOTE_ROW,
};
use crate::checklist_text::{
    checklist_markdown, parse_checklist_text, replace_rows, row_block, row_key, who_key,
    write_title, MdRow, ParsedChecklist, TextRow, VIEW_CHECKLIST, WHO_PREFIX,
};
use crate::geometry::{center, BoxF, Point};
use crate::jsstr::YText;
use crate::schema::{js_slice, num_any, ShapeType, MAX_TITLE};
use crate::shape::{Shape, View};

/// What a new card made with assignees is wider by: two faces and their gap.
const ASSIGNEE_ROOM: f64 = 48.0;
const NOTE_LIST_KEYS: [&str; 7] = [
    "view",
    "title",
    "columns",
    "rows",
    "picks",
    "notes",
    "assignees",
];

/// What an add or update said about a checklist note, checked; box props and text went
/// through `clean_props`.
struct NoteListInput {
    props: JsonMap<String, Value>,
    /// `Some(true)`: `view:"checklist"`; `Some(false)`: back to a plain note.
    view: Option<bool>,
    title: Option<String>,
    columns: Option<Vec<String>>,
    rows: Option<Vec<String>>,
    picks: Option<Value>,
    notes: Option<Value>,
    assignees: Option<Value>,
}

fn clean_note_list(
    kind: ShapeType,
    raw: &JsonMap<String, Value>,
    txn: &TransactionMut,
    shapes: &MapRef,
    self_id: Option<&str>,
    ctx: &OpsCtx,
) -> Result<NoteListInput, String> {
    if kind == ShapeType::Checklist {
        // The checklist type's own names and spellings (`text` is its title).
        let mut props = clean_props(ShapeType::Checklist, raw, txn, shapes, self_id, ctx)?;
        let title = props
            .remove("title")
            .and_then(|t| t.as_str().map(str::to_owned));
        let columns = props.remove("columns").as_ref().map(columns_of);
        let rows = props
            .remove("rows")
            .as_ref()
            .map(|r| rows_of(r).into_iter().map(|r| r.label).collect());
        return Ok(NoteListInput {
            props,
            view: None,
            title,
            columns,
            rows,
            picks: raw.get("picks").cloned(),
            notes: raw.get("notes").cloned(),
            assignees: raw.get("assignees").cloned(),
        });
    }
    let rest: JsonMap<String, Value> = raw
        .iter()
        .filter(|(k, _)| !NOTE_LIST_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut out = NoteListInput {
        props: clean_props(ShapeType::Sticky, &rest, txn, shapes, self_id, ctx)?,
        view: None,
        title: None,
        columns: None,
        rows: None,
        picks: raw.get("picks").cloned(),
        notes: raw.get("notes").cloned(),
        assignees: raw.get("assignees").cloned(),
    };
    if let Some(v) = raw.get("view") {
        out.view = Some(match v {
            Value::String(s) if s == VIEW_CHECKLIST => true,
            Value::Null | Value::Bool(false) => false,
            Value::String(s) if s.is_empty() || s == "note" => false,
            _ => return Err("`view` must be \"checklist\" or null".into()),
        });
    }
    if let Some(t) = raw.get("title") {
        let t = t.as_str().ok_or("`title` must be a string")?;
        out.title = Some(js_slice(t, MAX_TITLE).to_owned());
    }
    if let Some(c) = raw.get("columns") {
        out.columns = Some(clean_columns(c)?);
    }
    if let Some(r) = raw.get("rows") {
        out.rows = Some(clean_rows(r, &[])?.into_iter().map(|r| r.label).collect());
    }
    Ok(out)
}

/// A new checklist: a note in the checklist view whose text is the list.
#[allow(clippy::too_many_lines)]
pub(super) fn add_checklist_note(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    kind: ShapeType,
    raw: &JsonMap<String, Value>,
    ctx: &OpsCtx,
    id: Option<String>,
) -> Result<(String, Point), String> {
    let input = clean_note_list(kind, raw, txn, shapes, id.as_deref(), ctx)?;
    if input.view == Some(false) {
        return Err("a checklist note needs `view:\"checklist\"`".into());
    }
    let mut props = input.props;
    let listed = kind == ShapeType::Checklist
        || input.rows.is_some()
        || input.title.is_some()
        || input.picks.is_some()
        || input.notes.is_some()
        || input.assignees.is_some();
    if listed && props.contains_key("text") && kind == ShapeType::Sticky {
        return Err(
            "give the note `text`, or `title`/`rows`/`picks`/`notes`/`assignees`, not both".into(),
        );
    }
    let columns = input.columns.clone().or_else(|| {
        (kind == ShapeType::Checklist)
            .then(|| DEFAULT_COLUMNS.iter().map(|c| (*c).to_owned()).collect())
    });
    let cols = columns.clone().unwrap_or_else(|| read_note_columns(None));
    let mut text = props
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let parsed = parse_checklist_text(&text, &cols);
    let mut row_count = parsed.rows.len();
    let mut note_count = parsed.rows.iter().filter(|r| !r.note.is_empty()).count();
    let mut picked: Vec<(String, String)> = Vec::new();
    let mut noted: Vec<String> = Vec::new();
    let mut assigned: Vec<(String, Vec<Assignee>)> = Vec::new();
    let by = picker(ctx);
    if listed {
        let rows = list_rows(input.rows.as_deref().unwrap_or_default());
        // Checked before anything is written: a bad pick, comment or assignee fails the
        // whole add.
        let resolved = match &input.picks {
            None => Vec::new(),
            Some(p) => clean_picks(p, &rows, &cols)?,
        };
        let said = match &input.notes {
            None => Vec::new(),
            Some(n) => clean_notes(n, &rows)?,
        };
        let people = match &input.assignees {
            None => Vec::new(),
            Some(a) => clean_assignees(a, &rows, &ctx.people)?,
        };
        let get = |list: &[(String, Vec<Assignee>)], id: &str| -> Vec<Assignee> {
            list.iter()
                .find(|(r, _)| r == id)
                .map(|(_, l)| l.clone())
                .unwrap_or_default()
        };
        for r in &rows {
            let (list, _) = merge_assignees(&[], &get(&people, &r.id), &by);
            if !list.is_empty() {
                assigned.push((r.label.clone(), list));
            }
        }
        let pick_of = |id: &str| -> Option<String> {
            resolved
                .iter()
                .find(|(r, _)| r == id)
                .and_then(|(_, c)| c.clone())
        };
        let note_of = |id: &str| -> String {
            said.iter()
                .find(|(r, _)| r == id)
                .map(|(_, t)| t.clone())
                .unwrap_or_default()
        };
        // `new Map(assigned)`: the last list given a label.
        let named = |label: &str| -> Vec<String> {
            assigned
                .iter()
                .rev()
                .find(|(l, _)| l == label)
                .map(|(_, list)| list.iter().map(|p| p.name.clone()).collect())
                .unwrap_or_default()
        };
        let md: Vec<MdRow> = rows
            .iter()
            .map(|r| MdRow {
                label: r.label.clone(),
                pick: pick_of(&r.id),
                note: note_of(&r.id),
                assignees: named(&r.label),
            })
            .collect();
        text = checklist_markdown(input.title.as_deref().unwrap_or_default(), &md, &cols);
        for r in &rows {
            if let Some(col) = pick_of(&r.id).filter(|c| !c.is_empty()) {
                picked.push((r.label.clone(), col));
            }
            if !note_of(&r.id).is_empty() {
                noted.push(r.label.clone());
            }
        }
        row_count = rows.len();
        note_count = noted.len();
    }
    props.insert("view".into(), VIEW_CHECKLIST.into());
    props.insert("text".into(), text.into());
    if !props.contains_key("color") {
        props.insert("color".into(), "green".into());
    }
    props.insert("by".into(), ctx.by.clone().into());
    if let Some(c) = &columns {
        props.insert("columns".into(), Value::from(c.clone()));
    }
    let (mw, mh) = CHECKLIST_MIN;
    let room = if assigned.is_empty() {
        0.0
    } else {
        ASSIGNEE_ROOM
    };
    let w = num(&props, "w")
        .unwrap_or(checklist_width(cols.len()) + room)
        .max(mw);
    let h = num(&props, "h")
        .unwrap_or_else(|| checklist_height(cols.len(), row_count, note_count))
        .max(mh);
    props.insert("w".into(), w.into());
    props.insert("h".into(), h.into());
    let at = place_props(txn, shapes, &mut props, w, h, ctx);
    let id = id.unwrap_or_else(super::new_shape_id);
    create(txn, shapes, ShapeType::Sticky, &id, &props, ctx);
    if let Some(Out::YMap(m)) = shapes.get(txn, &id) {
        for (label, col) in &picked {
            let mut o = JsonMap::new();
            o.insert("col".into(), col.clone().into());
            m.insert(txn, who_key(label), by.stamp(o));
        }
        for label in &noted {
            m.insert(txn, note_by_key(label), by.stamp(JsonMap::new()));
        }
        for (label, list) in &assigned {
            m.insert(txn, assignees_key(label), people_any(list));
        }
    }
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

/// The note's text as a `Y.Text` (made from a plain string when someone wrote one).
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

fn text_of(txn: &TransactionMut, t: &TextRef) -> String {
    use yrs::GetString as _;
    t.get_string(txn)
}

fn labels(p: &ParsedChecklist) -> Vec<String> {
    p.rows.iter().map(|r| r.label.clone()).collect()
}

/// `(row, value)` pairs for rows `t<i>` of `parsed`, last row first (each write stays
/// inside its own row's lines, so earlier offsets hold).
fn by_index<T: Clone>(parsed: &ParsedChecklist, list: &[(String, T)]) -> Vec<(TextRow, T)> {
    let mut out: Vec<(TextRow, T)> = list
        .iter()
        .filter_map(|(id, v)| {
            let i: usize = id.strip_prefix('t')?.parse().ok()?;
            Some((parsed.rows.get(i)?.clone(), v.clone()))
        })
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.0.index));
    out
}

/// An update to a note that is (or is becoming) a checklist: box props and `text` as for
/// any note, then `view`, `columns`, `rows`, `title`, `picks`, `notes`, `assignees` — each
/// an edit of the note's text. Turning the view on leaves the text alone.
#[allow(clippy::too_many_lines)]
pub(super) fn update_checklist_note(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    shape: &Shape,
    raw: &JsonMap<String, Value>,
    ctx: &OpsCtx,
) -> Result<String, String> {
    let m = need_shape_map(txn, shapes, &shape.id)?;
    let was = shape.view == Some(View::Checklist);
    let input = clean_note_list(ShapeType::Sticky, raw, txn, shapes, Some(&shape.id), ctx)?;
    let mut props = input.props.clone();
    let listed = input.view == Some(true) || (input.view.is_none() && was);
    if !listed {
        // Back to (or still) a plain note: what is left is a plain note's update.
        for (k, given) in [
            ("columns", input.columns.is_some()),
            ("rows", input.rows.is_some()),
            ("picks", input.picks.is_some()),
            ("notes", input.notes.is_some()),
            ("assignees", input.assignees.is_some()),
        ] {
            if given {
                return Err(format!(
                    "`{k}` is for a checklist: set `view:\"checklist\"`"
                ));
            }
        }
        if let Some(t) = &input.title {
            props.insert("text".into(), t.clone().into());
        }
        super::clamp_min(ShapeType::Sticky, &mut props);
        store_update(txn, &m, ShapeType::Sticky, &props, ctx);
        if input.view == Some(false) && m.contains_key(txn, "view") {
            m.remove(txn, "view");
        }
        return Ok(shape.id.clone());
    }
    // Everything is checked against the list as it will be before anything is written.
    let columns = input
        .columns
        .clone()
        .unwrap_or_else(|| read_note_columns(map_json(txn, &m, "columns").as_ref()));
    let text = props
        .get("text")
        .and_then(Value::as_str)
        .map_or_else(|| shape.text.clone(), str::to_owned);
    let rows = match &input.rows {
        Some(r) => list_rows(r),
        None => list_rows(&labels(&parse_checklist_text(&text, &columns))),
    };
    if let Some(p) = &input.picks {
        clean_picks(p, &rows, &columns)?;
    }
    if let Some(n) = &input.notes {
        clean_notes(n, &rows)?;
    }
    if let Some(a) = &input.assignees {
        clean_assignees(a, &rows, &ctx.people)?;
    }
    super::clamp_min(ShapeType::Checklist, &mut props);
    let (w_given, h_given) = (props.contains_key("w"), props.contains_key("h"));
    store_update(txn, &m, ShapeType::Sticky, &props, ctx);
    let t = note_text(txn, &m);
    if !was {
        m.insert(txn, "view", VIEW_CHECKLIST);
        if let Some(c) = &input.columns {
            m.insert(txn, "columns", string_list(c));
        }
        if !w_given && shape.w < checklist_width(columns.len()) {
            m.insert(txn, "w", num_any(checklist_width(columns.len())));
        }
    } else if let Some(c) = &input.columns {
        write_note_columns(txn, &m, &t, c);
    }
    if let Some(next) = &input.rows {
        let now = parse_checklist_text(&text_of(txn, &t), &columns);
        let mut left: Vec<TextRow> = now.rows.clone();
        // A row keeps its pick, its comment and its assignees by label.
        let mut carry = |label: &str| -> Option<TextRow> {
            let i = left.iter().position(|r| r.label == label).or_else(|| {
                left.iter()
                    .position(|r| row_key(&r.label) == row_key(label))
            })?;
            Some(left.remove(i))
        };
        let blocks: Vec<String> = next
            .iter()
            .map(|label| {
                let was = carry(label);
                row_block(
                    label,
                    was.as_ref().and_then(|w| w.pick.as_deref()),
                    &columns,
                    was.as_ref().map_or("", |w| w.note.as_str()),
                    was.as_ref().map_or(&[][..], |w| w.assignees.as_slice()),
                )
            })
            .collect();
        replace_rows(&mut YText { txn, t: &t }, &now, &blocks);
        let keep: Vec<String> = next
            .iter()
            .flat_map(|l| [who_key(l), note_by_key(l), assignees_key(l)])
            .collect();
        let gone: Vec<String> = m
            .keys(txn)
            .filter(|k| {
                (k.starts_with(WHO_PREFIX)
                    || k.starts_with(NOTE_BY_PREFIX)
                    || k.starts_with(ASSIGNEES_PREFIX))
                    && !keep.iter().any(|kk| kk == k)
            })
            .map(str::to_owned)
            .collect();
        for key in gone {
            m.remove(txn, &key);
        }
    }
    if let Some(title) = &input.title {
        let p = parse_checklist_text(&text_of(txn, &t), &columns);
        write_title(&mut YText { txn, t: &t }, &p, title);
    }
    let by = picker(ctx);
    // The lists were checked above against the same rows; a failure here would only come
    // from a label the text cannot hold, and then that part is skipped rather than half
    // written.
    if let Some(p) = &input.picks {
        let now = parse_checklist_text(&text_of(txn, &t), &columns);
        let resolved = clean_picks(p, &list_rows(&labels(&now)), &columns).unwrap_or_default();
        for (row, col) in by_index(&now, &resolved) {
            write_note_pick(txn, &m, &t, &row, col.as_deref(), &columns, &by);
        }
    }
    let noted = |txn: &TransactionMut| {
        parse_checklist_text(&text_of(txn, &t), &columns)
            .rows
            .iter()
            .filter(|r| !r.note.is_empty())
            .count()
    };
    let noted_before = noted(txn);
    if let Some(n) = &input.notes {
        let now = parse_checklist_text(&text_of(txn, &t), &columns);
        let said = clean_notes(n, &list_rows(&labels(&now))).unwrap_or_default();
        for (row, text) in by_index(&now, &said) {
            if row.note != text {
                write_note_note(txn, &m, &t, &row, &text, &by);
            }
        }
    }
    if let Some(a) = &input.assignees {
        let now = parse_checklist_text(&text_of(txn, &t), &columns);
        let said = clean_assignees(a, &list_rows(&labels(&now)), &ctx.people).unwrap_or_default();
        for (row, list) in by_index(&now, &said) {
            let current = note_assignees(&|k: &str| map_json(&*txn, &m, k), &row);
            let (people, _added) = merge_assignees(&current, &list, &by);
            write_note_assignees(txn, &m, &t, &row, &people);
        }
    }
    let after = parse_checklist_text(&text_of(txn, &t), &columns);
    let noted_after = after.rows.iter().filter(|r| !r.note.is_empty()).count();
    if was && (input.rows.is_some() || input.columns.is_some()) && !h_given {
        let h = checklist_height(columns.len(), after.rows.len(), noted_after);
        m.insert(txn, "h", num_any(h));
    } else if was && noted_after != noted_before && !h_given {
        // Comments alone: the card grows (or shrinks) by their lines.
        let cur = crate::schema::out_f64(m.get(txn, "h").as_ref()).unwrap_or(shape.h);
        #[allow(clippy::cast_precision_loss)]
        let delta = NOTE_ROW * (noted_after as f64 - noted_before as f64);
        m.insert(txn, "h", num_any(CHECKLIST_MIN.1.max(cur + delta)));
    }
    if was && input.columns.is_some() && !w_given {
        let w = shape.w.max(checklist_width(columns.len()));
        m.insert(txn, "w", num_any(w));
    }
    m.insert(txn, "updatedAt", yrs::Any::from(ctx.now_ms));
    Ok(shape.id.clone())
}
