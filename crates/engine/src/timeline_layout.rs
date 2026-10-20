//! How tall a timeline card's content is — the part of the page's
//! `canvas/timeline-layout.ts` the server needs: `layoutTimeline(…).needed`, measured with
//! the page's `estimateWidth` (what the page measures with when it has no browser).
//!
//! The arithmetic keeps the page's order of operations, so the same text at the same width
//! needs the same height to the pixel.

#![allow(clippy::cast_precision_loss)]

use std::sync::LazyLock;

use regex::Regex;

use crate::jsstr::{is_js_space, js_regex, js_trim};
use crate::timeline_text::{civil_from_days, date_utc_days, day_of, ItemKind, Step};

mod tl {
    pub const HEAD: f64 = 62.0;
    pub const PAD_X: f64 = 24.0;
    pub const PAD_TOP: f64 = 22.0;
    pub const MIN_ABOVE: f64 = 30.0;
    pub const ABOVE_MAX: f64 = 216.0;
    pub const STEM: f64 = 16.0;
    pub const TIER_GAP: f64 = 8.0;
    pub const BELOW_GAP: f64 = 4.0;
    pub const BOX_GAP: f64 = 8.0;
    pub const EDGE: f64 = 8.0;
    pub const STEM_INSET: f64 = 9.0;
    pub const STEM_CLEAR: f64 = 5.0;
    pub const LABEL_GAP: f64 = 9.0;
    pub const LABEL_H: f64 = 14.0;
    pub const SUB_H: f64 = 13.0;
    pub const LANES_GAP: f64 = 10.0;
    pub const LANE: f64 = 24.0;
    pub const LANE_GAP: f64 = 4.0;
    pub const PAD_BOTTOM: f64 = 16.0;
    pub const CHIP_H: f64 = 24.0;
    pub const CHIP_PAD_X: f64 = 10.0;
    pub const CHIP_MAX: f64 = 184.0;
    pub const BUBBLE_MIN: f64 = 132.0;
    pub const BUBBLE_MAX: f64 = 240.0;
    pub const BUBBLE_PAD_X: f64 = 11.0;
    pub const BUBBLE_PAD_Y: f64 = 8.0;
    pub const BUBBLE_TITLE: f64 = 18.0;
    pub const BUBBLE_LINE: f64 = 17.0;
    pub const BUBBLE_LINES: usize = 4;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Tick,
    TickStrong,
    Chip,
    BubbleTitle,
    BubbleBody,
}

impl Font {
    fn px(self) -> f64 {
        match self {
            Self::Tick | Self::TickStrong => 11.0,
            Self::Chip => 12.0,
            Self::BubbleTitle | Self::BubbleBody => 12.5,
        }
    }
    fn bold(self) -> f64 {
        match self {
            Self::Tick | Self::Chip | Self::BubbleBody => 1.0,
            Self::TickStrong => 1.05,
            Self::BubbleTitle => 1.06,
        }
    }
}

/// The page's `estimateWidth`: per-character widths of a system sans.
pub fn estimate_width(text: &str, font: Font) -> f64 {
    let mut em = 0.0;
    for c in text.chars() {
        em += match c {
            ' ' | 'i' | 'l' | '.' | ',' | ':' | ';' | '\'' | '|' | '!' | '·' => 0.28,
            'f' | 'r' | 't' | 'j' | 'I' | '(' | ')' | '[' | ']' | '-' => 0.38,
            'm' | 'w' | 'M' | 'W' | '@' => 0.86,
            '0'..='9' => 0.6,
            'A'..='Z' => 0.68,
            // `c.charCodeAt(0)`: a character outside the BMP counts by its high surrogate.
            _ if first_unit(c) > 0x2E80 => 1.0,
            _ => 0.55,
        };
    }
    em * font.px() * font.bold()
}

fn first_unit(c: char) -> u32 {
    let mut buf = [0u16; 2];
    u32::from(c.encode_utf16(&mut buf)[0])
}

