//! The checklist (RSVP) card — the page's `canvas/checklist.ts`: a title, 1–4 columns
//! ("Yes", "No") and up to 60 rows (people, tasks), each row with at most one pick, a
//! comment and up to 12 assignees.
//!
//! Two stored forms, one card:
//!
//! * **A note** (`type:"sticky"`, `view:"checklist"`, what `add` writes since 0.2.0): the
//!   note's text is the checklist, as markdown every Copper draws as task boxes — see
//!   [`crate::checklist_text`]. Beside the text, keyed by the row's label
//!   ([`crate::checklist_text::row_key`]): `who:<label>` → `{col, by, byId, at}` (who
//!   picked it), `noteBy:<label>` → `{by, byId, at}` (who wrote its comment) and
//!   `assignees:<label>` → `[{name, id?, email?, by?, byId?, at?}]` (who they are).
//! * **Legacy** (`type:"checklist"`, what 0.1 wrote), on the shape's own `Y.Map`:
//!
//! ```text
//! title               string
//! columns             string[]                  whole value, last writer wins
//! rows                {id, label}[]             whole value, last writer wins
//! pick:<rowId>        {col, by, byId, at}       one key per row
//! note:<rowId>        {text, by, byId, at}      a row's comment
//! assignees:<rowId>   [{name, id?, email?, by?, byId?, at?}]
//! ```
//!
//! A pick lives under its row's own key, so two people picking different rows at the
//! same moment both stick. `col` is the column's label; a pick naming no current column
//! (or no current row) reads as no pick.

use serde::Serialize;
use serde_json::{json, Map as JsonMap, Value};
use yrs::{Any, Map, MapRef, ReadTxn, TextRef, TransactionMut};

use crate::checklist_text::{
    assignee_name, note_lines, parse_checklist_text, row_key, who_key, write_row_assignees,
    write_row_note, write_row_pick, TextRow, TICK_COLUMN, WHO_PREFIX,
};
use crate::jsstr::{fold, js_trim, quoted, YText};
use crate::schema::{jnum, js_len, js_slice, json_f64, json_to_any};

/// Prefix of the per-row pick keys.
pub const PICK_PREFIX: &str = "pick:";
/// A legacy card's comment on a row: `note:<rowId>` → `{text, by, byId, at}`.
pub const NOTE_PREFIX: &str = "note:";
/// Who last wrote a note's row comment: `noteBy:<label key>` → `{by, byId, at}`.
pub const NOTE_BY_PREFIX: &str = "noteBy:";
/// Who a row's assignees are: `assignees:<label key>` on a note, `assignees:<rowId>` on a
/// legacy card.
pub const ASSIGNEES_PREFIX: &str = "assignees:";
/// People on one row.
pub const MAX_ASSIGNEES: usize = 12;
pub const MAX_COLUMNS: usize = 4;
pub const MAX_ROWS: usize = 60;
/// Labels are cut to these many UTF-16 units (as JavaScript counts).
pub const MAX_COLUMN_LABEL: usize = 40;
pub const MAX_ROW_LABEL: usize = 200;
/// A row's comment is cut to these many characters.
pub const MAX_NOTE: usize = 4000;
pub const DEFAULT_COLUMNS: [&str; 2] = ["Yes", "No"];
/// The smallest a checklist card gets (either form).
pub const CHECKLIST_MIN: (f64, f64) = (200.0, 72.0);

/// Layout numbers shared with the page (world px), for a fresh card's size.
const HEAD: f64 = 49.0;
const COLUMNS_HEAD: f64 = 28.0;
const ROW: f64 = 34.0;
/// A row's collapsed comment: one muted line under it.
pub const NOTE_ROW: f64 = 22.0;
const FOOT: f64 = 14.0;
const COL: f64 = 64.0;
const LABEL: f64 = 168.0;
const PAD: f64 = 14.0;

/// The pick key of a legacy row.
pub fn pick_key(row_id: &str) -> String {
    format!("{PICK_PREFIX}{row_id}")
}
pub fn note_key(row_id: &str) -> String {
    format!("{NOTE_PREFIX}{row_id}")
}
pub fn note_by_key(label: &str) -> String {
    format!("{NOTE_BY_PREFIX}{}", row_key(label))
}
/// A note row's assignees' ids, by its label.
pub fn assignees_key(label: &str) -> String {
    format!("{ASSIGNEES_PREFIX}{}", row_key(label))
}
/// A legacy row's assignees, by its id.
pub fn legacy_assignees_key(row_id: &str) -> String {
    format!("{ASSIGNEES_PREFIX}{row_id}")
}

/// A comment as it is kept: `\n` line breaks, at most [`MAX_NOTE`] characters.
pub fn clean_note(text: &str) -> String {
    js_slice(&note_lines(text), MAX_NOTE).to_owned()
}

/// A fresh card's width for this many columns.
#[allow(clippy::cast_precision_loss)]
pub fn checklist_width(columns: usize) -> f64 {
    if columns <= 1 {
        280.0
    } else {
        PAD * 2.0 + LABEL + COL * columns as f64
    }
}

/// The height a card needs before anyone has measured it (one line per label, one per
/// comment).
#[allow(clippy::cast_precision_loss)]
pub fn checklist_height(columns: usize, rows: usize, notes: usize) -> f64 {
    HEAD + if columns > 1 { COLUMNS_HEAD } else { 0.0 }
        + ROW * rows.max(1) as f64
        + NOTE_ROW * notes as f64
        + FOOT
}

/// One row: a person or a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub label: String,
}

impl Row {
    pub fn to_json(&self) -> Value {
        json!({ "id": self.id, "label": self.label })
    }
}

/// The id a note's row goes by: its place among the rows.
pub fn note_row_id(index: usize) -> String {
    format!("t{index}")
}

