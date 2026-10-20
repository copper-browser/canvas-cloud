//! A checklist kept in a note's own text — the page's `canvas/checklist-text.ts`: a
//! `type:"sticky"` with `view:"checklist"`, whose body is the markdown every Copper
//! already draws as a note with clickable task boxes.
//!
//! ```text
//! ### Going            the title: the first #, ## or ### heading
//! - [ ] Ann            a row with no pick
//! - [x] Ada Lovelace   one column: ticked is the pick
//! - [x] Bob · No       several columns (`columns` on the shape): the suffix names the pick
//!   > A comment on Bob's row, as indented quote lines.
//!   @Grace Hopper      who the row is assigned to, one indented `@` line each
//! ```
//!
//! The text is the truth. A ticked line with no suffix (or one naming no column) picks the
//! first column; an unticked line has no pick. Every write changes only the characters of
//! its own lines, so edits to different rows from any two Coppers merge.
//!
//! Offsets are byte offsets (see `crate::jsstr`).

use std::sync::LazyLock;

use regex::Regex;

use crate::jsstr::{collapse_space, fold, js_regex, js_trim, js_trim_end, TextBuf};
use crate::schema::js_slice;

pub const VIEW_CHECKLIST: &str = "checklist";
pub const WHO_PREFIX: &str = "who:";
/// The one column of a note shown as a checklist without columns of its own.
pub const TICK_COLUMN: &str = "Done";
/// Characters in an assignee's name.
pub const MAX_ASSIGNEE_NAME: usize = 80;

/// How the page writes a comment line (before its words).
const NOTE_LEAD: &str = "  >";
/// How the page writes an assignee line (before the name).
const ASSIGNEE_LEAD: &str = "  @";

static HEADING: LazyLock<Regex> = LazyLock::new(|| js_regex(r"^(#{1,3})(\s+)(%.*?)\s*#*\s*$"));
/// `- [ ] label`, `* [x] label`, `1. [ ] label`; a doubled box (`[xx]`) still reads.
static TASK_LINE: LazyLock<Regex> = LazyLock::new(|| {
    js_regex(r"^(\s*(?:[-*]|[0-9]{1,9}[.)])\s+)\[([ xX]{1,3})\](?:([ \t]+)(%.*))?$")
});
static SUFFIX: LazyLock<Regex> = LazyLock::new(|| js_regex(r"\s+·\s+([^·]+?)\s*$"));
/// A comment line under a task line: indented, then `>` and one optional space.
static NOTE_LINE: LazyLock<Regex> = LazyLock::new(|| js_regex(r"^[ \t]+> ?(%.*)$"));
/// An assignee line under a task line: indented, `@`, then the name.
static ASSIGNEE_LINE: LazyLock<Regex> =
    LazyLock::new(|| js_regex(r"^[ \t]+@[ \t]*(\S%.*?)[ \t]*$"));

/// The key of a row's who-picked entry: its label, trimmed, any case, single spaces.
pub fn row_key(label: &str) -> String {
    collapse_space(&js_trim(label).to_lowercase())
}

pub fn who_key(label: &str) -> String {
    format!("{WHO_PREFIX}{}", row_key(label))
}

/// One line, as a label or a title can only be (`/[\r\n]+/g` → `' '`).
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_break = false;
    for c in s.chars() {
        if c == '\r' || c == '\n' {
            if !in_break {
                out.push(' ');
            }
            in_break = true;
        } else {
            out.push(c);
            in_break = false;
        }
    }
    out
}

/// A comment as it is kept: line breaks are `\n` (`/\r\n?/g` → `\n`).
pub fn note_lines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// A comment's lines as they follow its task line (each with the line break before it).
pub fn note_block(note: &str) -> String {
    let clean = note_lines(note);
    if clean.is_empty() {
        return String::new();
    }
    clean
        .split('\n')
        .map(|l| {
            if l.is_empty() {
                format!("\n{NOTE_LEAD}")
            } else {
                format!("\n{NOTE_LEAD} {l}")
            }
        })
        .collect()
}