const STRIDES: [usize; 16] = [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256];
const MAX_TICKS: usize = 1200;

struct Parts {
    y: i64,
    m: i64,
    d: i64,
}

fn parts(day: i64) -> Parts {
    let (y, m, d) = civil_from_days(day);
    Parts { y, m: m - 1, d }
}

fn step_days(step: Step) -> f64 {
    match step {
        Step::Day => 1.0,
        Step::Week => 7.0,
        Step::Month => 30.4,
        Step::Quarter => 91.3,
        Step::Year => 365.25,
    }
}

/// Step ticks: every day or week from the start; the first of each month, quarter or year.
pub fn step_ticks(start: i64, end: i64, step: Step) -> Vec<i64> {
    let mut out = Vec::new();
    if matches!(step, Step::Day | Step::Week) {
        let by = if step == Step::Day { 1 } else { 7 };
        let mut d = start;
        while d <= end && out.len() < MAX_TICKS * 4 {
            out.push(d);
            d += by;
        }
        return out;
    }
    let months = match step {
        Step::Month => 1,
        Step::Quarter => 3,
        _ => 12,
    };
    let p = parts(start);
    let (mut y, mut m) = (p.y, p.m);
    if p.d != 1 || m % months != 0 {
        m = m.div_euclid(months) * months + months;
        y += m.div_euclid(12);
        m %= 12;
    }
    let mut d = date_utc_days(y, m, 1);
    while d <= end && out.len() < MAX_TICKS * 4 {
        out.push(d);
        m += months;
        y += m.div_euclid(12);
        m %= 12;
        d = date_utc_days(y, m, 1);
    }
    out
}

/// One item as the layout sees it.
#[derive(Debug, Clone)]
pub struct LayoutItem {
    pub index: usize,
    pub date: String,
    pub end: Option<String>,
    pub label: String,
    pub body: Option<String>,
    pub kind: ItemKind,
}

#[derive(Debug, Clone)]
struct Sized {
    index: usize,
    x: f64,
    w: f64,
    h: f64,
}

#[derive(Debug, Clone)]
struct Placed {
    s: Sized,
    tier: i64,
    bx: f64,
}

static LIST_LEAD: LazyLock<Regex> =
    LazyLock::new(|| js_regex(r"^\s*(?:[-*+]|[0-9]+[.)])\s+(?:\[[ xX]\]\s+)?"));
static HEAD_LEAD: LazyLock<Regex> = LazyLock::new(|| js_regex(r"^\s*#{1,3}\s+"));
static QUOTE_LEAD: LazyLock<Regex> = LazyLock::new(|| js_regex(r"^\s*>\s?"));
static WORD_LEAD: LazyLock<Regex> = LazyLock::new(|| js_regex(r"^\s*(?:#{1,3}|>)\s?"));
static LINK: LazyLock<Regex> = LazyLock::new(|| js_regex(r"\[([^\]]*)\]\([^)]*\)"));
static MARKS: LazyLock<Regex> = LazyLock::new(|| js_regex(r"(\*\*|__|\*|_|~~|`)"));
static BOLD_MARK: LazyLock<Regex> = LazyLock::new(|| js_regex(r"\*\*|__"));

/// A body line as it reads: no emphasis marks or link targets; a bullet's indent kept.
fn plain_line(line: &str) -> String {
    let s = LIST_LEAD.replace(line, "    ");
    let s = HEAD_LEAD.replace(&s, "");
    let s = QUOTE_LEAD.replace(&s, "");
    let s = LINK.replace_all(&s, "$1");
    MARKS.replace_all(&s, "").into_owned()
}