/// Rows `t0…` for these labels (a note's rows).
pub fn list_rows<S: AsRef<str>>(labels: &[S]) -> Vec<Row> {
    labels
        .iter()
        .enumerate()
        .map(|(i, l)| Row {
            id: note_row_id(i),
            label: l.as_ref().to_owned(),
        })
        .collect()
}

/// A row's pick: the column's label, who made it and when.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub col: String,
    pub by: String,
    pub by_id: String,
    pub at: f64,
}

impl Pick {
    pub fn to_json(&self) -> Value {
        json!({ "col": self.col, "by": self.by, "byId": self.by_id, "at": jnum(self.at) })
    }
}

/// A row's comment and who wrote it, when.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub text: String,
    pub by: String,
    pub by_id: String,
    pub at: f64,
}

/// Someone a row is assigned to.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Assignee {
    pub name: String,
    /// A cloud user id (lower-case).
    pub id: Option<String>,
    pub email: Option<String>,
    /// Who assigned them, when.
    pub by: Option<String>,
    pub by_id: Option<String>,
    pub at: Option<f64>,
}

impl Assignee {
    /// What an assignee keeps on the shape (the page's `storedAssignee`).
    pub fn stored(&self) -> Value {
        let mut o = JsonMap::new();
        o.insert("name".into(), self.name.clone().into());
        for (k, v) in [
            ("id", &self.id),
            ("email", &self.email),
            ("by", &self.by),
            ("byId", &self.by_id),
        ] {
            if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
                o.insert(k.into(), v.clone().into());
            }
        }
        if let Some(at) = self.at {
            o.insert("at".into(), jnum(at));
        }
        Value::Object(o)
    }

    /// `{name, id?, email?}` as an agent reads it.
    pub fn summary(&self) -> Value {
        let mut o = JsonMap::new();
        o.insert("name".into(), self.name.clone().into());
        if let Some(id) = self.id.as_ref().filter(|v| !v.is_empty()) {
            o.insert("id".into(), id.clone().into());
        }
        if let Some(e) = self.email.as_ref().filter(|v| !v.is_empty()) {
            o.insert("email".into(), e.clone().into());
        }
        Value::Object(o)
    }
}

/// Someone on the canvas, whom `assignees` resolve against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
}

/// A checklist as read from the document (either form).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Checklist {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    /// Picks that name a current row and column, in row order.
    pub picks: Vec<(String, Pick)>,
    /// Row comments, in row order.
    pub notes: Vec<(String, Note)>,
    /// Rows' assignees (rows that have some), in row order.
    pub assignees: Vec<(String, Vec<Assignee>)>,
}

impl Checklist {
    pub fn pick(&self, row_id: &str) -> Option<&Pick> {
        self.picks.iter().find(|(r, _)| r == row_id).map(|(_, p)| p)
    }
    pub fn note(&self, row_id: &str) -> Option<&Note> {
        self.notes.iter().find(|(r, _)| r == row_id).map(|(_, n)| n)
    }
    pub fn people(&self, row_id: &str) -> &[Assignee] {
        self.assignees
            .iter()
            .find(|(r, _)| r == row_id)
            .map_or(&[], |(_, a)| a.as_slice())
    }
}

/// `Object.entries(o)` in JavaScript's order as far as JSON keeps it: array-index keys
/// first, ascending, then the rest (the server's maps are sorted, so the rest is
/// alphabetical rather than insertion order).
pub fn js_entries(o: &JsonMap<String, Value>) -> Vec<(&String, &Value)> {
    let index = |k: &str| -> Option<u64> {
        if k.is_empty() || k.len() > 10 || !k.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if k.len() > 1 && k.starts_with('0') {
            return None;
        }
        k.parse::<u64>().ok().filter(|n| *n < u64::from(u32::MAX))
    };
    let mut out: Vec<(&String, &Value)> = o.iter().collect();
    out.sort_by_key(|(k, _)| index(k).map_or((1, 0), |n| (0, n)));
    out
}

// ---- tolerant reads -----------------------------------------------------------------------

/// Columns as stored: strings, deduplicated, at most four; none → `fallback`.
pub fn read_columns_or(v: Option<&Value>, fallback: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in v.and_then(Value::as_array).into_iter().flatten() {
        let Some(c) = c.as_str() else { continue };
        if js_trim(c).is_empty() || out.iter().any(|o| fold(o) == fold(c)) {
            continue;
        }
        out.push(c.to_owned());
        if out.len() == MAX_COLUMNS {
            break;
        }
    }
    if out.is_empty() {
        fallback.iter().map(|c| (*c).to_owned()).collect()
    } else {
        out
    }
}

/// Columns as stored; none → Yes / No.
pub fn read_columns(v: Option<&Value>) -> Vec<String> {
    read_columns_or(v, &DEFAULT_COLUMNS)
}

/// A note's columns: its own, else one column of ticks.
pub fn read_note_columns(v: Option<&Value>) -> Vec<String> {
    read_columns_or(v, &[TICK_COLUMN])
}

