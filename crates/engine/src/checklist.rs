//! The checklist (RSVP) shape — the page's `Canvas/src/canvas/checklist.ts`: a title,
//! 1–4 columns ("Yes", "No") and up to 60 rows, each row with at most one pick.
//!
//! Stored on the shape's own `Y.Map`, beside the usual box props:
//!
//! ```text
//! title          string
//! columns        string[]                  whole value, last writer wins
//! rows           {id, label}[]             whole value, last writer wins
//! pick:<rowId>   {col, by, byId, at}       one key per row
//! ```
//!
//! A pick lives under its row's own key, so two people picking different rows at the
//! same moment both stick. `col` is the column's label; a pick naming no current column
//! (or no current row) reads as no pick.

use serde::Serialize;
use serde_json::{json, Map as JsonMap, Value};
use yrs::{Any, Map, MapRef, ReadTxn, TransactionMut};

use crate::schema::{jnum, js_slice, json_f64, json_to_any};

/// Prefix of the per-row pick keys.
pub const PICK_PREFIX: &str = "pick:";
pub const MAX_COLUMNS: usize = 4;
pub const MAX_ROWS: usize = 60;
/// Labels are cut to these many UTF-16 units (as JavaScript counts).
pub const MAX_COLUMN_LABEL: usize = 40;
pub const MAX_ROW_LABEL: usize = 200;
pub const DEFAULT_COLUMNS: [&str; 2] = ["Yes", "No"];

/// Layout numbers shared with the page (world px), for a fresh card's size.
const HEAD: f64 = 49.0;
const COLUMNS_HEAD: f64 = 28.0;
const ROW: f64 = 34.0;
const FOOT: f64 = 14.0;
const COL: f64 = 64.0;
const LABEL: f64 = 168.0;
const PAD: f64 = 14.0;

/// The pick key of a row.
pub fn pick_key(row_id: &str) -> String {
    format!("{PICK_PREFIX}{row_id}")
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

/// The height a card needs before anyone has measured it (one line per label).
#[allow(clippy::cast_precision_loss)]
pub fn checklist_height(columns: usize, rows: usize) -> f64 {
    HEAD + if columns > 1 { COLUMNS_HEAD } else { 0.0 } + ROW * rows.max(1) as f64 + FOOT
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

/// A checklist as read from the document.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Checklist {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    /// Picks that name a current row and column, in row order.
    pub picks: Vec<(String, Pick)>,
}

impl Checklist {
    pub fn pick(&self, row_id: &str) -> Option<&Pick> {
        self.picks.iter().find(|(r, _)| r == row_id).map(|(_, p)| p)
    }
}

/// `s.trim().toLowerCase()`.
fn fold(s: &str) -> String {
    s.trim().to_lowercase()
}

/// `JSON.stringify(s)`, for messages.
fn quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

// ---- tolerant reads -----------------------------------------------------------------------

/// Columns as stored: strings, deduplicated, at most four; none → Yes / No.
pub fn read_columns(v: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in v.and_then(Value::as_array).into_iter().flatten() {
        let Some(c) = c.as_str() else { continue };
        if c.trim().is_empty() || out.iter().any(|o| fold(o) == fold(c)) {
            continue;
        }
        out.push(c.to_owned());
        if out.len() == MAX_COLUMNS {
            break;
        }
    }
    if out.is_empty() {
        DEFAULT_COLUMNS.iter().map(|c| (*c).to_owned()).collect()
    } else {
        out
    }
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

/// One stored pick, if it names a column.
pub fn read_pick(v: Option<&Value>) -> Option<Pick> {
    let o = v?.as_object()?;
    let col = o
        .get("col")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let s = |k: &str| {
        o.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Some(Pick {
        col: col.to_owned(),
        by: s("by"),
        by_id: s("byId"),
        at: o.get("at").and_then(json_f64).unwrap_or(0.0),
    })
}

/// The checklist on a shape map; `get` reads one prop as JSON.
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
    Checklist {
        columns,
        rows,
        picks,
    }
}

/// How many rows picked each column, in column order.
pub fn tally(c: &Checklist) -> Vec<(String, u64)> {
    c.columns
        .iter()
        .map(|col| {
            let n = c.picks.iter().filter(|(_, p)| &p.col == col).count();
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

/// One row as `canvas_read` shows it: `{id, label, pick, by?, at?}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RowSummary {
    pub id: String,
    pub label: String,
    pub pick: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<Value>,
}

/// The rows as `canvas_read` shows them.
pub fn row_summaries(c: &Checklist) -> Vec<RowSummary> {
    c.rows
        .iter()
        .map(|r| {
            let p = c.pick(&r.id);
            RowSummary {
                id: r.id.clone(),
                label: r.label.clone(),
                pick: p.map(|p| p.col.clone()),
                by: p.map(|p| p.by.clone()),
                at: p.map(|p| jnum(p.at)),
            }
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
            .filter(|s| !s.trim().is_empty())
            .ok_or("`columns` names must be non-empty strings")?;
        let name = js_slice(name.trim(), MAX_COLUMN_LABEL).to_owned();
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
            Value::String(s) => given.push((js_slice(s.trim(), MAX_ROW_LABEL).to_owned(), None)),
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
                given.push((js_slice(label.trim(), MAX_ROW_LABEL).to_owned(), id));
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

/// The row `reference` names: its id, its label, or its label in any case.
pub fn find_row<'a>(rows: &'a [Row], reference: &str) -> Result<&'a Row, String> {
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
                    "`picks`: more than one row is called {} — use its id",
                    quoted(reference)
                ))
            }
        }
    }
    Err(format!("`picks`: no row {}", quoted(reference)))
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
    for (reference, value) in o {
        let row = find_row(rows, reference)?;
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
        if let Some(at) = out.iter().position(|(r, _)| *r == row.id) {
            out[at].1 = col;
        } else {
            out.push((row.id.clone(), col));
        }
    }
    Ok(out)
}