/// A name as an assignee line keeps it: one line, single spaces, no leading @, at most 80
/// characters (`""` for none).
pub fn assignee_name(s: &str) -> String {
    let collapsed = collapse_space(&one_line(s));
    let t = js_trim(&collapsed);
    let t = t.trim_start_matches('@');
    let t = crate::jsstr::js_trim_start(t);
    js_trim(js_slice(t, MAX_ASSIGNEE_NAME)).to_owned()
}

/// Names as a row keeps them: cleaned, none empty, each person once (any case).
pub fn assignee_names<S: AsRef<str>>(names: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in names {
        let name = assignee_name(raw.as_ref());
        if !name.is_empty() && !out.iter().any(|o| fold(o) == fold(&name)) {
            out.push(name);
        }
    }
    out
}

/// Assignees' lines as they follow a row's comment (each with the line break before it).
pub fn assignee_block<S: AsRef<str>>(names: &[S]) -> String {
    use std::fmt::Write as _;
    assignee_names(names)
        .iter()
        .fold(String::new(), |mut out, n| {
            let _ = write!(out, "\n{ASSIGNEE_LEAD}{n}");
            out
        })
}

/// One task line and what hangs under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRow {
    /// Order among the task lines, from 0.
    pub index: usize,
    pub label: String,
    pub ticked: bool,
    /// The column a ` · Column` suffix names, if it names one.
    pub suffix: Option<String>,
    /// The suffix, or the first column when ticked without one; `None` unticked.
    pub pick: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
    /// Inside the brackets.
    pub box_start: usize,
    pub box_end: usize,
    pub label_start: usize,
    pub label_end: usize,
    /// Something (a space at least) follows the box.
    pub has_body: bool,
    /// The row's comment (`""` for none).
    pub note: String,
    /// The names of its `@` lines, in order, each once.
    pub assignees: Vec<String>,
    /// Where its first `@` line starts, counting the line break before it.
    pub assign_start: Option<usize>,
    /// Comment lines first, then `@` lines, as the page writes them.
    pub tidy: bool,
    /// Where the row's last line ends.
    pub block_end: usize,
}

/// The title's heading: where its words sit and where its line ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heading {
    pub start: usize,
    pub end: usize,
    pub line_end: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedChecklist {
    pub title: String,
    pub heading: Option<Heading>,
    pub rows: Vec<TextRow>,
}