/// Rows as stored: `{id, label}` with unique ids, at most sixty.
pub fn read_rows(v: Option<&Value>) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    for r in v.and_then(Value::as_array).into_iter().flatten() {
        let Some(o) = r.as_object() else { continue };
        let Some(id) = o
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if out.iter().any(|r| r.id == id) {
            continue;
        }
        out.push(Row {
            id: id.to_owned(),
            label: o
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
        if out.len() == MAX_ROWS {
            break;
        }
    }
    out
}

fn str_of(o: &JsonMap<String, Value>, k: &str) -> String {
    o.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// One stored pick, if it names a column.
pub fn read_pick(v: Option<&Value>) -> Option<Pick> {
    let o = v?.as_object()?;
    let col = o
        .get("col")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    Some(Pick {
        col: col.to_owned(),
        by: str_of(o, "by"),
        by_id: str_of(o, "byId"),
        at: o.get("at").and_then(json_f64).unwrap_or(0.0),
    })
}

/// Who wrote a comment and when (`noteBy:` on a note); `None` when unknown.
fn read_note_by(v: Option<&Value>) -> Option<(String, String, f64)> {
    let o = v?.as_object()?;
    let by = o.get("by")?.as_str()?;
    Some((
        by.to_owned(),
        str_of(o, "byId"),
        o.get("at").and_then(json_f64).unwrap_or(0.0),
    ))
}

/// A legacy card's comment on a row, when it has words.
pub fn read_legacy_note(v: Option<&Value>) -> Option<Note> {
    let o = v?.as_object()?;
    let text = o
        .get("text")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())?;
    let (by, by_id, at) = read_note_by(v).unwrap_or_default();
    Some(Note {
        text: text.to_owned(),
        by,
        by_id,
        at,
    })
}

/// One stored assignee (tolerant: a bare string is a name).
pub fn read_assignee(v: &Value) -> Option<Assignee> {
    if let Value::String(s) = v {
        let name = assignee_name(s);
        return (!name.is_empty()).then(|| Assignee {
            name,
            ..Assignee::default()
        });
    }
    let o = v.as_object()?;
    let name = assignee_name(o.get("name")?.as_str()?);
    if name.is_empty() {
        return None;
    }
    let s = |k: &str| o.get(k).and_then(Value::as_str);
    Some(Assignee {
        name,
        id: s("id")
            .filter(|i| !js_trim(i).is_empty())
            .map(|i| js_slice(&js_trim(i).to_lowercase(), 80).to_owned()),
        email: s("email")
            .filter(|e| e.contains('@'))
            .map(|e| js_slice(js_trim(e), 200).to_owned()),
        by: s("by").filter(|b| !b.is_empty()).map(str::to_owned),
        by_id: s("byId").filter(|b| !b.is_empty()).map(str::to_owned),
        at: o.get("at").and_then(json_f64).filter(|a| *a > 0.0),
    })
}

/// The same person: the same id when both have one, else the same name (any case), else
/// the same email.
pub fn same_person(a: &Assignee, b: &Assignee) -> bool {
    if let (Some(x), Some(y)) = (
        a.id.as_ref().filter(|i| !i.is_empty()),
        b.id.as_ref().filter(|i| !i.is_empty()),
    ) {
        return x == y;
    }
    if fold(&a.name) == fold(&b.name) {
        return true;
    }
    match (
        a.email.as_ref().filter(|e| !e.is_empty()),
        b.email.as_ref().filter(|e| !e.is_empty()),
    ) {
        (Some(x), Some(y)) => fold(x) == fold(y),
        _ => false,
    }
}

/// A stored list of assignees, each person once, at most twelve.
pub fn read_assignee_list(v: Option<&Value>) -> Vec<Assignee> {
    let mut out: Vec<Assignee> = Vec::new();
    for raw in v.and_then(Value::as_array).into_iter().flatten() {
        let Some(a) = read_assignee(raw) else {
            continue;
        };
        if out.iter().any(|o| same_person(o, &a)) {
            continue;
        }
        out.push(a);
        if out.len() == MAX_ASSIGNEES {
            break;
        }
    }
    out
}

/// A legacy checklist on a shape map; `get` reads one prop as JSON.
pub fn read_checklist(get: &dyn Fn(&str) -> Option<Value>) -> Checklist {
    let columns = read_columns(get("columns").as_ref());
    let rows = read_rows(get("rows").as_ref());
    let picks = rows
        .iter()
        .filter_map(|r| {
            let p = read_pick(get(&pick_key(&r.id)).as_ref())?;
            columns.contains(&p.col).then(|| (r.id.clone(), p))
        })
        .collect();
    let notes = rows
        .iter()
        .filter_map(|r| {
            Some((
                r.id.clone(),
                read_legacy_note(get(&note_key(&r.id)).as_ref())?,
            ))
        })
        .collect();
    let assignees = rows
        .iter()
        .filter_map(|r| {
            let people = read_assignee_list(get(&legacy_assignees_key(&r.id)).as_ref());
            (!people.is_empty()).then(|| (r.id.clone(), people))
        })
        .collect();
    Checklist {
        columns,
        rows,
        picks,
        notes,
        assignees,
    }
}

/// A note row's assignees: the names its `@` lines hold, each with whatever is known
/// beside the note about that name.
pub fn note_assignees(get: &dyn Fn(&str) -> Option<Value>, row: &TextRow) -> Vec<Assignee> {
    if row.assignees.is_empty() {
        return Vec::new();
    }
    let known = read_assignee_list(get(&assignees_key(&row.label)).as_ref());
    row.assignees
        .iter()
        .map(
            |name| match known.iter().find(|p| fold(&p.name) == fold(name)) {
                Some(k) => Assignee {
                    name: name.clone(),
                    ..k.clone()
                },
                None => Assignee {
                    name: name.clone(),
                    ..Assignee::default()
                },
            },
        )
        .collect()
}

/// A note's checklist: its title and the card as the board reads it.
pub fn read_note_checklist(get: &dyn Fn(&str) -> Option<Value>, text: &str) -> (String, Checklist) {
    let columns = read_note_columns(get("columns").as_ref());
    let parsed = parse_checklist_text(text, &columns);
    let mut c = Checklist {
        rows: parsed
            .rows
            .iter()
            .map(|r| Row {
                id: note_row_id(r.index),
                label: r.label.clone(),
            })
            .collect(),
        ..Checklist::default()
    };
    for r in &parsed.rows {
        let id = note_row_id(r.index);
        if !r.note.is_empty() {
            let (by, by_id, at) =
                read_note_by(get(&note_by_key(&r.label)).as_ref()).unwrap_or_default();
            c.notes.push((
                id.clone(),
                Note {
                    text: r.note.clone(),
                    by,
                    by_id,
                    at,
                },
            ));
        }
        if !r.assignees.is_empty() {
            c.assignees.push((id.clone(), note_assignees(get, r)));
        }
        let Some(pick) = &r.pick else { continue };
        let who = read_pick(get(&who_key(&r.label)).as_ref())
            .filter(|w| fold(&w.col) == fold(pick))
            .map(|w| Pick {
                col: pick.clone(),
                ..w
            });
        c.picks.push((
            id,
            who.unwrap_or_else(|| Pick {
                col: pick.clone(),
                by: String::new(),
                by_id: String::new(),
                at: 0.0,
            }),
        ));
    }
    c.columns = columns;
    (parsed.title, c)
}

/// How many rows picked each column, in column order.
pub fn tally(c: &Checklist) -> Vec<(String, u64)> {
    c.columns
        .iter()
        .map(|col| {
            let n = c
                .rows
                .iter()
                .filter(|r| c.pick(&r.id).is_some_and(|p| &p.col == col))
                .count();
            (col.clone(), n as u64)
        })
        .collect()
}

/// `{column: count}` in column order (a JSON object, as the page prints it).
#[allow(clippy::ref_option)]
pub fn ser_tally<S: serde::Serializer>(
    v: &Option<Vec<(String, u64)>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap as _;
    let items = v.as_deref().unwrap_or_default();
    let mut m = s.serialize_map(Some(items.len()))?;
    for (k, n) in items {
        m.serialize_entry(k, n)?;
    }
    m.end()
}

/// One row as `canvas_read` shows it: `{id?, label, pick, by?, at?, note?, noteBy?,
/// noteAt?, assignees?}` (a note's rows have no `id`: their place is their id).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RowSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub label: String,
    pub pick: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(rename = "noteBy", skip_serializing_if = "Option::is_none")]
    pub note_by: Option<String>,
    #[serde(rename = "noteAt", skip_serializing_if = "Option::is_none")]
    pub note_at: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Vec<Value>>,
}

