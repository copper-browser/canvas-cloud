//! A timeline kept in a note's own text — the page's `canvas/timeline-text.ts`: a
//! `type:"sticky"` with `view:"timeline"`, whose body is markdown every Copper already draws
//! as a readable note.
//!
//! ```text
//! ### Q4 launch                                  the title: the first #, ## or ### heading
//! 2026-10-01 → 2026-12-31 · weekly               the range and its step (daily … yearly)
//!
//! - 2026-10-14 · Kickoff                         a pin: one date (a note)
//! - 2026-10-20 → 2026-11-03 · Build sprint · blue    a section: a span of days, in a colour
//! - 2026-11-18 · Launch review · purple
//!   > Bubble body, markdown,                     a pin with quote lines under it is a bubble
//!   > as quote lines under the item.
//! ```
//!
//! The text is the truth. Every write changes only the characters of its own lines, so
//! edits to different items from any two Coppers merge. Offsets are byte offsets (see
//! `crate::jsstr`).

use std::sync::LazyLock;

use regex::Regex;

use crate::checklist_text::one_line;
use crate::jsstr::{js_regex, js_trim, TextBuf};

pub const VIEW_TIMELINE: &str = "timeline";

/// A timeline's step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Year => "year",
        }
    }
    /// The word a range line says.
    pub fn word(self) -> &'static str {
        match self {
            Self::Day => "daily",
            Self::Week => "weekly",
            Self::Month => "monthly",
            Self::Quarter => "quarterly",
            Self::Year => "yearly",
        }
    }
    /// The next coarser step, if any.
    pub fn coarser(self) -> Option<Self> {
        match self {
            Self::Day => Some(Self::Week),
            Self::Week => Some(Self::Month),
            Self::Month => Some(Self::Quarter),
            Self::Quarter => Some(Self::Year),
            Self::Year => None,
        }
    }
}

pub const DEFAULT_STEP: Step = Step::Week;

/// The colours an item can wear: the board's palette less white.
pub const ITEM_COLORS: [&str; 6] = ["yellow", "pink", "blue", "green", "purple", "gray"];

/// What an item is by its shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Note,
    Bubble,
    Section,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Bubble => "bubble",
            Self::Section => "section",
        }
    }
}

/// A step from what a person or an agent wrote: `week`, `weekly`, `Weeks`…
pub fn read_step(v: &str) -> Option<Step> {
    Some(match js_trim(v).to_lowercase().as_str() {
        "d" | "day" | "days" | "daily" => Step::Day,
        "w" | "wk" | "week" | "weeks" | "weekly" => Step::Week,
        "m" | "mo" | "month" | "months" | "monthly" => Step::Month,
        "q" | "quarter" | "quarters" | "quarterly" => Step::Quarter,
        "y" | "yr" | "year" | "years" | "yearly" | "annual" | "annually" => Step::Year,
        _ => return None,
    })
}

/// An item colour from what was written (`Blue`, `grey`).
pub fn read_item_color(v: &str) -> Option<&'static str> {
    let c = js_trim(v).to_lowercase();
    let c = if c == "grey" { "gray" } else { c.as_str() };
    ITEM_COLORS.iter().copied().find(|k| *k == c)
}

// ---- days -----------------------------------------------------------------------------

/// Days since 1970-01-01 of a proleptic Gregorian date (month 1–12, any day count).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month 1–12, day)` of a day number.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `Date.UTC(y, m, d) / DAY_MS`: month 0-based and overflowing into years, day overflowing
/// into months, a year 0–99 meaning 1900–1999.
pub fn date_utc_days(y: i64, m: i64, d: i64) -> i64 {
    let y = if (0..=99).contains(&y) { y + 1900 } else { y };
    let y = y + m.div_euclid(12);
    let m = m.rem_euclid(12);
    days_from_civil(y, m + 1, 1) + d - 1
}