/// Reads the title and the rows out of a note's text. Never fails.
#[allow(clippy::too_many_lines)]
pub fn parse_checklist_text<S: AsRef<str>>(text: &str, columns: &[S]) -> ParsedChecklist {
    let mut out = ParsedChecklist::default();
    let by_fold = |s: &str| -> Option<String> {
        let f = fold(s);
        columns
            .iter()
            .rev()
            .find(|c| fold(c.as_ref()) == f)
            .map(|c| c.as_ref().to_owned())
    };
    let mut at = 0usize;
    // The row whose comment lines may follow, and those lines so far.
    let mut open: Option<(usize, Vec<String>)> = None;
    for line in text.split('\n') {
        let start = at;
        let end = start + line.len();
        at = end + 1;
        if let Some((ri, lines)) = open.as_mut() {
            if let Some(c) = NOTE_LINE.captures(line) {
                lines.push(c.get(1).map_or("", |m| m.as_str()).to_owned());
                let row = &mut out.rows[*ri];
                row.note = lines.join("\n");
                row.block_end = end;
                if row.assign_start.is_some() {
                    row.tidy = false;
                }
                continue;
            }
            if let Some(c) = ASSIGNEE_LINE.captures(line) {
                let row = &mut out.rows[*ri];
                if row.assign_start.is_none() {
                    row.assign_start = Some(start - 1);
                }
                let name = assignee_name(c.get(1).map_or("", |m| m.as_str()));
                if !name.is_empty() && !row.assignees.iter().any(|n| fold(n) == fold(&name)) {
                    row.assignees.push(name);
                } else {
                    row.tidy = false;
                }
                row.block_end = end;
                continue;
            }
            open = None;
        }
        if let Some(task) = TASK_LINE.captures(line) {
            let lead = task.get(1).map_or("", |m| m.as_str());
            let bx = task.get(2).map_or(" ", |m| m.as_str());
            let space = task.get(3).map_or("", |m| m.as_str());
            let body = js_trim_end(task.get(4).map_or("", |m| m.as_str()));
            let box_start = start + lead.len() + 1;
            let box_end = box_start + bx.len();
            let label_start = box_end + 1 + space.len();
            let mut label = body.to_owned();
            let mut suffix = None;
            if let Some(m) = SUFFIX.captures(body) {
                if let Some(named) = by_fold(m.get(1).map_or("", |g| g.as_str())) {
                    body[..m.get(0).map_or(0, |g| g.start())].clone_into(&mut label);
                    suffix = Some(named);
                }
            }
            let ticked = bx.contains(['x', 'X']);
            let pick = if ticked {
                suffix
                    .clone()
                    .or_else(|| columns.first().map(|c| c.as_ref().to_owned()))
            } else {
                None
            };
            let label_end = label_start + label.len();
            out.rows.push(TextRow {
                index: out.rows.len(),
                label,
                ticked,
                suffix,
                pick,
                line_start: start,
                line_end: end,
                box_start,
                box_end,
                label_start,
                label_end,
                has_body: task.get(3).is_some(),
                note: String::new(),
                assignees: Vec::new(),
                assign_start: None,
                tidy: true,
                block_end: end,
            });
            open = Some((out.rows.len() - 1, Vec::new()));
            continue;
        }
        if out.heading.is_none() {
            if let Some(h) = HEADING.captures(line) {
                let title_start = start
                    + h.get(1).map_or(0, |m| m.as_str().len())
                    + h.get(2).map_or(0, |m| m.as_str().len());
                h.get(3)
                    .map_or("", |m| m.as_str())
                    .clone_into(&mut out.title);
                out.heading = Some(Heading {
                    start: title_start,
                    end: title_start + out.title.len(),
                    line_end: end,
                });
            }
        }
    }
    out
}

/// A row's line as the page writes it.
pub fn row_line<S: AsRef<str>>(label: &str, pick: Option<&str>, columns: &[S]) -> String {
    let tail = match pick {
        Some(p) if !p.is_empty() && columns.len() > 1 => format!(" · {p}"),
        _ => String::new(),
    };
    let tick = if pick.is_some_and(|p| !p.is_empty()) {
        'x'
    } else {
        ' '
    };
    format!("- [{tick}] {}{tail}", one_line(label))
}

/// A row's line and, under it, its comment's lines, then its assignees'.
pub fn row_block<S: AsRef<str>, N: AsRef<str>>(
    label: &str,
    pick: Option<&str>,
    columns: &[S],
    note: &str,
    assignees: &[N],
) -> String {
    format!(
        "{}{}{}",
        row_line(label, pick, columns),
        note_block(note),
        assignee_block(assignees)
    )
}

/// One row for [`checklist_markdown`].
#[derive(Debug, Clone, Default)]
pub struct MdRow {
    pub label: String,
    pub pick: Option<String>,
    pub note: String,
    pub assignees: Vec<String>,
}

/// A whole checklist note: the title as a heading, then one block per row.
pub fn checklist_markdown<S: AsRef<str>>(title: &str, rows: &[MdRow], columns: &[S]) -> String {
    let mut lines: Vec<String> = rows
        .iter()
        .map(|r| row_block(&r.label, r.pick.as_deref(), columns, &r.note, &r.assignees))
        .collect();
    if !js_trim(title).is_empty() {
        lines.insert(0, format!("### {}", js_trim(&one_line(title))));
    }
    lines.join("\n")
}

// ---- writes on the note's text ---------------------------------------------------------