fn row_extras(c: &Checklist, row_id: &str, clip: &dyn Fn(&str) -> String, s: &mut RowSummary) {
    if let Some(n) = c.note(row_id) {
        s.note = Some(clip(&n.text));
        s.note_by = (!n.by.is_empty()).then(|| n.by.clone());
        s.note_at = (n.at != 0.0).then(|| jnum(n.at));
    }
    let people = c.people(row_id);
    if !people.is_empty() {
        s.assignees = Some(people.iter().map(Assignee::summary).collect());
    }
}

/// A legacy card's rows as `canvas_read` shows them (with ids; `by`/`at` on every pick).
pub fn row_summaries(c: &Checklist, clip: &dyn Fn(&str) -> String) -> Vec<RowSummary> {
    c.rows
        .iter()
        .map(|r| {
            let p = c.pick(&r.id);
            let mut s = RowSummary {
                id: Some(r.id.clone()),
                label: r.label.clone(),
                pick: p.map(|p| p.col.clone()),
                by: p.map(|p| p.by.clone()),
                at: p.map(|p| jnum(p.at)),
                note: None,
                note_by: None,
                note_at: None,
                assignees: None,
            };
            row_extras(c, &r.id, clip, &mut s);
            s
        })
        .collect()
}

/// A note's rows as `canvas_read` shows them: by label, who and when only when known.
pub fn note_row_summaries(c: &Checklist, clip: &dyn Fn(&str) -> String) -> Vec<RowSummary> {
    c.rows
        .iter()
        .map(|r| {
            let p = c.pick(&r.id);
            let mut s = RowSummary {
                id: None,
                label: r.label.clone(),
                pick: p.map(|p| p.col.clone()),
                by: p.filter(|p| !p.by.is_empty()).map(|p| p.by.clone()),
                at: p.filter(|p| p.at != 0.0).map(|p| jnum(p.at)),
                note: None,
                note_by: None,
                note_at: None,
                assignees: None,
            };
            row_extras(c, &r.id, clip, &mut s);
            s
        })
        .collect()
}

// ---- input (ops) --------------------------------------------------------------------------

/// `columns`: 1–4 distinct non-empty strings, each cut to 40 characters.
pub fn clean_columns(v: &Value) -> Result<Vec<String>, String> {
    let list = v
        .as_array()
        .filter(|l| (1..=MAX_COLUMNS).contains(&l.len()))
        .ok_or("`columns` must be a list of 1 to 4 names")?;
    let mut out: Vec<String> = Vec::new();
    for c in list {
        let name = c
            .as_str()
            .filter(|s| !js_trim(s).is_empty())
            .ok_or("`columns` names must be non-empty strings")?;
        let name = js_slice(js_trim(name), MAX_COLUMN_LABEL).to_owned();
        if out.iter().any(|o| fold(o) == fold(&name)) {
            return Err(format!("`columns` has {} twice", quoted(&name)));
        }
        out.push(name);
    }
    Ok(out)
}