/// Rows a body line takes at `width`, wrapped word by word as the page wraps it.
fn wrapped_lines(line: &str, width: f64) -> usize {
    let lead = LIST_LEAD.find(line);
    let indent = if lead.is_some() {
        estimate_width("    ", Font::BubbleBody)
    } else {
        0.0
    };
    let rest = &line[lead.map_or(0, |m| m.end())..];
    let rest = WORD_LEAD.replace(rest, "");
    let words = rest.split(is_js_space).filter(|w| !w.is_empty());
    let space = estimate_width(" ", Font::BubbleBody);
    let mut rows = 1usize;
    let mut at = indent;
    let mut bold = false;
    for word in words {
        let marks = BOLD_MARK.find_iter(word).count();
        let shown = LINK.replace_all(word, "$1");
        let shown = MARKS.replace_all(&shown, "");
        let w = estimate_width(
            &shown,
            if bold || marks > 0 {
                Font::BubbleTitle
            } else {
                Font::BubbleBody
            },
        );
        if marks % 2 == 1 {
            bold = !bold;
        }
        let next = if at > indent { at + space + w } else { at + w };
        if next > width && at > indent {
            rows += 1;
            at = indent + w;
            // A word wider than the row breaks where it must (bounded: a width at or under
            // the indent would loop forever).
            let mut guard = 0;
            while at > width && guard < 10_000 {
                rows += 1;
                at -= width - indent;
                guard += 1;
            }
        } else {
            at = next;
        }
    }
    rows
}

/// A pin's box size: a chip for a note, a card for a bubble.
fn size_of(item: &LayoutItem, x: f64, card_w: f64) -> Sized {
    let room = 40f64.max(card_w - tl::EDGE * 2.0);
    let label = js_trim(&item.label);
    if item.kind != ItemKind::Bubble {
        let w = if label.is_empty() {
            0.0
        } else {
            tl::CHIP_MAX
                .min(room)
                .min((estimate_width(label, Font::Chip) + tl::CHIP_PAD_X * 2.0 + 2.0).ceil())
        };
        return Sized {
            index: item.index,
            x,
            w,
            h: if label.is_empty() { 0.0 } else { tl::CHIP_H },
        };
    }
    let body = item.body.as_deref().unwrap_or_default();
    let raw: Vec<&str> = body.split('\n').collect();
    let mut widest = if label.is_empty() {
        0.0
    } else {
        estimate_width(label, Font::BubbleTitle)
    };
    for line in &raw {
        widest = f64::max(widest, estimate_width(&plain_line(line), Font::BubbleBody));
    }
    let w = room.min(
        tl::BUBBLE_MIN.max(tl::BUBBLE_MAX.min((widest + tl::BUBBLE_PAD_X * 2.0 + 6.0).ceil())),
    );
    let inner = w - tl::BUBBLE_PAD_X * 2.0 - 2.0;
    let mut lines = 0usize;
    for line in &raw {
        if js_trim(&plain_line(line)).is_empty() {
            continue;
        }
        lines += wrapped_lines(line, 20f64.max(inner));
    }
    let shown = lines.min(tl::BUBBLE_LINES);
    let h = tl::BUBBLE_PAD_Y * 2.0
        + if !label.is_empty() || shown == 0 {
            tl::BUBBLE_TITLE
        } else {
            0.0
        }
        + shown as f64 * tl::BUBBLE_LINE;
    Sized {
        index: item.index,
        x,
        w,
        h,
    }
}

/// Allowed positions minus forbidden open intervals → the point nearest `ideal`.
fn nearest_free(lo: f64, hi: f64, forbidden: &[(f64, f64)], ideal: f64) -> Option<f64> {
    if hi < lo {
        return None;
    }
    let mut free: Vec<(f64, f64)> = vec![(lo, hi)];
    for &(a, b) in forbidden {
        let mut next = Vec::with_capacity(free.len() + 1);
        for &(p, q) in &free {
            if b <= p || a >= q {
                next.push((p, q));
                continue;
            }
            if a > p {
                next.push((p, a));
            }
            if b < q {
                next.push((b, q));
            }
        }
        free = next;
        if free.is_empty() {
            return None;
        }
    }
    let mut best: Option<f64> = None;
    for (p, q) in free {
        let c = q.min(p.max(ideal));
        if best.is_none_or(|b| (c - ideal).abs() < (b - ideal).abs()) {
            best = Some(c);
        }
    }
    best
}