/// Sets (or with `None` clears) one row's pick: its box and its suffix change, nothing
/// else. `row` is from a parse of the text as it is now.
pub fn write_row_pick<B: TextBuf + ?Sized, S: AsRef<str>>(
    t: &mut B,
    row: &TextRow,
    col: Option<&str>,
    columns: &[S],
) {
    let text = t.text();
    let want_tail = match col {
        Some(c) if !c.is_empty() && columns.len() > 1 => format!(" · {c}"),
        _ => String::new(),
    };
    let tail = &text[row.label_end..row.line_end];
    // Right to left, so the box's offset still holds after the tail changes.
    if js_trim_end(tail) != want_tail {
        t.splice(row.label_end, tail.len(), &want_tail);
    }
    let want_box = if col.is_some_and(|c| !c.is_empty()) {
        "x"
    } else {
        " "
    };
    if &text[row.box_start..row.box_end] != want_box {
        t.splice(row.box_start, row.box_end - row.box_start, want_box);
    }
}

/// Sets (or with `""` clears) one row's comment: only its comment lines change.
pub fn write_row_note<B: TextBuf + ?Sized>(t: &mut B, row: &TextRow, note: &str) {
    let text = t.text();
    if !row.tidy {
        write_row_tail(t, &text, row, note, &row.assignees);
        return;
    }
    let prev = &text[row.line_end..row.assign_start.unwrap_or(row.block_end)];
    let next = note_block(note);
    if prev == next {
        return;
    }
    // A first comment goes in just after the line break that ends the row's line.
    if prev.is_empty() && row.line_end < text.len() {
        t.insert(row.line_end + 1, &format!("{}\n", &next[1..]));
        return;
    }
    t.splice_within(row.line_end, prev, &next);
}

/// Sets the people a row is assigned to (names; none clears them): only its `@` lines
/// change.
pub fn write_row_assignees<B: TextBuf + ?Sized, N: AsRef<str>>(
    t: &mut B,
    row: &TextRow,
    names: &[N],
) {
    let text = t.text();
    if !row.tidy {
        write_row_tail(t, &text, row, &row.note, names);
        return;
    }
    let next = assignee_block(names);
    match row.assign_start {
        None => {
            if next.is_empty() {
                return;
            }
            if row.block_end == row.line_end && row.line_end < text.len() {
                t.insert(row.line_end + 1, &format!("{}\n", &next[1..]));
            } else {
                t.insert(row.block_end, &next);
            }
        }
        Some(a) => t.splice_within(a, &text[a..row.block_end], &next),
    }
}

/// A block whose lines are out of order: all its lines under the row, as the page writes
/// them.
fn write_row_tail<B: TextBuf + ?Sized, N: AsRef<str>>(
    t: &mut B,
    text: &str,
    row: &TextRow,
    note: &str,
    names: &[N],
) {
    let next = format!("{}{}", note_block(note), assignee_block(names));
    t.splice_within(row.line_end, &text[row.line_end..row.block_end], &next);
}

/// The title: the first heading's words, or a new `###` heading on top.
pub fn write_title<B: TextBuf + ?Sized>(t: &mut B, parsed: &ParsedChecklist, title: &str) {
    let next = one_line(title);
    if let Some(h) = parsed.heading {
        t.splice_within(h.start, &parsed.title, &next);
        return;
    }
    if js_trim(&next).is_empty() {
        return;
    }
    let empty = t.text().is_empty();
    t.insert(
        0,
        &if empty {
            format!("### {next}")
        } else {
            format!("### {next}\n")
        },
    );
}