/// The weekday of a day number, 0 = Sunday.
pub fn weekday(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

static ISO: LazyLock<Regex> = LazyLock::new(|| js_regex(r"^([0-9]{4})-([0-9]{2})-([0-9]{2})$"));

/// Days since 1970-01-01 for an ISO date, or `None` when it is not a real day.
#[allow(clippy::many_single_char_names)]
pub fn day_of(iso: &str) -> Option<i64> {
    let m = ISO.captures(js_trim(iso))?;
    let n = |i: usize| m.get(i).and_then(|g| g.as_str().parse::<i64>().ok());
    let (y, mo, d) = (n(1)?, n(2)?, n(3)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let t = date_utc_days(y, mo - 1, d);
    (civil_from_days(t) == (y, mo, d)).then_some(t)
}

/// The ISO date of a day number.
pub fn iso_of(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    let sign = if y < 0 { "-" } else { "" };
    format!("{sign}{:04}-{m:02}-{d:02}", y.abs())
}

/// A real ISO date, as written (trimmed).
pub fn clean_iso(v: &str) -> Option<String> {
    day_of(v).map(|_| js_trim(v).to_owned())
}

/// Today (UTC) as an ISO date, from milliseconds since the epoch.
pub fn today_iso(now_ms: i64) -> String {
    iso_of(now_ms.div_euclid(86_400_000))
}

/// `iso` moved by whole months (the day clamped to the month's length).
pub fn add_months(iso: &str, months: i64) -> String {
    let Some(m) = ISO.captures(iso) else {
        return iso.to_owned();
    };
    let n = |i: usize| {
        m.get(i)
            .and_then(|g| g.as_str().parse::<i64>().ok())
            .unwrap_or(0)
    };
    let (y, mo0, d) = (n(1), n(2) - 1 + months, n(3));
    let ty = y + mo0.div_euclid(12);
    let tm = mo0.rem_euclid(12);
    let last = civil_from_days(date_utc_days(ty, tm + 1, 0)).2;
    iso_of(date_utc_days(ty, tm, d.min(last)))
}

pub fn add_days(iso: &str, days: i64) -> String {
    day_of(iso).map_or_else(|| iso.to_owned(), |d| iso_of(d + days))
}

// ---- reading -------------------------------------------------------------------------

const DATE: &str = r"([0-9]{4}-[0-9]{2}-[0-9]{2})";
const ARROW: &str = r"\s*(?:→|->|–|—|\.\.)\s*";

static HEADING: LazyLock<Regex> = LazyLock::new(|| js_regex(r"^(#{1,3})(\s+)(%.*?)\s*#*\s*$"));
static CLOSING: LazyLock<Regex> = LazyLock::new(|| js_regex(r"\s+#+\s*$"));
/// `2026-10-01 → 2026-12-31 · weekly` (the step optional).
static RANGE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    js_regex(&format!(
        r"^\s*{DATE}{ARROW}{DATE}(?:\s*·\s*([A-Za-z]+))?\s*$"
    ))
});
/// `- 2026-10-14 · Kickoff`, `* 2026-10-20 → 2026-11-03 · Sprint · blue`.
static ITEM_LINE: LazyLock<Regex> =
    LazyLock::new(|| js_regex(&format!(r"^\s*[-*]\s+{DATE}(?:{ARROW}{DATE})?(%.*)$")));
/// What follows the dates: ` · label · colour`, `: label`, ` - label` or just ` label`.
static ITEM_REST: LazyLock<Regex> =
    LazyLock::new(|| js_regex(r"^(?:\s*[·:]\s*|\s+-\s+|\s+)(%.*)$"));
/// A bubble's body line under its item.
static BODY_LINE: LazyLock<Regex> = LazyLock::new(|| js_regex(r"^[ \t]*> ?(%.*)$"));
static SEP: LazyLock<Regex> = LazyLock::new(|| js_regex(r"\s+·\s+"));
static LETTERS: LazyLock<Regex> = LazyLock::new(|| js_regex(r"^[A-Za-z]+$"));
static DATED_REF: LazyLock<Regex> = LazyLock::new(|| {
    js_regex(&format!(
        r"^{DATE}(?:\s*(?:→|->)\s*{DATE})?(?:\s*·\s*(%.*))?$"
    ))
});
/// How the page writes a body line (before its words).
const BODY_LEAD: &str = "  >";