/// The most tiers a pack tries before giving up on a clear spot (the page's loops are
/// unbounded; a pathological width could spin).
const TIER_CAP: i64 = 4096;

struct Packer {
    placed: Vec<Placed>,
    card_w: f64,
}

impl Packer {
    fn inset(s: &Sized) -> f64 {
        tl::STEM_INSET.min(s.w / 2.0)
    }
    fn lo(s: &Sized) -> f64 {
        tl::EDGE.max(s.x - s.w + Self::inset(s))
    }
    fn hi(&self, s: &Sized) -> f64 {
        (self.card_w - tl::EDGE - s.w).min(s.x - Self::inset(s))
    }

    /// Where `p` can sit in `tier`, clear of everyone but `skip`.
    fn spot(&self, p: &Sized, tier: i64, skip: Option<usize>, extra_stems: &[f64]) -> Option<f64> {
        let mut forbidden: Vec<(f64, f64)> = Vec::new();
        for (i, q) in self.placed.iter().enumerate() {
            if Some(i) == skip {
                continue;
            }
            if q.tier == tier && q.s.w > 0.0 {
                forbidden.push((q.bx - tl::BOX_GAP - p.w, q.bx + q.s.w + tl::BOX_GAP));
            }
            if q.tier > tier {
                forbidden.push((q.s.x - p.w - tl::STEM_CLEAR, q.s.x + tl::STEM_CLEAR));
            }
        }
        for s in extra_stems {
            forbidden.push((s - p.w - tl::STEM_CLEAR, s + tl::STEM_CLEAR));
        }
        nearest_free(Self::lo(p), self.hi(p), &forbidden, p.x - p.w / 2.0)
    }

    fn blocks(q: &Placed, x: f64) -> bool {
        q.s.w > 0.0 && x > q.bx - tl::STEM_CLEAR && x < q.bx + q.s.w + tl::STEM_CLEAR
    }

    /// The lowest tier where `p` fits and its stem runs clear (nudging boxes in its way).
    fn try_place(&mut self, p: &Sized) -> bool {
        let top = self.placed.iter().map(|q| q.tier).fold(-1, i64::max) + 1;
        for tier in 0..=top {
            let Some(bx) = self.spot(p, tier, None, &[]) else {
                continue;
            };
            let in_way: Vec<usize> = (0..self.placed.len())
                .filter(|&i| {
                    let q = &self.placed[i];
                    q.tier >= 0 && q.tier < tier && Self::blocks(q, p.x)
                })
                .collect();
            let mut moves: Vec<(usize, f64)> = Vec::new();
            let mut clear = true;
            for &i in &in_way {
                let q = self.placed[i].s.clone();
                let qt = self.placed[i].tier;
                if let Some(nx) = self.spot(&q, qt, Some(i), &[p.x]) {
                    moves.push((i, nx));
                } else {
                    clear = false;
                    break;
                }
            }
            if !clear {
                continue;
            }
            for (i, nx) in moves {
                self.placed[i].bx = nx;
            }
            self.placed.push(Placed {
                s: p.clone(),
                tier,
                bx,
            });
            return true;
        }
        false
    }
}