/// `^[A-Za-z0-9_-]{1,32}$`
fn valid_row_id(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// A fresh row id: `r` and eight base-36 characters.
pub fn new_row_id() -> String {
    let mut n = uuid::Uuid::new_v4().as_u128();
    let mut id = String::from("r");
    for _ in 0..8 {
        let d = u32::try_from(n % 36).unwrap_or(0);
        id.push(char::from_digit(d, 36).unwrap_or('0'));
        n /= 36;
    }
    id
}

/// `rows`: up to 60 labels (`"Ann"`) or `{label, id?}`. A row keeps its id — and so
/// its pick — when it names an existing row's id, or failing that has the same label
/// as an existing row not yet taken.
pub fn clean_rows(v: &Value, existing: &[Row]) -> Result<Vec<Row>, String> {
    const SHAPE: &str = "`rows` must be a list of labels or {label, id?}";
    let list = v.as_array().ok_or(SHAPE)?;
    if list.len() > MAX_ROWS {
        return Err(format!("`rows`: at most {MAX_ROWS} rows"));
    }
    let mut given: Vec<(String, Option<String>)> = Vec::with_capacity(list.len());
    for r in list {
        match r {
            Value::String(s) => given.push((js_slice(js_trim(s), MAX_ROW_LABEL).to_owned(), None)),
            Value::Object(o) => {
                let label = [o.get("label"), o.get("text"), o.get("title")]
                    .into_iter()
                    .flatten()
                    .find(|v| !v.is_null())
                    .unwrap_or(&Value::Null);
                let label = match label {
                    Value::Null => "",
                    Value::String(s) => s.as_str(),
                    _ => return Err("`rows`: a label must be a string".into()),
                };
                let id = match o.get("id") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) if valid_row_id(s) => Some(s.clone()),
                    Some(_) => return Err("`rows`: an id must be 1–32 of A–Z a–z 0–9 _ -".into()),
                };
                given.push((js_slice(js_trim(label), MAX_ROW_LABEL).to_owned(), id));
            }
            _ => return Err(SHAPE.into()),
        }
    }
    let mut taken: Vec<String> = Vec::new();
    for id in given.iter().filter_map(|(_, id)| id.as_ref()) {
        if taken.contains(id) {
            return Err(format!("`rows`: id {id} is used twice"));
        }
        taken.push(id.clone());
    }
    let mut out = Vec::with_capacity(given.len());
    for (label, id) in given {
        let id = if let Some(id) = id {
            id
        } else {
            let mut id = existing
                .iter()
                .find(|r| r.label == label && !taken.contains(&r.id))
                .map(|r| r.id.clone());
            while id.as_ref().is_none_or(|i| taken.contains(i)) {
                id = Some(new_row_id());
            }
            let id = id.unwrap_or_default();
            taken.push(id.clone());
            id
        };
        out.push(Row { id, label });
    }
    Ok(out)
}

/// The row `reference` names: its id, its label, or its label in any case. `field` names
/// the op's key in a complaint.
pub fn find_row<'a>(rows: &'a [Row], reference: &str, field: &str) -> Result<&'a Row, String> {
    if let Some(r) = rows.iter().find(|r| r.id == reference) {
        return Ok(r);
    }
    let exact: Vec<&Row> = rows.iter().filter(|r| r.label == reference).collect();
    let folded: Vec<&Row> = rows
        .iter()
        .filter(|r| fold(&r.label) == fold(reference))
        .collect();
    for hits in [exact, folded] {
        match hits.len() {
            0 => {}
            1 => return Ok(hits[0]),
            _ => {
                return Err(format!(
                    "`{field}`: more than one row is called {} — use its id",
                    quoted(reference)
                ))
            }
        }
    }
    Err(format!("`{field}`: no row {}", quoted(reference)))
}

/// Keeps the first position of a row, the last value given for it (a JavaScript `Map`).
fn set_entry<T>(out: &mut Vec<(String, T)>, id: &str, v: T) {
    if let Some(at) = out.iter().position(|(r, _)| r == id) {
        out[at].1 = v;
    } else {
        out.push((id.to_owned(), v));
    }
}

/// `picks`: `{rowIdOrLabel: column | true | null}` → `[(rowId, column | None)]`. A column
/// is named in any case; `true` is the first column; null, false or "" clears the row.
pub fn clean_picks(
    v: &Value,
    rows: &[Row],
    columns: &[String],
) -> Result<Vec<(String, Option<String>)>, String> {
    let o = v
        .as_object()
        .ok_or("`picks` must be an object like {\"Ann\": \"Yes\"}")?;
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for (reference, value) in js_entries(o) {
        let row = find_row(rows, reference, "picks")?;
        let col = match value {
            Value::Null | Value::Bool(false) => None,
            Value::String(s) if s.is_empty() => None,
            Value::Bool(true) => columns.first().cloned(),
            Value::String(s) => Some(
                columns
                    .iter()
                    .find(|c| fold(c) == fold(s))
                    .cloned()
                    .ok_or_else(|| {
                        format!("`picks`: no column {} ({})", quoted(s), columns.join(", "))
                    })?,
            ),
            _ => return Err("`picks` values must be a column name, true or null".into()),
        };
        set_entry(&mut out, &row.id, col);
    }
    Ok(out)
}

/// `notes`: `{rowIdOrLabel: text | null}` → `[(rowId, text)]`, `""` clearing the row's
/// comment. Text is cut to [`MAX_NOTE`].
pub fn clean_notes(v: &Value, rows: &[Row]) -> Result<Vec<(String, String)>, String> {
    let o = v
        .as_object()
        .ok_or("`notes` must be an object like {\"Ann\": \"Back at 6\"}")?;
    let mut out: Vec<(String, String)> = Vec::new();
    for (reference, value) in js_entries(o) {
        let row = find_row(rows, reference, "notes")?;
        let text = match value {
            Value::Null | Value::Bool(false) => String::new(),
            Value::String(s) => clean_note(s),
            _ => return Err("`notes` values must be text or null".into()),
        };
        set_entry(&mut out, &row.id, text);
    }
    Ok(out)
}

