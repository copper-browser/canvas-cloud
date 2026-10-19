//! The shared canvas Yjs schema (spec §4) and small value helpers.
//!
//! Document layout (identical on the TS page, the Swift host and this server):
//!
//! * `shapes`: `Y.Map<shapeId, Y.Map<prop, any>>` — per-property last-writer-wins.
//! * `agents`: `Y.Map<agentId, {name, color, cursor:{x,y}|null, status, updatedAt}>` (plain
//!   JSON object values).
//! * `meta`: `Y.Map` with `name` and `createdBy`.
//! * `comments`: optional, untouched by the server.

use serde_json::Value;
use yrs::types::ToJson;
use yrs::{Any, Number, Out, ReadTxn};

/// Root map holding every shape.
pub const SHAPES: &str = "shapes";
/// Root map holding agent presence.
pub const AGENTS: &str = "agents";
/// Root map holding canvas metadata.
pub const META: &str = "meta";

/// Every shape type the schema knows.
pub const SHAPE_TYPES: [&str; 7] = [
    "sticky",
    "text",
    "frame",
    "arrow",
    "image",
    "link",
    "checklist",
];

/// A shape type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeType {
    Sticky,
    Text,
    Frame,
    Arrow,
    Image,
    Link,
    /// A clickable RSVP / to-do card (`crate::checklist`).
    Checklist,
}

impl ShapeType {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sticky" => Self::Sticky,
            "text" => Self::Text,
            "frame" => Self::Frame,
            "arrow" => Self::Arrow,
            "image" => Self::Image,
            "link" => Self::Link,
            "checklist" => Self::Checklist,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sticky => "sticky",
            Self::Text => "text",
            Self::Frame => "frame",
            Self::Arrow => "arrow",
            Self::Image => "image",
            Self::Link => "link",
            Self::Checklist => "checklist",
        }
    }

    /// Default size (the page's `SHAPE_SIZE`).
    pub fn default_size(self) -> (f64, f64) {
        match self {
            Self::Sticky => (200.0, 200.0),
            Self::Text => (240.0, 36.0),
            Self::Frame => (480.0, 320.0),
            Self::Arrow => (0.0, 0.0),
            Self::Image => (320.0, 240.0),
            Self::Link => (300.0, 84.0),
            Self::Checklist => (324.0, 120.0),
        }
    }

    /// Smallest size of resizable types (`None` for arrows).
    pub fn min_size(self) -> Option<(f64, f64)> {
        match self {
            Self::Sticky => Some((96.0, 64.0)),
            Self::Text => Some((40.0, 24.0)),
            Self::Frame => Some((160.0, 120.0)),
            Self::Image => Some((32.0, 32.0)),
            Self::Link => Some((180.0, 56.0)),
            Self::Checklist => Some((200.0, 72.0)),
            Self::Arrow => None,
        }
    }

    pub fn default_color(self) -> &'static str {
        match self {
            Self::Sticky => "yellow",
            Self::Image | Self::Link => "white",
            Self::Text | Self::Frame | Self::Arrow => "gray",
            Self::Checklist => "green",
        }
    }

    /// Whether the body text is a `Y.Text` (concurrent typing merges).
    pub fn has_body(self) -> bool {
        matches!(self, Self::Sticky | Self::Text)
    }

    /// Props an op may set on this type.
    pub fn allowed(self) -> &'static [&'static str] {
        match self {
            Self::Sticky => &["x", "y", "w", "h", "color", "z", "text", "fontSize"],
            Self::Text => &[
                "x", "y", "w", "h", "color", "z", "text", "fontSize", "align",
            ],
            Self::Frame => &["x", "y", "w", "h", "color", "z", "title", "image"],
            Self::Arrow => &["color", "z", "from", "to", "label"],
            Self::Image => &[
                "x", "y", "w", "h", "color", "z", "src", "naturalW", "naturalH",
            ],
            Self::Link => &["x", "y", "w", "h", "color", "z", "url", "title", "favicon"],
            Self::Checklist => &[
                "x", "y", "w", "h", "color", "z", "title", "columns", "rows", "picks",
            ],
        }
    }

    /// Friendly spellings: `text` on a frame or link is its title, on an arrow its label.
    pub fn alias(self, key: &str) -> &str {
        match (self, key) {
            (Self::Frame | Self::Link | Self::Checklist, "text" | "label") => "title",
            (Self::Arrow, "text" | "title") => "label",
            (Self::Sticky | Self::Text, "title") => "text",
            _ => key,
        }
    }
}

/// Longest `data:` URL accepted for images (2 MiB, as on the page).
pub const MAX_DATA_URL: usize = 2 * 1024 * 1024;
/// Longest sticky/text body (UTF-16 units).
pub const MAX_TEXT: usize = 20_000;
/// Titles and labels are cut to this many UTF-16 units.
pub const MAX_TITLE: usize = 500;