/// Packs pins into tiers, nearest the axis first.
fn pack(list: &[Sized], card_w: f64) -> Vec<Placed> {
    let mut pk = Packer {
        placed: Vec::new(),
        card_w,
    };
    let mut sorted: Vec<&Sized> = list.iter().collect();
    sorted.sort_by(|a, b| {
        (a.x - b.x)
            .partial_cmp(&0.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.index.cmp(&b.index))
    });
    for p in sorted {
        if p.w == 0.0 {
            pk.placed.push(Placed {
                s: p.clone(),
                tier: -1,
                bx: p.x,
            });
            continue;
        }
        if pk.try_place(p) {
            continue;
        }
        // A box in the way could not step aside: let it go up instead, over this one.
        let mut done = false;
        let mut in_way: Vec<Placed> = pk
            .placed
            .iter()
            .filter(|q| q.tier >= 0 && Packer::blocks(q, p.x))
            .cloned()
            .collect();
        in_way.sort_by_key(|q| q.tier);
        for q in in_way {
            let saved = pk.placed.clone();
            let Some(at) = pk.placed.iter().position(|o| {
                o.s.index == q.s.index && o.tier == q.tier && o.bx.to_bits() == q.bx.to_bits()
            }) else {
                continue;
            };
            pk.placed.remove(at);
            let lifted = q.s.clone();
            if pk.try_place(p) && pk.try_place(&lifted) {
                done = true;
                break;
            }
            pk.placed = saved;
        }
        if done {
            continue;
        }
        // No clear way up: the lowest tier with room, the stem behind the box below.
        let mut tier = 0;
        loop {
            let forbidden: Vec<(f64, f64)> = pk
                .placed
                .iter()
                .filter(|q| q.tier == tier && q.s.w > 0.0)
                .map(|q| (q.bx - tl::BOX_GAP - p.w, q.bx + q.s.w + tl::BOX_GAP))
                .collect();
            let bx = nearest_free(Packer::lo(p), pk.hi(p), &forbidden, p.x - p.w / 2.0);
            if let Some(bx) = bx {
                pk.placed.push(Placed {
                    s: p.clone(),
                    tier,
                    bx,
                });
                break;
            }
            if tier >= TIER_CAP {
                // Nowhere at all (a card narrower than the box): one tier over the rest.
                pk.placed.push(Placed {
                    s: p.clone(),
                    tier: pk.placed.iter().map(|q| q.tier).fold(-1, i64::max) + 1,
                    bx: Packer::lo(p),
                });
                break;
            }
            tier += 1;
        }
    }
    pk.placed
}

/// Each tier's height (its tallest box).
fn tier_heights(placed: &[Placed]) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    for p in placed {
        if p.tier < 0 {
            continue;
        }
        let t = usize::try_from(p.tier).unwrap_or(0);
        if out.len() <= t {
            out.resize(t + 1, 0.0);
        }
        out[t] = out[t].max(p.s.h);
    }
    out
}

fn stack_h(heights: &[f64], gap: f64) -> f64 {
    heights.iter().sum::<f64>() + gap * heights.len().saturating_sub(1) as f64
}

struct Tick {
    day: i64,
    x: f64,
    step: bool,
    label: Option<String>,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn month(m: i64) -> &'static str {
    MONTHS[usize::try_from(m.rem_euclid(12)).unwrap_or(0)]
}