/// One assignee as an agent names them — a name, an email or a user id (or `{name, id?,
/// email?}`) — resolved against the people on the canvas: their id, email, name, or the
/// start of their name; else a name of its own.
pub fn resolve_assignee(v: &Value, people: &[Person]) -> Result<Assignee, String> {
    let owned;
    let o = match v {
        Value::String(s) => {
            let mut m = JsonMap::new();
            m.insert("name".into(), Value::String(s.clone()));
            owned = m;
            &owned
        }
        Value::Object(o) => o,
        _ => {
            return Err(
                "`assignees` names must be strings like \"Ann\" or \"ann@example.com\"".into(),
            )
        }
    };
    let s = |k: &str| o.get(k).and_then(Value::as_str);
    let said = [s("id"), s("email"), s("name")]
        .into_iter()
        .flatten()
        .find(|x| !js_trim(x).is_empty())
        .ok_or("`assignees`: each needs a name, an email or an id")?;
    let key = js_trim(said).trim_start_matches('@').to_lowercase();
    // The start of a name only when it is one person's.
    let starts: Vec<&Person> = if js_len(&key) >= 2 {
        people
            .iter()
            .filter(|p| p.name.to_lowercase().starts_with(&key))
            .collect()
    } else {
        Vec::new()
    };
    let hit = people
        .iter()
        .find(|p| p.id.to_lowercase() == key)
        .or_else(|| {
            people.iter().find(|p| {
                p.email
                    .as_ref()
                    .is_some_and(|e| !e.is_empty() && e.to_lowercase() == key)
            })
        })
        .or_else(|| people.iter().find(|p| p.name.to_lowercase() == key))
        .or_else(|| (starts.len() == 1).then(|| starts[0]));
    if let Some(hit) = hit {
        let mut name = assignee_name(&hit.name);
        if name.is_empty() {
            name = assignee_name(hit.email.as_deref().unwrap_or_default());
        }
        if name.is_empty() {
            "Someone".clone_into(&mut name);
        }
        return Ok(Assignee {
            name,
            id: Some(hit.id.to_lowercase()),
            email: hit.email.clone().filter(|e| !e.is_empty()),
            ..Assignee::default()
        });
    }
    let name = assignee_name(s("name").filter(|n| !js_trim(n).is_empty()).unwrap_or(said));
    if name.is_empty() {
        return Err("`assignees`: a name must have letters in it".into());
    }
    let email = match s("email").filter(|e| e.contains('@')) {
        Some(e) => js_trim(e).to_owned(),
        None if said.contains('@') && !js_trim(said).chars().any(crate::jsstr::is_js_space) => {
            js_trim(said).to_owned()
        }
        None => String::new(),
    };
    Ok(Assignee {
        name,
        email: (!email.is_empty()).then_some(email),
        ..Assignee::default()
    })
}

/// `assignees`: `{rowIdOrLabel: [who, …] | null}` → `[(rowId, people)]`, an empty list
/// clearing the row. At most twelve to a row.
pub fn clean_assignees(
    v: &Value,
    rows: &[Row],
    people: &[Person],
) -> Result<Vec<(String, Vec<Assignee>)>, String> {
    let o = v
        .as_object()
        .ok_or("`assignees` must be an object like {\"Ann\": [\"Grace Hopper\"]}")?;
    let mut out: Vec<(String, Vec<Assignee>)> = Vec::new();
    for (reference, value) in js_entries(o) {
        let row = find_row(rows, reference, "assignees")?;
        let given: Vec<Value> = match value {
            Value::Null | Value::Bool(false) => {
                set_entry(&mut out, &row.id, Vec::new());
                continue;
            }
            Value::String(_) => vec![value.clone()],
            Value::Array(a) => a.clone(),
            _ => return Err("`assignees` values must be a list of names or null".into()),
        };
        if given.len() > MAX_ASSIGNEES {
            return Err(format!(
                "`assignees`: at most {MAX_ASSIGNEES} people on a row"
            ));
        }
        let mut list: Vec<Assignee> = Vec::new();
        for g in &given {
            let p = resolve_assignee(g, people)?;
            if !list.iter().any(|x| same_person(x, &p)) {
                list.push(p);
            }
        }
        set_entry(&mut out, &row.id, list);
    }
    Ok(out)
}

/// The people on a row as they will be, from `next`: who stays keeps who assigned them
/// and when; who is new is assigned by `by`, now. Also who is new.
pub fn merge_assignees(
    now: &[Assignee],
    next: &[Assignee],
    by: &Picker,
) -> (Vec<Assignee>, Vec<Assignee>) {
    let mut out: Vec<Assignee> = Vec::new();
    let mut added: Vec<Assignee> = Vec::new();
    for raw in next {
        let name = assignee_name(&raw.name);
        if name.is_empty() {
            continue;
        }
        let p = Assignee {
            name,
            ..raw.clone()
        };
        if out.iter().any(|o| same_person(o, &p)) {
            continue;
        }
        if let Some(was) = now.iter().find(|o| same_person(o, &p)) {
            out.push(Assignee {
                name: p.name.clone(),
                id: p.id.clone().or_else(|| was.id.clone()),
                email: p.email.clone().or_else(|| was.email.clone()),
                by: if was.by.as_ref().is_some_and(|b| !b.is_empty()) {
                    was.by.clone()
                } else {
                    p.by.clone().or_else(|| was.by.clone())
                },
                by_id: if was.by.as_ref().is_some_and(|b| !b.is_empty()) {
                    was.by_id.clone()
                } else {
                    p.by_id.clone().or_else(|| was.by_id.clone())
                },
                at: if was.at.is_some_and(|a| a != 0.0) {
                    was.at
                } else {
                    p.at.or(was.at)
                },
            });
        } else {
            #[allow(clippy::cast_precision_loss)]
            let fresh = Assignee {
                by: Some(by.name.to_owned()),
                by_id: Some(by.id.to_owned()),
                at: Some(by.at as f64),
                ..p
            };
            out.push(fresh.clone());
            added.push(fresh);
        }
        if out.len() == MAX_ASSIGNEES {
            break;
        }
    }
    (out, added)
}

