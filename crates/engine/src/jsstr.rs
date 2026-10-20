//! JavaScript string semantics the canvas page relies on (its `\s`, `.`, `trim`,
//! `toLowerCase`), so text the server writes matches what the page would write, and the
//! single-splice edit both use on a `Y.Text`.
//!
//! Offsets here are byte offsets into Rust strings; the server's documents use `yrs`'s
//! default byte offsets, so they apply to a `Y.Text` unchanged. Logic ported from the page
//! keeps its arithmetic, measured in bytes instead of UTF-16 units, which gives the same
//! text.

use regex::Regex;
use yrs::{GetString, Text, TextRef, TransactionMut};

/// JavaScript's `\s` as a regex class (`WhiteSpace` and `LineTerminator`).
pub const WS: &str = r"[\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]";
/// JavaScript's `\S`.
pub const NWS: &str = r"[^\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]";
/// JavaScript's `.` without the `s` flag.
pub const DOT: &str = r"[^\n\r\x{2028}\x{2029}]";

/// Compiles a pattern written with JavaScript's meaning of `\s`, `\S` and `%.` (the
/// page's `.`; a plain `.` is never used for "any character" in ported patterns). Digits
/// are always spelled `[0-9]` (Rust's `\d` is Unicode).
///
/// # Panics
///
/// Only on a bad pattern literal (a programming error caught by the unit tests).
#[allow(clippy::expect_used)]
pub fn js_regex(pattern: &str) -> Regex {
    let p = pattern
        .replace(r"\s", WS)
        .replace(r"\S", NWS)
        .replace("%.", DOT);
    Regex::new(&p).expect("valid regex")
}

/// JavaScript's whitespace (what `trim` strips and `\s` matches).
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `s.trim()`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// `s.trimEnd()`.
pub fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_space)
}

/// `s.trimStart()`.
pub fn js_trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_space)
}

/// `s.trim().toLowerCase()`.
pub fn fold(s: &str) -> String {
    js_trim(s).to_lowercase()
}

/// `s.replace(/\s+/g, ' ')`.
pub fn collapse_space(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if is_js_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `JSON.stringify(s)`, for messages.
pub fn quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// Byte offsets of the smallest single splice turning `prev` into `next`: the start, how
/// many bytes of `prev` go and what goes in their place.
pub fn text_splice<'a>(prev: &str, next: &'a str) -> (usize, usize, &'a str) {
    let start = prev
        .char_indices()
        .zip(next.chars())
        .find(|((_, a), b)| a != b)
        .map_or_else(|| prev.len().min(next.len()), |((i, _), _)| i);
    // The common prefix ends on a char boundary of both strings.
    let start = if prev.is_char_boundary(start) && next.is_char_boundary(start) {
        start
    } else {
        0
    };
    let (pt, nt) = (&prev[start..], &next[start..]);
    let suffix: usize = pt
        .chars()
        .rev()
        .zip(nt.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let suffix = suffix.min(pt.len()).min(nt.len());
    (start, pt.len() - suffix, &nt[..nt.len() - suffix])
}

/// A text being edited: a `Y.Text` in a transaction, or a plain string for a dry run.
pub trait TextBuf {
    /// The whole text as it is now.
    fn text(&self) -> String;
    /// Inserts `s` at byte offset `at`.
    fn insert(&mut self, at: usize, s: &str);
    /// Removes `len` bytes from byte offset `at`.
    fn delete(&mut self, at: usize, len: usize);

    /// Removes then inserts at `index`.
    fn splice(&mut self, index: usize, remove: usize, insert: &str) {
        if remove > 0 {
            self.delete(index, remove);
        }
        if !insert.is_empty() {
            self.insert(index, insert);
        }
    }

    /// Replaces `prev` (at `index`) by `next`, touching only the characters that differ.
    fn splice_within(&mut self, index: usize, prev: &str, next: &str) {
        let (at, remove, insert) = text_splice(prev, next);
        self.splice(index + at, remove, insert);
    }

    /// Replaces the whole text by `next` with one minimal splice.
    fn set_all(&mut self, next: &str) {
        let prev = self.text();
        self.splice_within(0, &prev, next);
    }
}

impl TextBuf for String {
    fn text(&self) -> String {
        self.clone()
    }
    fn insert(&mut self, at: usize, s: &str) {
        self.insert_str(at, s);
    }
    fn delete(&mut self, at: usize, len: usize) {
        self.replace_range(at..at + len, "");
    }
}

/// A `Y.Text` edited inside a transaction.
pub struct YText<'a, 'doc> {
    pub txn: &'a mut TransactionMut<'doc>,
    pub t: &'a TextRef,
}

fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl TextBuf for YText<'_, '_> {
    fn text(&self) -> String {
        self.t.get_string(self.txn)
    }
    fn insert(&mut self, at: usize, s: &str) {
        self.t.insert(self.txn, u32_of(at), s);
    }
    fn delete(&mut self, at: usize, len: usize) {
        self.t.remove_range(self.txn, u32_of(at), u32_of(len));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_space() {
        assert_eq!(js_trim("\u{FEFF} a\u{A0}\n"), "a");
        assert_eq!(js_trim("\u{85}a"), "\u{85}a", "NEL is not JS whitespace");
        assert_eq!(collapse_space("a \t\n b  c"), "a b c");
        let re = js_regex(r"^\s*(%.*?)\s*$");
        assert_eq!(&re.captures("  x y \u{2028}").unwrap()[1], "x y");
        assert!(re.captures("a\nb").is_none());
    }

    #[test]
    fn splices() {
        assert_eq!(text_splice("abc", "abXc"), (2, 0, "X"));
        assert_eq!(text_splice("aéc", "aüc"), (1, 2, "ü"));
        let mut s = String::from("hello world");
        s.splice_within(6, "world", "there");
        assert_eq!(s, "hello there");
        s.set_all("hi there");
        assert_eq!(s, "hi there");
    }
}