/// One item line and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextItem {
    pub index: usize,
    pub date: String,
    pub end: Option<String>,
    pub label: String,
    pub color: Option<&'static str>,
    /// A bubble's body (quote lines joined by `\n`); `None` when it has none.
    pub body: Option<String>,
    pub kind: ItemKind,
    pub line_start: usize,
    pub line_end: usize,
    pub block_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRange {
    pub start: String,
    pub end: String,
    pub step: Step,
    pub step_given: bool,
    pub line_start: usize,
    pub line_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heading {
    pub start: usize,
    pub end: usize,
    pub line_end: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedTimeline {
    pub title: String,
    pub heading: Option<Heading>,
    pub range: Option<TextRange>,
    pub items: Vec<TextItem>,
}

/// A body as it is kept: `\n` line breaks.
pub fn body_lines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn kind_of(end: Option<&str>, body: Option<&str>) -> ItemKind {
    if end.is_some_and(|e| !e.is_empty()) {
        ItemKind::Section
    } else if body.is_some() {
        ItemKind::Bubble
    } else {
        ItemKind::Note
    }
}

/// Splits what follows an item's dates into its label and colour.
fn label_and_color(rest: &str) -> (String, Option<&'static str>) {
    let body = match ITEM_REST.captures(rest) {
        Some(m) => js_trim(m.get(1).map_or("", |g| g.as_str())),
        None => js_trim(rest),
    };
    if body.is_empty() {
        return (String::new(), None);
    }
    let parts: Vec<&str> = SEP.split(body).collect();
    let last = parts.last().copied().unwrap_or_default();
    let color = read_item_color(last);
    if color.is_some() && parts.len() > 1 {
        return (
            js_trim(&parts[..parts.len() - 1].join(" · ")).to_owned(),
            color,
        );
    }
    if color.is_some() && parts.len() == 1 && LETTERS.is_match(js_trim(last)) {
        return (String::new(), color);
    }
    (body.to_owned(), None)
}

/// Reads the title, the range and the items out of a note's text. Never fails.
pub fn parse_timeline_text(text: &str) -> ParsedTimeline {
    let mut out = ParsedTimeline::default();
    let mut at = 0usize;
    let mut open: Option<(usize, Vec<String>)> = None;
    for line in text.split('\n') {
        let start = at;
        let end = start + line.len();
        at = end + 1;
        if let Some((i, lines)) = open.as_mut() {
            if let Some(b) = BODY_LINE.captures(line) {
                lines.push(b.get(1).map_or("", |g| g.as_str()).to_owned());
                let item = &mut out.items[*i];
                item.body = Some(lines.join("\n"));
                item.kind = kind_of(item.end.as_deref(), item.body.as_deref());
                item.block_end = end;
                continue;
            }
            open = None;
        }
        if let Some(m) = ITEM_LINE.captures(line) {
            let a = day_of(m.get(1).map_or("", |g| g.as_str()));
            let second = m.get(2);
            let b = second.and_then(|g| day_of(g.as_str()));
            if let Some(a) = a.filter(|_| second.is_none() || b.is_some()) {
                let (label, color) = label_and_color(m.get(3).map_or("", |g| g.as_str()));
                let lo = match b {
                    Some(b) if b < a => b,
                    _ => a,
                };
                let hi = b.map(|b| a.max(b));
                out.items.push(TextItem {
                    index: out.items.len(),
                    date: iso_of(lo),
                    end: hi.map(iso_of),
                    label,
                    color,
                    body: None,
                    kind: if hi.is_some() {
                        ItemKind::Section
                    } else {
                        ItemKind::Note
                    },
                    line_start: start,
                    line_end: end,
                    block_end: end,
                });
                open = Some((out.items.len() - 1, Vec::new()));
                continue;
            }
        }
        if out.range.is_none() {
            if let Some(r) = RANGE_LINE.captures(line) {
                let a = day_of(r.get(1).map_or("", |g| g.as_str()));
                let b = day_of(r.get(2).map_or("", |g| g.as_str()));
                if let (Some(a), Some(b)) = (a, b) {
                    let step = r.get(3).and_then(|g| read_step(g.as_str()));
                    out.range = Some(TextRange {
                        start: iso_of(a.min(b)),
                        end: iso_of(a.max(b)),
                        step: step.unwrap_or(DEFAULT_STEP),
                        step_given: step.is_some(),
                        line_start: start,
                        line_end: end,
                    });
                    continue;
                }
            }
        }
        if out.heading.is_none() {
            if let Some(h) = HEADING.captures(line) {
                let lead = h.get(1).map_or(0, |g| g.len()) + h.get(2).map_or(0, |g| g.len());
                let rest = &line[lead..];
                let raw = CLOSING
                    .find(rest)
                    .map_or(rest, |c| &rest[..rest.len() - c.as_str().len()]);
                h.get(3)
                    .map_or("", |g| g.as_str())
                    .clone_into(&mut out.title);
                out.heading = Some(Heading {
                    start: start + lead,
                    end: start + lead + raw.len(),
                    line_end: end,
                });
            }
        }
    }
    out
}

// ---- writing ------------------------------------------------------------------------

/// An item as a write gives it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemFields {
    pub date: String,
    pub end: Option<String>,
    pub label: String,
    pub color: Option<&'static str>,
    /// `None`: no body (a note, a plain section); `Some("")`: an empty bubble.
    pub body: Option<String>,
}

impl From<&TextItem> for ItemFields {
    fn from(it: &TextItem) -> Self {
        Self {
            date: it.date.clone(),
            end: it.end.clone(),
            label: it.label.clone(),
            color: it.color,
            body: it.body.clone(),
        }
    }
}

pub fn range_line(start: &str, end: &str, step: Step) -> String {
    format!("{start} → {end} · {}", step.word())
}

/// An item's line as the page writes it.
pub fn item_line(it: &ItemFields) -> String {
    let mut s = format!("- {}", it.date);
    if let Some(end) = it.end.as_ref().filter(|e| !e.is_empty()) {
        s.push_str(" → ");
        s.push_str(end);
    }
    let label = one_line(&it.label);
    let label = js_trim(&label);
    if !label.is_empty() {
        s.push_str(" · ");
        s.push_str(label);
    }
    if let Some(c) = it.color {
        s.push_str(" · ");
        s.push_str(c);
    }
    s
}

/// A body's lines as they follow its item's line; none for `None`.
pub fn body_block(body: Option<&str>) -> String {
    let Some(body) = body else {
        return String::new();
    };
    body_lines(body)
        .split('\n')
        .map(|l| {
            if l.is_empty() {
                format!("\n{BODY_LEAD}")
            } else {
                format!("\n{BODY_LEAD} {l}")
            }
        })
        .collect()
}

pub fn item_block(it: &ItemFields) -> String {
    item_line(it) + &body_block(it.body.as_deref())
}

fn by_date(items: &[ItemFields]) -> Vec<&ItemFields> {
    let mut v: Vec<&ItemFields> = items.iter().collect();
    v.sort_by(|a, b| a.date.cmp(&b.date));
    v
}

/// A whole timeline note: the title, the range line, a blank line, then the items in date
/// order.
pub fn timeline_markdown(
    title: &str,
    start: &str,
    end: &str,
    step: Step,
    items: &[ItemFields],
) -> String {
    let mut head: Vec<String> = Vec::new();
    if !js_trim(title).is_empty() {
        head.push(format!("### {}", js_trim(&one_line(title))));
    }
    head.push(range_line(start, end, step));
    let blocks: Vec<String> = by_date(items).into_iter().map(item_block).collect();
    if blocks.is_empty() {
        head.join("\n")
    } else {
        format!("{}\n\n{}", head.join("\n"), blocks.join("\n"))
    }
}

/// The title: the first heading's words, or a new `###` heading on top.
pub fn write_title<B: TextBuf + ?Sized>(t: &mut B, parsed: &ParsedTimeline, title: &str) {
    let next = one_line(title);
    if let Some(h) = parsed.heading {
        let text = t.text();
        t.splice_within(h.start, &text[h.start..h.end], &next);
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

/// The range line: rewritten in place, or a new one under the title (or on top).
pub fn write_range<B: TextBuf + ?Sized>(
    t: &mut B,
    parsed: &ParsedTimeline,
    start: &str,
    end: &str,
    step: Step,
) {
    let line = range_line(start, end, step);
    if let Some(r) = &parsed.range {
        let text = t.text();
        t.splice_within(r.line_start, &text[r.line_start..r.line_end], &line);
        return;
    }
    if let Some(h) = parsed.heading {
        t.insert(h.line_end, &format!("\n{line}"));
        return;
    }
    let empty = t.text().is_empty();
    t.insert(0, &if empty { line } else { format!("{line}\n") });
}

/// One item's line and body as `next` says: its body lines first, then the line.
fn write_item_in_place<B: TextBuf + ?Sized>(t: &mut B, item: &TextItem, next: &ItemFields) {
    let text = t.text();
    let prev_body = &text[item.line_end..item.block_end];
    let next_body = body_block(next.body.as_deref());
    if prev_body != next_body {
        if prev_body.is_empty() && item.line_end < text.len() {
            t.insert(item.line_end + 1, &format!("{}\n", &next_body[1..]));
        } else {
            t.splice_within(item.line_end, prev_body, &next_body);
        }
    }
    let prev_line = &text[item.line_start..item.line_end];
    let next_line = item_line(next);
    if prev_line != next_line {
        t.splice_within(item.line_start, prev_line, &next_line);
    }
}

/// Removes an item's line and its body's lines (and one line break with them).
pub fn remove_item_lines<B: TextBuf + ?Sized>(t: &mut B, item: &TextItem) {
    let length = t.text().len();
    let end = item.block_end;
    if end < length {
        t.delete(item.line_start, end - item.line_start + 1);
    } else if item.line_start > 0 {
        t.delete(item.line_start - 1, end - item.line_start + 1);
    } else if end > 0 {
        t.delete(0, end);
    }
}

/// A new item's block where its date puts it; its index among the items.
pub fn insert_item_lines<B: TextBuf + ?Sized>(
    t: &mut B,
    parsed: &ParsedTimeline,
    fields: &ItemFields,
) -> usize {
    let block = item_block(fields);
    let text = t.text();
    let after = parsed.items.iter().rfind(|it| it.date <= fields.date);
    if let Some(after) = after {
        if after.block_end < text.len() {
            t.insert(after.block_end + 1, &format!("{block}\n"));
        } else {
            t.insert(after.block_end, &format!("\n{block}"));
        }
        return after.index + 1;
    }
    if let Some(first) = parsed.items.first() {
        t.insert(first.line_start, &format!("{block}\n"));
        return 0;
    }
    let anchor = parsed
        .range
        .as_ref()
        .map(|r| r.line_end)
        .or(parsed.heading.map(|h| h.line_end));
    let bytes = text.as_bytes();
    if let Some(anchor) = anchor {
        let mut at = anchor;
        while at < bytes.len()
            && bytes[at] == b'\n'
            && (bytes.get(at + 1) == Some(&b'\n') || at + 1 == bytes.len())
        {
            at += 1;
        }
        if at >= bytes.len() {
            let ins = if text.ends_with('\n') {
                if text.ends_with("\n\n") {
                    block
                } else {
                    format!("\n{block}")
                }
            } else {
                format!("\n\n{block}")
            };
            t.insert(text.len(), &ins);
        } else {
            t.insert(anchor, &format!("\n\n{block}"));
        }
        return 0;
    }
    if text.is_empty() {
        t.insert(0, &block);
    } else {
        let ins = if text.ends_with('\n') {
            block
        } else {
            format!("\n{block}")
        };
        t.insert(text.len(), &ins);
    }
    0
}

/// Changes one item: in place while its date keeps it between its neighbours, else lifted
/// out and put back where its new date belongs. Its index afterwards.
pub fn write_item<B: TextBuf + ?Sized>(
    t: &mut B,
    parsed: &ParsedTimeline,
    item: &TextItem,
    next: &ItemFields,
) -> usize {
    let prev = item.index.checked_sub(1).and_then(|i| parsed.items.get(i));
    let after = parsed.items.get(item.index + 1);
    let fits =
        prev.is_none_or(|p| p.date <= next.date) && after.is_none_or(|a| next.date <= a.date);
    if fits {
        write_item_in_place(t, item, next);
        return item.index;
    }
    remove_item_lines(t, item);
    let reparsed = parse_timeline_text(&t.text());
    insert_item_lines(t, &reparsed, next)
}

/// Replaces every item by `next`, in date order, where the items were (or under the range
/// line, a blank line between). Lines that are not items stay. One splice of what differs.
pub fn replace_items<B: TextBuf + ?Sized>(t: &mut B, parsed: &ParsedTimeline, next: &[ItemFields]) {
    let before = t.text();
    let lines: Vec<&str> = before.split('\n').collect();
    let mut starts = Vec::with_capacity(lines.len());
    let mut at = 0usize;
    for l in &lines {
        starts.push(at);
        at += l.len() + 1;
    }
    let line_at = |offset: usize| -> usize {
        // The last line starting at or before `offset`.
        starts.partition_point(|s| *s <= offset).saturating_sub(1)
    };
    let mut is_item = vec![false; lines.len()];
    for it in &parsed.items {
        for flag in is_item
            .iter_mut()
            .take(line_at(it.block_end) + 1)
            .skip(line_at(it.line_start))
        {
            *flag = true;
        }
    }
    let blocks: Vec<String> = by_date(next).into_iter().map(item_block).collect();
    let mut insert_at = parsed.items.first().map(|f| line_at(f.line_start));
    let mut lead: Vec<String> = Vec::new();
    if insert_at.is_none() {
        let anchor = parsed
            .range
            .as_ref()
            .map(|r| line_at(r.line_start))
            .or(parsed.heading.map(|h| line_at(h.line_end)));
        insert_at = Some(match anchor {
            None => lines.len(),
            Some(a) => {
                let mut i = a + 1;
                if !blocks.is_empty() && lines.get(i).is_some_and(|l| js_trim(l).is_empty()) {
                    i += 1;
                } else if !blocks.is_empty() {
                    lead.push(String::new());
                }
                i
            }
        });
    }
    let insert_at = insert_at.unwrap_or(lines.len());
    let mut out: Vec<&str> = Vec::new();
    for i in 0..=lines.len() {
        if i == insert_at && !blocks.is_empty() {
            out.extend(lead.iter().map(String::as_str));
            out.extend(blocks.iter().map(String::as_str));
        }
        if i < lines.len() && !is_item[i] {
            out.push(lines[i]);
        }
    }
    if blocks.is_empty() && !parsed.items.is_empty() && is_item.last().copied().unwrap_or(false) {
        while out.len() > 1 && out.last() == Some(&"") {
            out.pop();
        }
    }
    while out.len() > 1 && out[0].is_empty() && before.is_empty() {
        out.remove(0);
    }
    let after = out.join("\n");
    t.splice_within(0, &before, &after);
}

/// The item an agent names: `"YYYY-MM-DD · label"`, a label (any case), or a date (its
/// first item); digits are an index.
pub fn resolve_item<'a>(parsed: &'a ParsedTimeline, reference: &str) -> Option<&'a TextItem> {
    let r = js_trim(reference);
    if !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()) {
        return r.parse::<usize>().ok().and_then(|i| parsed.items.get(i));
    }
    let low = r.to_lowercase();
    if let Some(m) = DATED_REF.captures(r) {
        let date = m.get(1).map_or("", |g| g.as_str());
        let end = m.get(2).map(|g| g.as_str());
        let label = m.get(3).map(|g| js_trim(g.as_str()).to_lowercase());
        let hit = parsed.items.iter().find(|it| {
            it.date == date
                && end.is_none_or(|e| it.end.as_deref() == Some(e))
                && label
                    .as_ref()
                    .is_none_or(|l| js_trim(&it.label).to_lowercase() == *l)
        });
        if hit.is_some() {
            return hit;
        }
    }
    parsed
        .items
        .iter()
        .find(|it| js_trim(&it.label).to_lowercase() == low)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days() {
        assert_eq!(day_of("1970-01-01"), Some(0));
        assert_eq!(day_of(" 2026-10-14 "), Some(20_740));
        assert_eq!(day_of("2026-02-29"), None);
        assert_eq!(day_of("2024-02-29"), Some(19_782));
        assert_eq!(day_of("0099-01-01"), None);
        assert_eq!(
            day_of("0100-01-01").map(iso_of).as_deref(),
            Some("0100-01-01")
        );
        assert_eq!(iso_of(20_740), "2026-10-14");
        assert_eq!(add_months("2026-01-31", 1), "2026-02-28");
        assert_eq!(add_months("2026-11-30", 3), "2027-02-28");
        assert_eq!(add_months("2026-03-15", -3), "2025-12-15");
        assert_eq!(add_days("2026-12-31", 1), "2027-01-01");
        assert_eq!(today_iso(0), "1970-01-01");
        assert_eq!(weekday(0), 4);
        assert_eq!(read_step(" Weeks "), Some(Step::Week));
        assert_eq!(read_item_color("Grey"), Some("gray"));
    }

    #[test]
    fn parses() {
        let text = "### Q4 launch ##\n2026-10-01 → 2026-12-31 · monthly\n\n- 2026-10-14 · Kickoff\n- 2026-11-03 -> 2026-10-20 · Build · sprint · Blue\n- 2026-11-18: Review\n  > Body\n  >\n> more\n- 2026-11-20 · purple\nprose";
        let p = parse_timeline_text(text);
        assert_eq!(p.title, "Q4 launch");
        let h = p.heading.unwrap();
        assert_eq!(&text[h.start..h.end], "Q4 launch");
        let r = p.range.as_ref().unwrap();
        assert_eq!(
            (r.start.as_str(), r.end.as_str(), r.step),
            ("2026-10-01", "2026-12-31", Step::Month)
        );
        assert_eq!(p.items.len(), 4);
        assert_eq!(p.items[1].date, "2026-10-20");
        assert_eq!(p.items[1].end.as_deref(), Some("2026-11-03"));
        assert_eq!(p.items[1].label, "Build · sprint");
        assert_eq!(p.items[1].color, Some("blue"));
        assert_eq!(p.items[2].body.as_deref(), Some("Body\n\nmore"));
        assert_eq!(p.items[2].kind, ItemKind::Bubble);
        assert_eq!(
            (p.items[3].label.as_str(), p.items[3].color),
            ("", Some("purple"))
        );
        assert_eq!(resolve_item(&p, "2026-11-18 · review").unwrap().index, 2);
        assert_eq!(resolve_item(&p, "KICKOFF").unwrap().index, 0);
        assert_eq!(resolve_item(&p, "1").unwrap().index, 1);
        assert!(resolve_item(&p, "9").is_none());
    }

    #[test]
    fn writes() {
        let f = |date: &str, label: &str| ItemFields {
            date: date.into(),
            label: label.into(),
            ..ItemFields::default()
        };
        let mut t = timeline_markdown(
            "Plan",
            "2026-10-01",
            "2026-12-31",
            Step::Week,
            &[f("2026-11-01", "B"), f("2026-10-05", "A")],
        );
        assert_eq!(
            t,
            "### Plan\n2026-10-01 → 2026-12-31 · weekly\n\n- 2026-10-05 · A\n- 2026-11-01 · B"
        );
        let p = parse_timeline_text(&t);
        insert_item_lines(&mut t, &p, &f("2026-10-20", "Mid"));
        assert!(
            t.ends_with("- 2026-10-05 · A\n- 2026-10-20 · Mid\n- 2026-11-01 · B"),
            "{t}"
        );
        let p = parse_timeline_text(&t);
        let it = p.items[0].clone();
        let mut next = ItemFields::from(&it);
        next.date = "2026-12-01".into();
        assert_eq!(write_item(&mut t, &p, &it, &next), 2);
        assert!(t.ends_with("- 2026-11-01 · B\n- 2026-12-01 · A"), "{t}");
        let p = parse_timeline_text(&t);
        replace_items(&mut t, &p, &[]);
        assert_eq!(t, "### Plan\n2026-10-01 → 2026-12-31 · weekly");
        let p = parse_timeline_text(&t);
        replace_items(&mut t, &p, &[f("2026-10-02", "X")]);
        assert_eq!(
            t,
            "### Plan\n2026-10-01 → 2026-12-31 · weekly\n\n- 2026-10-02 · X"
        );
        let mut e = String::new();
        let p = parse_timeline_text(&e);
        write_range(&mut e, &p, "2026-01-01", "2026-02-01", Step::Day);
        let p = parse_timeline_text(&e);
        write_title(&mut e, &p, "T");
        assert_eq!(e, "### T\n2026-01-01 → 2026-02-01 · daily");
    }
}