/// The least height a card with this content needs at width `w` (the page's
/// `layoutTimeline({…, h: 0}).needed`).
#[allow(clippy::too_many_lines)]
pub fn needed_height(start: &str, end: &str, step: Step, items: &[LayoutItem], w: f64) -> f64 {
    let w = 1f64.max(w);
    let mut start_day = day_of(start).unwrap_or(0);
    let mut end_day = day_of(end).unwrap_or(start_day);
    if end_day < start_day {
        std::mem::swap(&mut start_day, &mut end_day);
    }
    let cells = (end_day - start_day + 1) as f64;
    let x0 = tl::PAD_X;
    let x1 = (x0 + 1.0).max(w - tl::PAD_X);
    let cell_w = (x1 - x0) / cells;
    let x_of = |day: i64| x0 + ((day - start_day) as f64 + 0.5) * cell_w;

    // ---- ticks and their labels (for whether a context row shows) ----
    let mut step = step;
    let mut days = step_ticks(start_day, end_day, step);
    while days.len() as f64 > 8f64.max((x1 - x0) / 3.0) {
        let Some(c) = step.coarser() else { break };
        step = c;
        days = step_ticks(start_day, end_day, step);
    }
    let step_len = step_days(step);
    let mut ticks: Vec<Tick> = days
        .iter()
        .map(|&day| Tick {
            day,
            x: x_of(day),
            step: true,
            label: None,
        })
        .collect();
    let mut edges: Vec<Tick> = Vec::new();
    let near = |ticks: &[Tick], day: i64| {
        ticks
            .iter()
            .any(|t| (((t.day - day).abs()) as f64) < 1f64.max(step_len * 0.4))
    };
    if !ticks.iter().any(|t| t.day == start_day) && !near(&ticks, start_day) {
        edges.push(Tick {
            day: start_day,
            x: x_of(start_day),
            step: false,
            label: None,
        });
    }
    if end_day != start_day && !ticks.iter().any(|t| t.day == end_day) && !near(&ticks, end_day) {
        edges.push(Tick {
            day: end_day,
            x: x_of(end_day),
            step: false,
            label: None,
        });
    }
    let primary = |t: &Tick| -> String {
        let p = parts(t.day);
        if !t.step {
            return format!("{} {}", month(p.m), p.d);
        }
        match step {
            Step::Day => p.d.to_string(),
            Step::Week => format!("{} {}", month(p.m), p.d),
            Step::Month => month(p.m).to_owned(),
            Step::Quarter => format!("Q{}", p.m.div_euclid(3) + 1),
            Step::Year => p.y.to_string(),
        }
    };
    let half = |s: &str, font: Font| estimate_width(s, font) / 2.0;
    let clamp_label = |x: f64, hw: f64| (w - 6.0 - hw).min((6.0 + hw).max(x));
    let fits = |list: &[(f64, f64)]| {
        let mut sorted = list.to_vec();
        sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        sorted
            .windows(2)
            .all(|p| p[1].0 - p[1].1 - (p[0].0 + p[0].1) >= 10.0)
    };
    let texts: Vec<String> = ticks.iter().map(&primary).collect();
    let mut stride = STRIDES[STRIDES.len() - 1];
    for s in STRIDES {
        let list: Vec<(f64, f64)> = ticks
            .iter()
            .enumerate()
            .filter(|(i, _)| i % s == 0)
            .map(|(i, t)| {
                let hw = half(&texts[i], Font::Tick);
                (clamp_label(t.x, hw), hw)
            })
            .collect();
        if fits(&list) {
            stride = s;
            break;
        }
    }
    let mut labelled: Vec<(f64, f64)> = Vec::new();
    for (i, t) in ticks.iter_mut().enumerate() {
        if i % stride != 0 {
            continue;
        }
        let hw = half(&texts[i], Font::Tick);
        t.label = Some(texts[i].clone());
        labelled.push((clamp_label(t.x, hw), hw));
    }
    for e in &mut edges {
        let text = primary(e);
        let hw = half(&text, Font::Tick);
        let at = clamp_label(e.x, hw);
        let mut with = labelled.clone();
        with.push((at, hw));
        if fits(&with) {
            e.label = Some(text);
            labelled.push((at, hw));
        }
    }
    let mut all: Vec<Tick> = ticks.into_iter().chain(edges).collect();
    all.sort_by_key(|t| t.day);
    let crosses = parts(start_day).y != parts(end_day).y;
    let mut prev: Option<i64> = None;
    let mut subs: Vec<(f64, f64)> = Vec::new();
    for t in &all {
        if t.label.is_none() {
            continue;
        }
        let p = parts(t.day);
        let q = prev.map(parts);
        let sub: Option<String> = if step == Step::Day {
            match q {
                None => Some(month(p.m).to_owned()),
                Some(q) if q.y != p.y => Some(format!("{} {}", month(p.m), p.y)),
                Some(q) => (q.m != p.m).then(|| month(p.m).to_owned()),
            }
        } else if step == Step::Year || !crosses {
            None
        } else {
            match q {
                Some(q) if q.y == p.y => None,
                _ => Some(p.y.to_string()),
            }
        };
        prev = Some(t.day);
        let Some(sub) = sub else { continue };
        let hw = half(&sub, Font::TickStrong);
        let at = clamp_label(t.x, hw);
        let mut with = subs.clone();
        with.push((at, hw));
        if !fits(&with) {
            continue;
        }
        subs.push((at, hw));
    }
    let sub_row = !subs.is_empty();

    // ---- sections in lanes ----
    let mut lane_ends: Vec<i64> = Vec::new();
    let mut spans: Vec<(usize, i64, i64)> = items
        .iter()
        .filter(|it| it.kind == ItemKind::Section)
        .map(|it| {
            let a = day_of(&it.date).unwrap_or(0);
            let b = day_of(it.end.as_deref().unwrap_or(&it.date)).unwrap_or(0);
            (it.index, a, b)
        })
        .collect();
    spans.sort_by(|p, q| {
        p.1.cmp(&q.1)
            .then(((q.2 - q.1) - (p.2 - p.1)).cmp(&0).reverse().reverse())
            .then(p.0.cmp(&q.0))
    });
    for &(_, a, b) in &spans {
        if b < start_day || a > end_day {
            continue;
        }
        let ca = a.max(start_day);
        let cb = b.min(end_day);
        let lane = lane_ends
            .iter()
            .position(|e| *e < ca)
            .unwrap_or(lane_ends.len());
        if lane == lane_ends.len() {
            lane_ends.push(cb);
        } else {
            lane_ends[lane] = cb;
        }
    }
    let lanes = lane_ends.len();

    // ---- pins ----
    let mut sized: Vec<Sized> = Vec::new();
    for it in items {
        if it.kind == ItemKind::Section {
            continue;
        }
        match day_of(&it.date) {
            Some(d) if d >= start_day && d <= end_day => sized.push(size_of(it, x_of(d), w)),
            _ => {}
        }
    }

    // ---- heights ----
    let label_block = tl::LABEL_GAP + tl::LABEL_H + if sub_row { tl::SUB_H } else { 0.0 };
    let lane_block = if lanes > 0 {
        tl::LANES_GAP + lanes as f64 * tl::LANE + (lanes - 1) as f64 * tl::LANE_GAP
    } else {
        0.0
    };
    let above = pack(&sized, w);
    let heights = tier_heights(&above);
    let mut k = heights.len().min(1);
    while k < heights.len() && stack_h(&heights[..=k], tl::TIER_GAP) <= tl::ABOVE_MAX {
        k += 1;
    }
    let k_tier = i64::try_from(k).unwrap_or(i64::MAX);
    let rest: Vec<Sized> = above
        .iter()
        .filter(|p| p.tier >= k_tier)
        .map(|p| p.s.clone())
        .collect();
    let down = if rest.is_empty() {
        Vec::new()
    } else {
        pack(&rest, w)
    };
    let down_h = tier_heights(&down);
    let up_px = tl::MIN_ABOVE.max(if k > 0 {
        stack_h(&heights[..k], tl::TIER_GAP) + tl::STEM
    } else {
        0.0
    });
    let down_px = if down_h.is_empty() {
        0.0
    } else {
        tl::BELOW_GAP + tl::STEM + stack_h(&down_h, tl::TIER_GAP)
    };
    tl::HEAD + tl::PAD_TOP + up_px + label_block + lane_block + down_px + tl::PAD_BOTTOM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_and_ticks() {
        assert!(
            (estimate_width("Ab 1", Font::Chip) - (0.68 + 0.55 + 0.28 + 0.6) * 12.0).abs() < 1e-9
        );
        let d = day_of("2026-10-15").unwrap();
        let e = day_of("2027-01-10").unwrap();
        let t = step_ticks(d, e, Step::Month);
        let isos: Vec<String> = t.iter().map(|x| crate::timeline_text::iso_of(*x)).collect();
        assert_eq!(isos, ["2026-11-01", "2026-12-01", "2027-01-01"]);
        assert_eq!(step_ticks(d, d + 13, Step::Week).len(), 2);
    }

    #[test]
    fn empty_card_height() {
        // Header 62 + padTop 22 + minAbove 30 + labels 23 + padBottom 16.
        let h = needed_height("2026-10-01", "2026-12-31", Step::Week, &[], 880.0);
        assert!((h - 153.0).abs() < 1e-9, "{h}");
    }
}