// ---- writes (inside a transaction) ---------------------------------------------------------

/// Who a pick is attributed to.
#[derive(Debug, Clone)]
pub struct Picker<'a> {
    pub name: &'a str,
    pub id: &'a str,
    pub at: i64,
}

fn map_json<T: ReadTxn>(txn: &T, m: &MapRef, k: &str) -> Option<Value> {
    m.get(txn, k).map(|o| crate::schema::out_to_json(txn, &o))
}

/// Set (or with `col` `None`, clear) one row's pick. Touches no other row.
pub fn write_pick(
    txn: &mut TransactionMut,
    m: &MapRef,
    row_id: &str,
    col: Option<&str>,
    by: &Picker,
) {
    let key = pick_key(row_id);
    match col {
        None => {
            if m.contains_key(txn, &key) {
                m.remove(txn, &key);
            }
        }
        Some(col) => {
            let pick = Pick {
                col: col.to_owned(),
                by: by.name.to_owned(),
                by_id: by.id.to_owned(),
                #[allow(clippy::cast_precision_loss)]
                at: by.at as f64,
            };
            m.insert(txn, key, json_to_any(&pick.to_json()));
        }
    }
}

/// New columns. A pick naming a column whose label is unchanged but for case follows
/// it; a pick naming a column that has gone is dropped.
pub fn write_columns(txn: &mut TransactionMut, m: &MapRef, next: &[String]) {
    m.insert(
        txn,
        "columns",
        Any::from(
            next.iter()
                .map(|c| Any::from(c.as_str()))
                .collect::<Vec<_>>(),
        ),
    );
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
        match next.iter().find(|c| fold(c) == fold(&pick.col)) {
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

/// New rows; the picks of rows that are gone go with them.
pub fn write_rows(txn: &mut TransactionMut, m: &MapRef, next: &[Row]) {
    let rows: Vec<Value> = next.iter().map(Row::to_json).collect();
    m.insert(txn, "rows", json_to_any(&Value::Array(rows)));
    let keep: Vec<String> = next.iter().map(|r| pick_key(&r.id)).collect();
    let gone: Vec<String> = m
        .keys(txn)
        .filter(|k| k.starts_with(PICK_PREFIX) && !keep.iter().any(|kk| kk == k))
        .map(str::to_owned)
        .collect();
    for key in gone {
        m.remove(txn, &key);
    }
}

/// The checklist props of an add or update, split from the plain ones: `columns` and
/// `rows` (already cleaned) and the raw `picks`.
#[derive(Debug, Default)]
pub struct ChecklistInput {
    pub columns: Option<Vec<String>>,
    pub rows: Option<Vec<Row>>,
    pub picks: Option<Value>,
}

impl ChecklistInput {
    /// Takes `columns`, `rows` and `picks` out of cleaned props.
    pub fn take(props: &mut JsonMap<String, Value>) -> Self {
        let columns = props.remove("columns").map(|v| read_columns(Some(&v)));
        let rows = props.remove("rows").map(|v| {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| {
                    Some(Row {
                        id: r.get("id")?.as_str()?.to_owned(),
                        label: r.get("label")?.as_str()?.to_owned(),
                    })
                })
                .collect()
        });
        Self {
            columns,
            rows,
            picks: props.remove("picks"),
        }
    }
}

/// The page's `writeChecklist`: new rows and columns (dropping the picks they orphan),
/// then the op's picks; fresh lists also size the card unless the op sized it.
pub fn write_checklist(
    txn: &mut TransactionMut,
    m: &MapRef,
    input: &ChecklistInput,
    by: &Picker,
    sized_w: bool,
    sized_h: bool,
) -> Result<(), String> {
    let live = read_checklist(&|k| map_json(txn, m, k));
    let rows = input.rows.as_deref().unwrap_or(&live.rows);
    let columns = input.columns.as_deref().unwrap_or(&live.columns);
    let resolved = match &input.picks {
        None => Vec::new(),
        Some(p) => clean_picks(p, rows, columns)?,
    };
    let (rows, columns) = (rows.to_vec(), columns.to_vec());
    if input.columns.is_some() {
        write_columns(txn, m, &columns);
    }
    if input.rows.is_some() {
        write_rows(txn, m, &rows);
    }
    for (row, col) in &resolved {
        write_pick(txn, m, row, col.as_deref(), by);
    }
    if input.columns.is_some() && !sized_w {
        let w = map_json(txn, m, "w")
            .as_ref()
            .and_then(json_f64)
            .unwrap_or(0.0);
        let w = w.max(checklist_width(columns.len()));
        m.insert(txn, "w", crate::schema::num_any(w));
    }
    if (input.rows.is_some() || input.columns.is_some()) && !sized_h {
        let h = checklist_height(columns.len(), rows.len());
        m.insert(txn, "h", crate::schema::num_any(h));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(labels: &[(&str, &str)]) -> Vec<Row> {
        labels
            .iter()
            .map(|(id, label)| Row {
                id: (*id).to_owned(),
                label: (*label).to_owned(),
            })
            .collect()
    }

    #[test]
    fn reads_tolerantly() {
        assert_eq!(read_columns(None), ["Yes", "No"]);
        assert_eq!(
            read_columns(Some(&json!([
                "Yes", "yes", "", 3, "No", "Maybe", "Later", "Never"
            ]))),
            ["Yes", "No", "Maybe", "Later"]
        );
        assert_eq!(
            read_rows(Some(
                &json!([{"id": "a", "label": "A"}, {"id": "a", "label": "dup"}, {"label": "no id"}, "str", {"id": "b"}])
            )),
            rows(&[("a", "A"), ("b", "")])
        );
        assert_eq!(read_pick(Some(&json!({"col": ""}))), None);
        assert_eq!(
            read_pick(Some(&json!({"col": "Yes", "at": 5}))),
            Some(Pick {
                col: "Yes".into(),
                by: String::new(),
                by_id: String::new(),
                at: 5.0
            })
        );
    }

    #[test]
    fn cleans_columns() {
        assert_eq!(
            clean_columns(&json!([" Yes ", "No"])).unwrap(),
            ["Yes", "No"]
        );
        assert_eq!(
            clean_columns(&json!(["x".repeat(60)])).unwrap()[0].len(),
            40
        );
        assert!(clean_columns(&json!([])).unwrap_err().contains("1 to 4"));
        assert!(clean_columns(&json!(["a", "b", "c", "d", "e"]))
            .unwrap_err()
            .contains("1 to 4"));
        assert_eq!(
            clean_columns(&json!(["Yes", "yes"])).unwrap_err(),
            "`columns` has \"yes\" twice"
        );
        assert!(clean_columns(&json!(["Yes", ""]))
            .unwrap_err()
            .contains("non-empty"));
    }

    #[test]
    fn cleans_rows_keeping_ids() {
        let existing = rows(&[("a", "Ann"), ("b", "Bob")]);
        let out = clean_rows(
            &json!(["Bob", {"label": "Ann", "id": "a"}, "Cy", {"text": "Dee"}]),
            &existing,
        )
        .unwrap();
        let labels: Vec<&str> = out.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["Bob", "Ann", "Cy", "Dee"]);
        assert_eq!(out[0].id, "b");
        assert_eq!(out[1].id, "a");
        assert!(out.iter().all(|r| valid_row_id(&r.id)));
        let mut ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
        ids.dedup();
        assert_eq!(ids.len(), 4);
        assert!(clean_rows(&json!("Ann"), &[]).unwrap_err().contains("list"));
        let many: Vec<String> = (0..61).map(|i| format!("p{i}")).collect();
        assert!(clean_rows(&json!(many), &[]).unwrap_err().contains("60"));
        assert!(clean_rows(&json!([{"label": "x", "id": "bad id!"}]), &[])
            .unwrap_err()
            .contains("id"));
        assert!(clean_rows(
            &json!([{"label": "x", "id": "a"}, {"label": "y", "id": "a"}]),
            &[]
        )
        .unwrap_err()
        .contains("twice"));
        assert!(clean_rows(&json!([{"label": 3}]), &[])
            .unwrap_err()
            .contains("string"));
    }

    #[test]
    fn cleans_picks() {
        let rs = rows(&[("a", "Ann"), ("b", "Bob"), ("c", "bob")]);
        let cols = vec!["Yes".to_owned(), "No".to_owned()];
        let mut got =
            clean_picks(&json!({"a": "yes", "Bob": "No", "c": null}), &rs, &cols).unwrap();
        got.sort();
        assert_eq!(
            got,
            [
                ("a".to_owned(), Some("Yes".to_owned())),
                ("b".to_owned(), Some("No".to_owned())),
                ("c".to_owned(), None)
            ]
        );
        assert_eq!(
            clean_picks(&json!({"ann": true}), &rs, &["Done".to_owned()]).unwrap(),
            [("a".to_owned(), Some("Done".to_owned()))]
        );
        assert!(clean_picks(&json!({"BOB": "Yes"}), &rs, &cols)
            .unwrap_err()
            .contains("more than one"));
        assert_eq!(
            clean_picks(&json!({"Zed": "Yes"}), &rs, &cols).unwrap_err(),
            "`picks`: no row \"Zed\""
        );
        assert_eq!(
            clean_picks(&json!({"Ann": "Maybe"}), &rs, &cols).unwrap_err(),
            "`picks`: no column \"Maybe\" (Yes, No)"
        );
        assert!(clean_picks(&json!(["Ann"]), &rs, &cols)
            .unwrap_err()
            .contains("object"));
        assert!(clean_picks(&json!({"Ann": 1}), &rs, &cols)
            .unwrap_err()
            .contains("values"));
    }

    #[test]
    fn sizes_like_the_page() {
        assert!((checklist_width(2) - 324.0).abs() < f64::EPSILON);
        assert!((checklist_width(1) - 280.0).abs() < f64::EPSILON);
        assert!((checklist_height(2, 3) - (49.0 + 28.0 + 102.0 + 14.0)).abs() < f64::EPSILON);
        assert!((checklist_height(1, 0) - 97.0).abs() < f64::EPSILON);
        let id = new_row_id();
        assert!(
            id.starts_with('r') && id.len() == 9 && valid_row_id(&id),
            "{id}"
        );
    }
}