// ---- writes (inside a transaction) ---------------------------------------------------------

/// Who a pick, comment or assignment is attributed to, and when.
#[derive(Debug, Clone)]
pub struct Picker<'a> {
    pub name: &'a str,
    pub id: &'a str,
    pub at: i64,
}

impl Picker<'_> {
    pub fn stamp(&self, mut o: JsonMap<String, Value>) -> Any {
        o.insert("by".into(), self.name.into());
        o.insert("byId".into(), self.id.into());
        o.insert("at".into(), self.at.into());
        json_to_any(&Value::Object(o))
    }
}

pub fn map_json<T: ReadTxn>(txn: &T, m: &MapRef, k: &str) -> Option<Value> {
    m.get(txn, k).map(|o| crate::schema::out_to_json(txn, &o))
}

pub(crate) fn remove_if_present(txn: &mut TransactionMut, m: &MapRef, key: &str) {
    if m.contains_key(txn, key) {
        m.remove(txn, key);
    }
}

pub(crate) fn string_list(items: &[String]) -> Any {
    Any::from(
        items
            .iter()
            .map(|c| Any::from(c.as_str()))
            .collect::<Vec<_>>(),
    )
}

pub(crate) fn people_any(people: &[Assignee]) -> Any {
    json_to_any(&Value::Array(people.iter().map(Assignee::stored).collect()))
}

/// Sets (or with `col` `None`, clears) one legacy row's pick. Touches no other row.
pub fn write_pick(
    txn: &mut TransactionMut,
    m: &MapRef,
    row_id: &str,
    col: Option<&str>,
    by: &Picker,
) {
    let key = pick_key(row_id);
    match col {
        None => remove_if_present(txn, m, &key),
        Some(col) => {
            let mut o = JsonMap::new();
            o.insert("col".into(), col.into());
            m.insert(txn, key, by.stamp(o));
        }
    }
}

/// New legacy columns. A pick naming a column whose label is unchanged but for case
/// follows it; a pick naming a column that has gone is dropped.
pub fn write_columns(txn: &mut TransactionMut, m: &MapRef, next: &[String]) {
    m.insert(txn, "columns", string_list(next));
    let keys: Vec<String> = m
        .keys(txn)
        .filter(|k| k.starts_with(PICK_PREFIX))
        .map(str::to_owned)
        .collect();
    for key in keys {
        let Some(pick) = read_pick(map_json(txn, m, &key).as_ref()) else {
            m.remove(txn, &key);
            continue;
        };
        match next.iter().rev().find(|c| fold(c) == fold(&pick.col)) {
            None => {
                m.remove(txn, &key);
            }
            Some(to) if *to != pick.col => {
                let moved = Pick {
                    col: to.clone(),
                    ..pick
                };
                m.insert(txn, key, json_to_any(&moved.to_json()));
            }
            Some(_) => {}
        }
    }
}

/// New legacy rows; the picks, comments and assignees of rows that are gone go with them.
pub fn write_rows(txn: &mut TransactionMut, m: &MapRef, next: &[Row]) {
    let rows: Vec<Value> = next.iter().map(Row::to_json).collect();
    m.insert(txn, "rows", json_to_any(&Value::Array(rows)));
    let keep: Vec<String> = next
        .iter()
        .flat_map(|r| {
            [
                pick_key(&r.id),
                note_key(&r.id),
                legacy_assignees_key(&r.id),
            ]
        })
        .collect();
    let gone: Vec<String> = m
        .keys(txn)
        .filter(|k| {
            (k.starts_with(PICK_PREFIX)
                || k.starts_with(NOTE_PREFIX)
                || k.starts_with(ASSIGNEES_PREFIX))
                && !keep.iter().any(|kk| kk == k)
        })
        .map(str::to_owned)
        .collect();
    for key in gone {
        m.remove(txn, &key);
    }
}

/// Sets (or with `""` clears) a legacy card's comment on one row.
pub fn write_legacy_note(
    txn: &mut TransactionMut,
    m: &MapRef,
    row_id: &str,
    text: &str,
    by: &Picker,
) {
    let key = note_key(row_id);
    if text.is_empty() {
        remove_if_present(txn, m, &key);
        return;
    }
    let mut o = JsonMap::new();
    o.insert("text".into(), text.into());
    m.insert(txn, key, by.stamp(o));
}

/// Sets a legacy row's assignees (merged with who is on it now).
pub fn write_legacy_assignees(
    txn: &mut TransactionMut,
    m: &MapRef,
    row_id: &str,
    next: &[Assignee],
    by: &Picker,
) {
    let key = legacy_assignees_key(row_id);
    let now = read_assignee_list(map_json(txn, m, &key).as_ref());
    let (people, _added) = merge_assignees(&now, next, by);
    if people.is_empty() {
        m.remove(txn, &key);
    } else {
        m.insert(txn, key, people_any(&people));
    }
}

/// What a legacy checklist update writes, checked against the lists as they will be.
#[derive(Debug, Default)]
pub struct LegacyPlan {
    pub columns: Option<Vec<String>>,
    pub rows: Option<Vec<Row>>,
    pub picks: Vec<(String, Option<String>)>,
    pub notes: Vec<(String, String)>,
    pub assignees: Vec<(String, Vec<Assignee>)>,
}