/// Replaces every row (with its comment's and assignees' lines) by `next` — one row's
/// block each — where the rows were (or under the title).
pub fn replace_rows<B: TextBuf + ?Sized>(t: &mut B, parsed: &ParsedChecklist, next: &[String]) {
    let before = t.text();
    let lines: Vec<&str> = before.split('\n').collect();
    let mut is_row = vec![false; lines.len()];
    let mut first_row_line: Option<usize> = None;
    let mut heading_line: Option<usize> = None;
    let mut n = 0usize;
    let mut at = 0usize;
    for (i, l) in lines.iter().enumerate() {
        let start = at;
        at += l.len() + 1;
        match parsed.rows.get(n) {
            Some(row) if start >= row.line_start && start <= row.block_end => {
                is_row[i] = true;
                first_row_line.get_or_insert(i);
                if at > row.block_end {
                    n += 1;
                }
            }
            _ => {
                if let Some(h) = parsed.heading {
                    if start <= h.start && h.line_end <= at {
                        heading_line = Some(i);
                    }
                }
            }
        }
    }
    let insert_at = first_row_line
        .or(heading_line.map(|h| h + 1))
        .unwrap_or(lines.len());
    let mut out: Vec<&str> = Vec::new();
    for i in 0..=lines.len() {
        if i == insert_at {
            out.extend(next.iter().map(String::as_str));
        }
        if i < lines.len() && !is_row[i] {
            out.push(lines[i]);
        }
    }
    // A note that was empty does not keep a blank first line.
    while out.len() > 1 && out[0].is_empty() && before.is_empty() {
        out.remove(0);
    }
    let after = out.join("\n");
    t.splice_within(0, &before, &after);
}

#[cfg(test)]
mod tests {
    use super::*;

    const YN: [&str; 2] = ["Yes", "No"];

    #[test]
    fn parses_rows_notes_and_assignees() {
        let text = "### Going\n- [x] Bob · No\n  > Busy\n  >\n  > Sorry\n  @Ann\n  @ ann\n- [ ] Cy\n* [X] Dee";
        let p = parse_checklist_text(text, &YN);
        assert_eq!(p.title, "Going");
        assert_eq!(p.rows.len(), 3);
        let bob = &p.rows[0];
        assert_eq!(bob.label, "Bob");
        assert_eq!(bob.pick.as_deref(), Some("No"));
        assert_eq!(bob.note, "Busy\n\nSorry");
        assert_eq!(bob.assignees, ["Ann"]);
        assert!(!bob.tidy, "a name written twice is untidy");
        assert_eq!(p.rows[1].pick, None);
        assert_eq!(p.rows[2].pick.as_deref(), Some("Yes"));
    }

    #[test]
    fn writes_only_their_lines() {
        let cols = ["Yes", "No"];
        let mut t = checklist_markdown(
            "Party",
            &[
                MdRow {
                    label: "Ann".into(),
                    ..MdRow::default()
                },
                MdRow {
                    label: "Bob".into(),
                    pick: Some("No".into()),
                    ..MdRow::default()
                },
            ],
            &cols,
        );
        assert_eq!(t, "### Party\n- [ ] Ann\n- [x] Bob · No");
        let p = parse_checklist_text(&t, &cols);
        write_row_pick(&mut t, &p.rows[0], Some("Yes"), &cols);
        assert_eq!(t, "### Party\n- [x] Ann · Yes\n- [x] Bob · No");
        let p = parse_checklist_text(&t, &cols);
        write_row_note(&mut t, &p.rows[0], "Back at 6");
        assert_eq!(
            t,
            "### Party\n- [x] Ann · Yes\n  > Back at 6\n- [x] Bob · No"
        );
        let p = parse_checklist_text(&t, &cols);
        write_row_assignees(&mut t, &p.rows[1], &["Grace"]);
        assert_eq!(
            t,
            "### Party\n- [x] Ann · Yes\n  > Back at 6\n- [x] Bob · No\n  @Grace"
        );
        let p = parse_checklist_text(&t, &cols);
        write_title(&mut t, &p, "Dinner");
        let p = parse_checklist_text(&t, &cols);
        replace_rows(&mut t, &p, &["- [ ] Cy".to_owned()]);
        assert_eq!(t, "### Dinner\n- [ ] Cy");
        assert_eq!(assignee_name("  @@ Ada \n Lovelace "), "Ada Lovelace");
        assert_eq!(row_key("  Ann  B "), "ann b");
    }
}