/// Length in UTF-16 code units (JavaScript `.length`).
pub fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The longest prefix of `s` that is at most `max` UTF-16 units (never splits a char).
pub fn js_slice(s: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &s[..i];
        }
    }
    s
}

/// A stable colour for an agent without one (the page's `colorFor`).
pub fn color_for(seed: &str) -> String {
    let mut hash: i32 = 0;
    for unit in seed.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(i32::from(unit));
    }
    format!("hsl({} 62% 48%)", hash.unsigned_abs() % 360)
}

/// Named palette colours (plus `#hex`).
pub const NAMED_COLORS: [&str; 7] = ["yellow", "pink", "blue", "green", "purple", "gray", "white"];

/// Agent presence statuses.
pub const AGENT_STATUSES: [&str; 3] = ["idle", "thinking", "writing"];

/// True when `s` is a palette colour or a `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` hex colour.
pub fn is_valid_color(s: &str) -> bool {
    if NAMED_COLORS.contains(&s) {
        return true;
    }
    match s.strip_prefix('#') {
        Some(hex) => {
            matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// Converts a JSON value into a Yjs `Any`.
pub fn json_to_any(v: &Value) -> Any {
    match v {
        Value::Null => Any::Null,
        Value::Bool(b) => Any::Bool(*b),
        Value::Number(n) => n
            .as_i64()
            .map(|i| Any::Number(Number::Int(i)))
            .or_else(|| n.as_f64().map(num_any))
            .unwrap_or(Any::Null),
        Value::String(s) => Any::String(s.as_str().into()),
        Value::Array(items) => Any::from(items.iter().map(json_to_any).collect::<Vec<_>>()),
        Value::Object(map) => Any::from(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_any(v)))
                .collect::<std::collections::HashMap<_, _>>(),
        ),
    }
}

/// Converts a Yjs `Any` into JSON.
pub fn any_to_json(a: &Any) -> Value {
    serde_json::to_value(a).unwrap_or(Value::Null)
}

/// Converts any Yjs output value (including nested shared types) into JSON.
pub fn out_to_json<T: ReadTxn>(txn: &T, out: &Out) -> Value {
    match out {
        Out::Any(a) => any_to_json(a),
        other => any_to_json(&other.to_json(txn)),
    }
}

/// A number as a compact Yjs value (integral values are stored as integers).
pub fn num_any(v: f64) -> Any {
    Any::Number(Number::try_i64(v))
}

/// Reads a finite number from a Yjs output value.
pub fn out_f64(out: Option<&Out>) -> Option<f64> {
    match out {
        Some(Out::Any(Any::Number(n))) => n.as_f64().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// Reads a string from a Yjs output value (plain strings and `Y.Text`).
pub fn out_string<T: ReadTxn>(txn: &T, out: Option<&Out>) -> Option<String> {
    match out {
        Some(Out::Any(Any::String(s))) => Some(s.to_string()),
        Some(Out::YText(t)) => {
            use yrs::GetString;
            Some(t.get_string(txn))
        }
        _ => None,
    }
}

/// A JSON number that is an integer when `v` is integral (as JavaScript prints it).
#[allow(clippy::cast_possible_truncation)]
pub fn jnum(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() <= 9_007_199_254_740_991.0 {
        Value::from(v as i64)
    } else {
        Value::from(v)
    }
}

/// `serialize_with` for `f64` fields: integral values print as integers.
pub fn ser_num<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&jnum(*v), s)
}

/// `serialize_with` for `Option<f64>` fields.
#[allow(clippy::ref_option)]
pub fn ser_opt_num<S: serde::Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&v.map(jnum), s)
}

/// Reads a finite number from a JSON value.
pub fn json_f64(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}

/// Current time in milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn colors() {
        for ok in ["yellow", "white", "#fff", "#ffff", "#a0b1c2", "#A0B1C2ff"] {
            assert!(is_valid_color(ok), "{ok}");
        }
        for bad in [
            "", "red", "#", "#ff", "#ggg", "#12345", "yellow ", "#1234567",
        ] {
            assert!(!is_valid_color(bad), "{bad}");
        }
    }

    #[test]
    fn json_roundtrip() {
        let v = json!({"a": 1, "b": 1.5, "c": "x", "d": [true, null], "e": {"f": -3}});
        assert_eq!(any_to_json(&json_to_any(&v)), v);
        assert_eq!(num_any(2.0), Any::Number(Number::Int(2)));
        assert_eq!(num_any(2.5), Any::Number(Number::Float(2.5)));
    }
}