impl LegacyPlan {
    /// Checks `picks`, `notes` and `assignees` against the final rows and columns.
    pub fn check(
        columns: Option<Vec<String>>,
        rows: Option<Vec<Row>>,
        raw: &JsonMap<String, Value>,
        live: &Checklist,
        people: &[Person],
    ) -> Result<Self, String> {
        let final_rows = rows.as_deref().unwrap_or(&live.rows);
        let final_cols = columns.as_deref().unwrap_or(&live.columns);
        let picks = match raw.get("picks") {
            None => Vec::new(),
            Some(p) => clean_picks(p, final_rows, final_cols)?,
        };
        let notes = match raw.get("notes") {
            None => Vec::new(),
            Some(n) => clean_notes(n, final_rows)?,
        };
        let assignees = match raw.get("assignees") {
            None => Vec::new(),
            Some(a) => clean_assignees(a, final_rows, people)?,
        };
        Ok(Self {
            columns,
            rows,
            picks,
            notes,
            assignees,
        })
    }

    /// The page's `writeChecklist`: new rows and columns (dropping what they orphan), then
    /// the picks, comments and assignees; fresh lists also size the card unless the op
    /// sized it.
    pub fn write(
        &self,
        txn: &mut TransactionMut,
        m: &MapRef,
        live: &Checklist,
        live_w: f64,
        by: &Picker,
        sized: (bool, bool),
    ) {
        let rows = self.rows.as_deref().unwrap_or(&live.rows);
        let columns = self.columns.as_deref().unwrap_or(&live.columns);
        if let Some(c) = &self.columns {
            write_columns(txn, m, c);
        }
        if let Some(r) = &self.rows {
            write_rows(txn, m, r);
        }
        for (row, col) in &self.picks {
            write_pick(txn, m, row, col.as_deref(), by);
        }
        for (row, text) in &self.notes {
            write_legacy_note(txn, m, row, text, by);
        }
        for (row, next) in &self.assignees {
            write_legacy_assignees(txn, m, row, next, by);
        }
        if self.columns.is_some() && !sized.0 {
            let w = live_w.max(checklist_width(columns.len()));
            m.insert(txn, "w", crate::schema::num_any(w));
        }
        if (self.rows.is_some() || self.columns.is_some()) && !sized.1 {
            let h = checklist_height(columns.len(), rows.len(), 0);
            m.insert(txn, "h", crate::schema::num_any(h));
        }
    }
}

// ---- writes on a note in the checklist view ------------------------------------------------

/// Sets (or clears) a note row's pick and who made it.
pub fn write_note_pick(
    txn: &mut TransactionMut,
    m: &MapRef,
    t: &TextRef,
    row: &TextRow,
    col: Option<&str>,
    columns: &[String],
    by: &Picker,
) {
    write_row_pick(&mut YText { txn, t }, row, col, columns);
    let key = who_key(&row.label);
    match col {
        Some(col) if !col.is_empty() => {
            let mut o = JsonMap::new();
            o.insert("col".into(), col.into());
            m.insert(txn, key, by.stamp(o));
        }
        _ => remove_if_present(txn, m, &key),
    }
}

/// Sets (or with `""` clears) a note row's comment and who wrote it.
pub fn write_note_note(
    txn: &mut TransactionMut,
    m: &MapRef,
    t: &TextRef,
    row: &TextRow,
    text: &str,
    by: &Picker,
) {
    write_row_note(&mut YText { txn, t }, row, text);
    let key = note_by_key(&row.label);
    if text.is_empty() {
        remove_if_present(txn, m, &key);
    } else {
        m.insert(txn, key, by.stamp(JsonMap::new()));
    }
}

/// Sets a note row's assignees: their `@` lines, and who they are beside the note.
pub fn write_note_assignees(
    txn: &mut TransactionMut,
    m: &MapRef,
    t: &TextRef,
    row: &TextRow,
    people: &[Assignee],
) {
    let names: Vec<&str> = people.iter().map(|p| p.name.as_str()).collect();
    write_row_assignees(&mut YText { txn, t }, row, &names);
    let key = assignees_key(&row.label);
    if people.is_empty() {
        remove_if_present(txn, m, &key);
    } else {
        m.insert(txn, key, people_any(people));
    }
}

/// New columns for a note. Each row keeps its pick when the column stays (the same name in
/// any case) and loses it when the column goes; only lines whose meaning would change are
/// rewritten.
pub fn write_note_columns(txn: &mut TransactionMut, m: &MapRef, t: &TextRef, next: &[String]) {
    use yrs::GetString as _;
    let old = read_note_columns(map_json(txn, m, "columns").as_ref());
    let text = t.get_string(txn);
    let before = parse_checklist_text(&text, &old);
    let after = parse_checklist_text(&text, next);
    let want = |pick: Option<&str>| -> Option<String> {
        let pick = pick?;
        next.iter().rev().find(|c| fold(c) == fold(pick)).cloned()
    };
    // Last row first: each write stays inside its own line, so earlier offsets hold.
    for (i, row) in before.rows.iter().enumerate().rev() {
        let to = want(row.pick.as_deref());
        if let Some(now) = after.rows.get(i) {
            if now.pick == to
                && now.label == row.label
                && !(next.len() == 1 && row.suffix.is_some())
            {
                continue;
            }
        }
        write_row_pick(&mut YText { txn, t }, row, to.as_deref(), next);
    }
    m.insert(txn, "columns", string_list(next));
    let keys: Vec<String> = m
        .keys(txn)
        .filter(|k| k.starts_with(WHO_PREFIX))
        .map(str::to_owned)
        .collect();
    for key in keys {
        let who = read_pick(map_json(txn, m, &key).as_ref());
        let to = who.as_ref().and_then(|w| want(Some(&w.col)));
        match (who, to) {
            (Some(who), Some(to)) => {
                if to != who.col {
                    let moved = Pick { col: to, ..who };
                    m.insert(txn, key, json_to_any(&moved.to_json()));
                }
            }
            _ => {
                m.remove(txn, &key);
            }
        }
    }
}

#[cfg(test)]
mod tests;
