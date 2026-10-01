//! Canvas ops (spec §4) with the same semantics as the canvas page's `Canvas/src/ops.ts`
//! (`copperCanvas.apply`), applied to a `yrs` document inside one transaction:
//!
//! ```text
//! {op:"add", shape:{type, id?, x?, y?, w?, h?, color?, …}}  // no x/y → free space near `near`
//! {op:"update", id, patch:{…}}                              // merges props (`props` also accepted)
//! {op:"move", id, dx, dy}                                    // a frame carries what sits inside it
//! {op:"resize", id, w, h}
//! {op:"delete", id}                                          // arrows ending on it go too
//! {op:"connect", from:id, to:id, label?, color?, fromSide?, toSide?, id?}
//! {op:"clear", confirm:true}                                 // server: needs confirmation
//! ```
//!
//! A failing op changes nothing and is reported in `errors`; the rest still apply.
//! `ids[i]` is the id op `i` created or touched (`null` when it failed, and for `clear`).
//!
//! [`apply_ops`] is a pure function over a [`TransactionMut`] and the `shapes` root map.

use serde::Serialize;
use serde_json::{json, Map as JsonMap, Value};
use yrs::{Any, GetString, Map, MapPrelim, MapRef, Out, Text, TextPrelim, TextRef, TransactionMut};

use crate::geometry::{center, contains_box, find_free_spot, js_round, BoxF, Point, Side};
use crate::schema::{
    color_for, is_valid_color, jnum, js_len, js_slice, json_f64, json_to_any, ShapeType,
    AGENT_STATUSES, MAX_DATA_URL, MAX_TEXT, MAX_TITLE,
};
use crate::shape::{
    all_shapes, arrow_mid, bare_id, box_lookup, live_shape, max_z, store_value, Endpoint, ImageRef,
    Shape,
};

/// Most ops accepted in one call (as on the page).
pub const MAX_OPS: usize = 500;
/// How long an agent shows `writing` after its last op.
pub const WRITING_MS: u64 = 1500;
const MAX_FAVICON: usize = 256 * 1024;
/// Accepted and dropped silently: the server owns these.
const IGNORED: [&str; 5] = ["id", "type", "by", "createdAt", "updatedAt"];

/// One canvas operation (parsed tolerantly, like the page).
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Add {
        shape: JsonMap<String, Value>,
    },
    Update {
        id: String,
        patch: JsonMap<String, Value>,
    },
    Move {
        id: String,
        dx: f64,
        dy: f64,
    },
    Resize {
        id: String,
        w: Option<f64>,
        h: Option<f64>,
    },
    Delete {
        id: String,
    },
    Connect {
        from: Value,
        to: Value,
        from_side: Option<Value>,
        to_side: Option<Value>,
        label: Option<Value>,
        color: Option<Value>,
        id: Option<Value>,
    },
    Clear {
        confirm: bool,
    },
}

fn shape_id_of(o: &JsonMap<String, Value>) -> Result<String, String> {
    match o.get("id") {
        Some(Value::String(s)) if !s.is_empty() => Ok(bare_id(s).to_owned()),
        _ => Err("`id` must be a shape id".into()),
    }
}

/// `v ?? 0` as a finite number.
fn num_or_zero(v: Option<&Value>, err: &str) -> Result<f64, String> {
    match v {
        None | Some(Value::Null) => Ok(0.0),
        Some(v) => json_f64(v).ok_or_else(|| err.to_owned()),
    }
}

impl Op {
    /// Parses one JSON op; the error text matches the page's.
    pub fn parse(v: &Value) -> Result<Self, String> {
        let o = v
            .as_object()
            .ok_or("an op must be an object like {op:\"add\", …}")?;
        match o.get("op").and_then(Value::as_str) {
            Some("add") => Ok(Op::Add {
                shape: o
                    .get("shape")
                    .and_then(Value::as_object)
                    .cloned()
                    .ok_or("`add` needs `shape:{type, …}`")?,
            }),
            Some("update") => {
                let id = shape_id_of(o)?;
                let patch = o
                    .get("patch")
                    .and_then(Value::as_object)
                    .or_else(|| o.get("props").and_then(Value::as_object))
                    .cloned()
                    .ok_or("`update` needs `patch:{…}`")?;
                Ok(Op::Update { id, patch })
            }
            Some("move") => {
                let err = "`dx` and `dy` must be finite numbers";
                Ok(Op::Move {
                    id: shape_id_of(o)?,
                    dx: num_or_zero(o.get("dx"), err)?,
                    dy: num_or_zero(o.get("dy"), err)?,
                })
            }
            Some("resize") => {
                let err = || "`w` and `h` must be positive numbers".to_owned();
                let dim = |k: &str| match o.get(k) {
                    None | Some(Value::Null) => Ok(None),
                    Some(v) => json_f64(v).map(Some).ok_or_else(err),
                };
                Ok(Op::Resize {
                    id: shape_id_of(o)?,
                    w: dim("w")?,
                    h: dim("h")?,
                })
            }
            Some("delete") => Ok(Op::Delete {
                id: shape_id_of(o)?,
            }),
            Some("connect") => Ok(Op::Connect {
                from: o.get("from").cloned().unwrap_or(Value::Null),
                to: o.get("to").cloned().unwrap_or(Value::Null),
                from_side: o.get("fromSide").cloned(),
                to_side: o.get("toSide").cloned(),
                label: o.get("label").cloned(),
                color: o.get("color").cloned(),
                id: o.get("id").cloned(),
            }),
            Some("clear") => Ok(Op::Clear {
                confirm: o.get("confirm") == Some(&Value::Bool(true)),
            }),
            _ => Err(format!(
                "unknown op {} (add, update, move, resize, delete, connect, clear)",
                o.get("op").unwrap_or(&Value::Null)
            )),
        }
    }

    /// The op's name as it appears on the wire.
    pub fn name(&self) -> &'static str {
        match self {
            Op::Add { .. } => "add",
            Op::Update { .. } => "update",
            Op::Move { .. } => "move",
            Op::Resize { .. } => "resize",
            Op::Delete { .. } => "delete",
            Op::Connect { .. } => "connect",
            Op::Clear { .. } => "clear",
        }
    }
}

/// Context for one batch of ops.
#[derive(Debug, Clone)]
pub struct OpsCtx {
    /// Written to `by` on every shape the batch creates.
    pub by: String,
    /// Written to `createdAt`/`updatedAt` (ms since the Unix epoch).
    pub now_ms: i64,
    /// `confirm: true` was given at request level, which unlocks `clear`.
    pub confirm_clear: bool,
    /// Where auto-placement looks for free space (the viewport centre on the page).
    pub origin: Point,
}

impl OpsCtx {
    /// A context stamped with the current time, origin `(0,0)` and `clear` locked.
    pub fn new(by: impl Into<String>) -> Self {
        Self {
            by: by.into(),
            now_ms: crate::schema::now_ms(),
            confirm_clear: false,
            origin: Point { x: 0.0, y: 0.0 },
        }
    }
}

/// One failed op (`index` is `-1` for request-level errors).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpError {
    pub index: i64,
    pub op: String,
    pub error: String,
}

/// Result of a batch: `{applied, ids, errors}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct OpsResult {
    /// Number of ops that succeeded.
    pub applied: usize,
    /// `ids[i]`: the id op `i` created or touched; `null` when it failed, and for `clear`.
    pub ids: Vec<Option<String>>,
    pub errors: Vec<OpError>,
    /// Where an agent's cursor should rest: the last thing it touched.
    #[serde(skip)]
    pub anchor: Option<Point>,
}

impl OpsResult {
    /// A result carrying one request-level error.
    pub fn request_error(error: impl Into<String>) -> Self {
        Self {
            errors: vec![OpError {
                index: -1,
                op: String::new(),
                error: error.into(),
            }],
            ..Self::default()
        }
    }
}

/// Applies a batch of parsed ops (see the module docs).
pub fn apply_ops(txn: &mut TransactionMut, shapes: &MapRef, ops: &[Op], ctx: &OpsCtx) -> OpsResult {
    let mut res = OpsResult::default();
    for (index, op) in ops.iter().enumerate() {
        let out = apply_one(txn, shapes, op, ctx);
        record(&mut res, index, op.name(), out);
    }
    res
}

/// Parses and applies raw JSON ops; parse failures are reported at their index.
pub fn apply_json_ops(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    ops: &[Value],
    ctx: &OpsCtx,
) -> OpsResult {
    if ops.len() > MAX_OPS {
        return OpsResult::request_error(format!(
            "at most {MAX_OPS} ops per call (got {})",
            ops.len()
        ));
    }
    let mut res = OpsResult::default();
    for (index, raw) in ops.iter().enumerate() {
        let name = raw.get("op").and_then(Value::as_str).unwrap_or("");
        let out = Op::parse(raw).and_then(|op| apply_one(txn, shapes, &op, ctx));
        record(&mut res, index, js_slice(name, 32), out);
    }
    res
}

type OneResult = Result<(Option<String>, Option<Point>), String>;

fn record(res: &mut OpsResult, index: usize, name: &str, out: OneResult) {
    match out {
        Ok((id, anchor)) => {
            res.applied += 1;
            res.ids.push(id);
            if anchor.is_some() {
                res.anchor = anchor;
            }
        }
        Err(error) => {
            res.ids.push(None);
            res.errors.push(OpError {
                index: i64::try_from(index).unwrap_or(i64::MAX),
                op: name.to_owned(),
                error,
            });
        }
    }
}

fn apply_one(txn: &mut TransactionMut, shapes: &MapRef, op: &Op, ctx: &OpsCtx) -> OneResult {
    match op {
        Op::Add { shape } => {
            let (id, at) = op_add(txn, shapes, shape, ctx)?;
            Ok((Some(id), Some(at)))
        }
        Op::Update { id, patch } => {
            let id = op_update(txn, shapes, id, patch, ctx)?;
            Ok((Some(id.clone()), Some(anchor_of(txn, shapes, &id, ctx))))
        }
        Op::Move { id, dx, dy } => {
            let id = op_move(txn, shapes, id, *dx, *dy, ctx)?;
            Ok((Some(id.clone()), Some(anchor_of(txn, shapes, &id, ctx))))
        }
        Op::Resize { id, w, h } => {
            let id = op_resize(txn, shapes, id, *w, *h, ctx)?;
            Ok((Some(id.clone()), Some(anchor_of(txn, shapes, &id, ctx))))
        }
        Op::Delete { id } => {
            let at = anchor_of(txn, shapes, id, ctx);
            let id = op_delete(txn, shapes, id)?;
            Ok((Some(id), Some(at)))
        }
        Op::Connect { .. } => {
            let (id, at) = op_connect(txn, shapes, op, ctx)?;
            Ok((Some(id), Some(at)))
        }
        Op::Clear { confirm } => {
            if !(*confirm || ctx.confirm_clear) {
                return Err("clear needs confirmation: send confirm: true".into());
            }
            shapes.clear(txn);
            Ok((None, None))
        }
    }
}

// ---- validation -------------------------------------------------------------------------

/// `^[A-Za-z0-9_:.-]{1,64}$`
fn valid_shape_id(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'.' | b'-'))
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// A `data:image/…` (≤ 2 MiB) or `https:` URL.
pub fn valid_image_src(v: &Value) -> Result<String, String> {
    let err = || "`src` must be a data: or https: URL".to_owned();
    let src = v.as_str().filter(|s| !s.is_empty()).ok_or_else(err)?;
    if src.starts_with("data:") {
        // ^data:image\/[a-z0-9.+-]+[;,]
        let ok = starts_with_ci(src, "data:image/")
            && src["data:image/".len()..]
                .bytes()
                .position(|b| b == b';' || b == b',')
                .is_some_and(|end| {
                    end > 0
                        && src.as_bytes()["data:image/".len().."data:image/".len() + end]
                            .iter()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
                });
        if !ok {
            return Err("`src` data URL must be an image".into());
        }
        if src.len() > MAX_DATA_URL {
            return Err("image data URL is over 2 MB".into());
        }
        return Ok(src.to_owned());
    }
    if starts_with_ci(src, "https://")
        && src.len() > "https://".len()
        && !src.chars().any(char::is_whitespace)
    {
        return Ok(src.to_owned());
    }
    Err(err())
}

fn has_scheme(s: &str) -> bool {
    // ^[a-z][a-z0-9+.-]*:
    let mut chars = s.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) {
            return false;
        }
    }
    false
}

/// An `http(s)`, `mailto` or `copper` URL, normalised (bare hosts get `https://`).
pub fn valid_url(v: &Value) -> Result<String, String> {
    let raw = v
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("`url` must be a non-empty string")?;
    let t = raw.trim();
    let candidate = if has_scheme(t) {
        t.to_owned()
    } else {
        format!("https://{t}")
    };
    let parsed = url::Url::parse(&candidate)
        .map_err(|_| format!("`url` is not a URL: {}", js_slice(t, 80)))?;
    if !matches!(parsed.scheme(), "http" | "https" | "mailto" | "copper") {
        return Err(format!("unsupported link scheme {}:", parsed.scheme()));
    }
    Ok(parsed.to_string())
}

/// Where a URL points, for a default link title.
pub fn host_of(u: &str) -> String {
    url::Url::parse(u)
        .ok()
        .and_then(|p| {
            p.host_str()
                .map(|h| h.strip_prefix("www.").unwrap_or(h).to_owned())
        })
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| u.to_owned())
}

fn check_end(
    txn: &TransactionMut,
    shapes: &MapRef,
    end: &Endpoint,
    key: &str,
    self_id: Option<&str>,
) -> Result<(), String> {
    let Some(r) = end.ref_id() else { return Ok(()) };
    if Some(r) == self_id {
        return Err(format!("`{key}` cannot be the arrow itself"));
    }
    match live_shape(txn, shapes, r) {
        None => Err(format!("`{key}`: no shape {r}")),
        Some(t) if t.is_arrow() => Err(format!("`{key}`: {r} is an arrow; arrows join boxes")),
        Some(_) => Ok(()),
    }
}

/// The checked, normalised props a caller gave for a shape of `kind` (the page's
/// `cleanProps`). `image: null` survives as `Value::Null` (= remove).
fn clean_props(
    kind: ShapeType,
    raw: &JsonMap<String, Value>,
    txn: &TransactionMut,
    shapes: &MapRef,
    self_id: Option<&str>,
) -> Result<JsonMap<String, Value>, String> {
    let allowed = kind.allowed();
    let mut out = JsonMap::new();
    for (raw_key, value) in raw {
        let key = kind.alias(raw_key);
        if IGNORED.contains(&key) {
            continue;
        }
        if !allowed.contains(&key) {
            if kind == ShapeType::Arrow && matches!(key, "x" | "y" | "w" | "h") {
                return Err(
                    "arrows have no box of their own: set `from`/`to` (or move the shapes they join)"
                        .into(),
                );
            }
            return Err(format!(
                "unknown prop `{raw_key}` for {} (allowed: {})",
                kind.as_str(),
                allowed.join(", ")
            ));
        }
        let cleaned = match key {
            "x" | "y" | "z" => {
                json_f64(value).ok_or_else(|| format!("`{key}` must be a finite number"))?;
                value.clone()
            }
            "w" | "h" => {
                json_f64(value)
                    .filter(|n| *n > 0.0)
                    .ok_or_else(|| format!("`{key}` must be a positive number"))?;
                value.clone()
            }
            "naturalW" | "naturalH" => {
                json_f64(value)
                    .filter(|n| *n >= 0.0)
                    .ok_or_else(|| format!("`{key}` must be a non-negative number"))?;
                value.clone()
            }
            "color" => match value.as_str() {
                Some(c) if is_valid_color(c) => value.clone(),
                _ => {
                    return Err(
                        "`color` must be yellow, pink, blue, green, purple, gray, white or a #hex colour"
                            .into(),
                    )
                }
            },
            "text" => {
                let t = value.as_str().ok_or("`text` must be a string")?;
                if js_len(t) > MAX_TEXT {
                    return Err(format!("`text` is over {MAX_TEXT} characters"));
                }
                value.clone()
            }
            "title" | "label" => {
                let t = value
                    .as_str()
                    .ok_or_else(|| format!("`{key}` must be a string"))?;
                Value::String(js_slice(t, MAX_TITLE).to_owned())
            }
            "fontSize" => {
                json_f64(value)
                    .filter(|n| (6.0..=240.0).contains(n))
                    .ok_or("`fontSize` must be between 6 and 240")?;
                value.clone()
            }
            "align" => match value.as_str() {
                Some("left" | "center" | "right") => value.clone(),
                _ => return Err("`align` must be left, center or right".into()),
            },
            "image" => clean_image(value)?,
            "src" => Value::String(valid_image_src(value)?),
            "url" => Value::String(valid_url(value)?),
            "favicon" => {
                let f = value
                    .as_str()
                    .ok_or("`favicon` must be a data: or https: URL")?;
                if !f.is_empty() {
                    if !(starts_with_ci(f, "data:image/") || starts_with_ci(f, "https://")) {
                        return Err("`favicon` must be a data: or https: URL".into());
                    }
                    if f.len() > MAX_FAVICON {
                        return Err("`favicon` is over 256 KB".into());
                    }
                }
                value.clone()
            }
            "from" | "to" => {
                let end = Endpoint::read(value).ok_or_else(|| {
                    format!("`{key}` must be a shape id, {{ref, side?}} or {{x, y}}")
                })?;
                check_end(txn, shapes, &end, key, self_id)?;
                end.to_json()
            }
            _ => value.clone(),
        };
        out.insert(key.to_owned(), cleaned);
    }
    Ok(out)
}

fn clean_image(value: &Value) -> Result<Value, String> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::String(s) if s.is_empty() => Ok(Value::Null),
        Value::String(_) => Ok(ImageRef {
            src: valid_image_src(value)?,
            natural_w: 0.0,
            natural_h: 0.0,
        }
        .to_json()),
        Value::Object(o) => Ok(ImageRef {
            src: valid_image_src(o.get("src").unwrap_or(&Value::Null))?,
            natural_w: o.get("naturalW").and_then(json_f64).unwrap_or(0.0),
            natural_h: o.get("naturalH").and_then(json_f64).unwrap_or(0.0),
        }
        .to_json()),
        _ => Err("`image` must be {src, naturalW, naturalH} or null".into()),
    }
}

fn num(props: &JsonMap<String, Value>, k: &str) -> Option<f64> {
    props.get(k).and_then(json_f64)
}

/// An image shape's starting size: natural size, at most 480 wide.
fn image_size_for(width: f64, height: f64) -> (f64, f64) {
    let nw = if width > 0.0 { width } else { 320.0 };
    let nh = if height > 0.0 { height } else { 240.0 };
    let w = js_round(nw.min(480.0));
    (w.max(16.0), js_round(w * nh / nw).max(16.0))
}

/// A frame sized for an image: 160–640 wide.
fn frame_size_for(width: f64, height: f64) -> (f64, f64) {
    let w = js_round(width.clamp(160.0, 640.0));
    let body = if height > 0.0 && width > 0.0 {
        w * height / width
    } else {
        w * 0.75
    };
    (w, js_round(body).max(120.0))
}

fn clamp_min(kind: ShapeType, props: &mut JsonMap<String, Value>) {
    if let Some((mw, mh)) = kind.min_size() {
        if let Some(w) = num(props, "w") {
            props.insert("w".into(), w.max(mw).into());
        }
        if let Some(h) = num(props, "h") {
            props.insert("h".into(), h.max(mh).into());
        }
    }
}

// ---- writes -------------------------------------------------------------------------------

fn shape_map(txn: &TransactionMut, shapes: &MapRef, id: &str) -> Option<MapRef> {
    match shapes.get(txn, id) {
        Some(Out::YMap(m)) => Some(m),
        _ => None,
    }
}

/// Byte offsets of the smallest single splice turning `prev` into `next`.
fn text_splice<'a>(prev: &str, next: &'a str) -> (usize, usize, &'a str) {
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

fn splice_into(txn: &mut TransactionMut, t: &TextRef, next: &str) {
    let prev = t.get_string(txn);
    let (index, remove, insert) = text_splice(&prev, next);
    let index = u32::try_from(index).unwrap_or(u32::MAX);
    if remove > 0 {
        t.remove_range(txn, index, u32::try_from(remove).unwrap_or(u32::MAX));
    }
    if !insert.is_empty() {
        t.insert(txn, index, insert);
    }
}

/// Sets a shape's body text: a minimal splice into its `Y.Text` (created from a plain
/// string if needed) for sticky/text, a plain string otherwise.
fn set_text(txn: &mut TransactionMut, m: &MapRef, kind: ShapeType, next: &str) {
    match m.get(txn, "text") {
        Some(Out::YText(t)) => splice_into(txn, &t, next),
        other => {
            if !kind.has_body() {
                m.insert(txn, "text", next);
                return;
            }
            let current = match other {
                Some(Out::Any(Any::String(s))) => s.to_string(),
                _ => String::new(),
            };
            let t = m.insert(txn, "text", TextPrelim::new(current));
            splice_into(txn, &t, next);
        }
    }
}

/// Writes `props` onto an existing shape (the page's `CanvasStore.update`).
fn store_update(
    txn: &mut TransactionMut,
    m: &MapRef,
    kind: ShapeType,
    props: &JsonMap<String, Value>,
    ctx: &OpsCtx,
) {
    for (k, v) in props {
        if k == "text" {
            if let Some(t) = v.as_str() {
                set_text(txn, m, kind, t);
                continue;
            }
        }
        if k == "image" && v.is_null() {
            m.remove(txn, "image");
            continue;
        }
        if let Some(any) = store_value(k, v) {
            m.try_update(txn, k.as_str(), any);
        }
    }
    m.try_update(txn, "updatedAt", Any::from(ctx.now_ms));
}

/// Creates a shape (the page's `CanvasStore.create`).
fn create(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    kind: ShapeType,
    id: &str,
    props: &JsonMap<String, Value>,
    ctx: &OpsCtx,
) {
    let existing = all_shapes(txn, shapes);
    let mut full = JsonMap::new();
    let (dw, dh) = kind.default_size();
    for (k, v) in [("x", 0.0), ("y", 0.0), ("w", dw), ("h", dh)] {
        full.insert(k.into(), v.into());
    }
    full.insert("color".into(), kind.default_color().into());
    full.insert("z".into(), (max_z(&existing) + 1.0).into());
    full.insert("createdAt".into(), ctx.now_ms.into());
    full.insert("updatedAt".into(), ctx.now_ms.into());
    for (k, v) in props {
        full.insert(k.clone(), v.clone());
    }
    let mut prelim: Vec<(String, Any)> = vec![
        ("id".into(), Any::from(id)),
        ("type".into(), Any::from(kind.as_str())),
    ];
    for (k, v) in &full {
        if k == "text" || v.is_null() {
            continue;
        }
        if let Some(any) = store_value(k, v) {
            prelim.push((k.clone(), any));
        }
    }
    let m = shapes.insert(txn, id, MapPrelim::from_iter(prelim));
    let text = full.get("text").and_then(Value::as_str);
    if kind.has_body() {
        m.insert(txn, "text", TextPrelim::new(text.unwrap_or_default()));
    } else if let Some(t) = text.filter(|t| !t.is_empty()) {
        m.insert(txn, "text", t);
    }
}

fn need_shape(txn: &TransactionMut, shapes: &MapRef, id: &str) -> Result<Shape, String> {
    live_shape(txn, shapes, bare_id(id)).ok_or_else(|| format!("no shape {id}"))
}

/// Where an agent's cursor goes after touching `id`.
fn anchor_of(txn: &TransactionMut, shapes: &MapRef, id: &str, ctx: &OpsCtx) -> Point {
    let all = all_shapes(txn, shapes);
    let Some(s) = all.iter().find(|s| s.id == id) else {
        return ctx.origin;
    };
    if s.is_arrow() {
        return arrow_mid(s, &box_lookup(&all)).unwrap_or(ctx.origin);
    }
    center(&s.bbox())
}

// ---- the ops ------------------------------------------------------------------------------

/// Fills an image's missing size from its natural size (and vice versa), like the page.
fn size_image(props: &mut JsonMap<String, Value>) {
    let nw = num(props, "naturalW").unwrap_or(0.0);
    let nh = num(props, "naturalH").unwrap_or(0.0);
    let (pw, ph) = (num(props, "w"), num(props, "h"));
    if pw.is_none() || ph.is_none() {
        let (sw, sh) = image_size_for(
            if nw > 0.0 { nw } else { 320.0 },
            if nh > 0.0 { nh } else { 240.0 },
        );
        let (width, height) = match (pw, ph) {
            (None, Some(height)) => (js_round(height * sw / sh), height),
            (Some(width), None) => (width, js_round(width * sh / sw)),
            _ => (sw, sh),
        };
        props.insert("w".into(), width.into());
        props.insert("h".into(), height.into());
    }
    let width = num(props, "w").unwrap_or(0.0);
    let height = num(props, "h").unwrap_or(0.0);
    if nw <= 0.0 || nh <= 0.0 {
        props.insert(
            "naturalW".into(),
            (if nw > 0.0 { nw } else { width }).into(),
        );
        props.insert(
            "naturalH".into(),
            (if nh > 0.0 { nh } else { height }).into(),
        );
    }
}

/// Per-type requirements and derived defaults of a shape being added.
fn type_defaults(kind: ShapeType, props: &mut JsonMap<String, Value>) -> Result<(), String> {
    match kind {
        ShapeType::Image => {
            if !props.contains_key("src") {
                return Err("an image needs `src`".into());
            }
            size_image(props);
        }
        ShapeType::Frame => {
            if let Some(img) = props.get("image").and_then(ImageRef::read) {
                if !props.contains_key("w") && !props.contains_key("h") {
                    let nw = if img.natural_w > 0.0 {
                        img.natural_w
                    } else {
                        640.0
                    };
                    let nh = if img.natural_h > 0.0 {
                        img.natural_h
                    } else {
                        480.0
                    };
                    let (width, height) = frame_size_for(nw, nh);
                    props.insert("w".into(), width.into());
                    props.insert("h".into(), height.into());
                }
            }
        }
        ShapeType::Link => {
            let Some(u) = props.get("url").and_then(Value::as_str).map(str::to_owned) else {
                return Err("a link needs `url`".into());
            };
            if props
                .get("title")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                props.insert("title".into(), host_of(&u).into());
            }
        }
        _ => {}
    }
    Ok(())
}

#[allow(clippy::many_single_char_names)]
fn op_add(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    raw: &JsonMap<String, Value>,
    ctx: &OpsCtx,
) -> Result<(String, Point), String> {
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .and_then(ShapeType::parse)
        .ok_or("`shape.type` must be sticky, text, frame, arrow, image or link")?;
    let id = match raw.get("id") {
        None => None,
        Some(Value::String(s)) if valid_shape_id(s) => {
            if shapes.contains_key(txn, s) {
                return Err(format!("a shape with id {s} already exists"));
            }
            Some(s.clone())
        }
        Some(_) => return Err("`shape.id` must be 1–64 of A–Z a–z 0–9 _ - : .".into()),
    };
    let mut props = clean_props(kind, raw, txn, shapes, id.as_deref())?;
    let id = id.unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    props.insert("by".into(), ctx.by.clone().into());

    if kind == ShapeType::Arrow {
        if !(props.contains_key("from") && props.contains_key("to")) {
            return Err("an arrow needs `from` and `to`".into());
        }
        create(txn, shapes, kind, &id, &props, ctx);
        let all = all_shapes(txn, shapes);
        let anchor = all
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| arrow_mid(s, &box_lookup(&all)))
            .unwrap_or(Point { x: 0.0, y: 0.0 });
        return Ok((id, anchor));
    }
    type_defaults(kind, &mut props)?;
    let (dw, dh) = kind.default_size();
    let (mut w, mut h) = (
        num(&props, "w").unwrap_or(dw),
        num(&props, "h").unwrap_or(dh),
    );
    if let Some((mw, mh)) = kind.min_size() {
        w = w.max(mw);
        h = h.max(mh);
    }
    props.insert("w".into(), w.into());
    props.insert("h".into(), h.into());
    let (x, y) = (num(&props, "x"), num(&props, "y"));
    if x.is_none() || y.is_none() {
        let taken: Vec<BoxF> = all_shapes(txn, shapes)
            .iter()
            .filter(|s| !s.is_arrow())
            .map(Shape::bbox)
            .collect();
        let spot = find_free_spot(&taken, w, h, ctx.origin);
        if x.is_none() {
            props.insert("x".into(), spot.x.into());
        }
        if y.is_none() {
            props.insert("y".into(), spot.y.into());
        }
    }
    create(txn, shapes, kind, &id, &props, ctx);
    let b = BoxF {
        x: num(&props, "x").unwrap_or(0.0),
        y: num(&props, "y").unwrap_or(0.0),
        w,
        h,
    };
    Ok((id, center(&b)))
}

fn op_update(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    id: &str,
    patch: &JsonMap<String, Value>,
    ctx: &OpsCtx,
) -> Result<String, String> {
    let shape = need_shape(txn, shapes, id)?;
    let mut props = clean_props(shape.kind, patch, txn, shapes, Some(&shape.id))?;
    clamp_min(shape.kind, &mut props);
    if shape.is_arrow() {
        let pick = |k: &str, cur: &Option<Endpoint>| {
            props
                .get(k)
                .and_then(Endpoint::read)
                .or_else(|| cur.clone())
        };
        let (from, to) = (pick("from", &shape.from), pick("to", &shape.to));
        if let (Some(a), Some(b)) = (
            from.as_ref().and_then(Endpoint::ref_id),
            to.as_ref().and_then(Endpoint::ref_id),
        ) {
            if a == b {
                return Err("an arrow cannot start and end on the same shape".into());
            }
        }
    }
    let m = shape_map(txn, shapes, &shape.id).ok_or_else(|| format!("no shape {id}"))?;
    store_update(txn, &m, shape.kind, &props, ctx);
    Ok(shape.id)
}

fn op_move(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    id: &str,
    dx: f64,
    dy: f64,
    ctx: &OpsCtx,
) -> Result<String, String> {
    let shape = need_shape(txn, shapes, id)?;
    if !dx.is_finite() || !dy.is_finite() {
        return Err("`dx` and `dy` must be finite numbers".into());
    }
    let mut moves: Vec<(String, JsonMap<String, Value>)> = Vec::new();
    if shape.is_arrow() {
        let shift = |e: &Option<Endpoint>| match e {
            Some(Endpoint::Point { x, y }) => Some(json!({ "x": jnum(x + dx), "y": jnum(y + dy) })),
            Some(e) => Some(e.to_json()),
            None => None,
        };
        let mut p = JsonMap::new();
        if let Some(f) = shift(&shape.from) {
            p.insert("from".into(), f);
        }
        if let Some(t) = shift(&shape.to) {
            p.insert("to".into(), t);
        }
        moves.push((shape.id.clone(), p));
    } else {
        let at = |s: &Shape| {
            let mut p = JsonMap::new();
            p.insert("x".into(), (s.x + dx).into());
            p.insert("y".into(), (s.y + dy).into());
            p
        };
        moves.push((shape.id.clone(), at(&shape)));
        if shape.kind == ShapeType::Frame {
            let frame = shape.bbox();
            for child in all_shapes(txn, shapes) {
                if child.id != shape.id && !child.is_arrow() && contains_box(&frame, &child.bbox())
                {
                    moves.push((child.id.clone(), at(&child)));
                }
            }
        }
    }
    for (mid, p) in moves {
        if let Some(m) = shape_map(txn, shapes, &mid) {
            for (k, v) in &p {
                if let Some(any) = store_value(k, v) {
                    m.try_update(txn, k.as_str(), any);
                }
            }
            m.try_update(txn, "updatedAt", Any::from(ctx.now_ms));
        }
    }
    Ok(shape.id)
}

fn op_resize(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    id: &str,
    w: Option<f64>,
    h: Option<f64>,
    ctx: &OpsCtx,
) -> Result<String, String> {
    let shape = need_shape(txn, shapes, id)?;
    let Some((mw, mh)) = shape.kind.min_size() else {
        return Err(format!("{} shapes have no size", shape.kind.as_str()));
    };
    let (w, h) = (w.unwrap_or(shape.w), h.unwrap_or(shape.h));
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return Err("`w` and `h` must be positive numbers".into());
    }
    let mut props = JsonMap::new();
    props.insert("w".into(), w.max(mw).into());
    props.insert("h".into(), h.max(mh).into());
    let m = shape_map(txn, shapes, &shape.id).ok_or_else(|| format!("no shape {id}"))?;
    store_update(txn, &m, shape.kind, &props, ctx);
    Ok(shape.id)
}

fn op_delete(txn: &mut TransactionMut, shapes: &MapRef, id: &str) -> Result<String, String> {
    let shape = need_shape(txn, shapes, id)?;
    let cascade: Vec<String> = all_shapes(txn, shapes)
        .into_iter()
        .filter(|s| s.touches(&[shape.id.as_str()]))
        .map(|s| s.id)
        .collect();
    shapes.remove(txn, &shape.id);
    for a in cascade {
        shapes.remove(txn, &a);
    }
    Ok(shape.id)
}

fn op_connect(
    txn: &mut TransactionMut,
    shapes: &MapRef,
    op: &Op,
    ctx: &OpsCtx,
) -> Result<(String, Point), String> {
    let Op::Connect {
        from,
        to,
        from_side,
        to_side,
        label,
        color,
        id,
    } = op
    else {
        return Err("not a connect op".into());
    };
    let end = |v: &Value, side: &Option<Value>, key: &str| -> Result<Endpoint, String> {
        let mut e = Endpoint::read(v).ok_or_else(|| format!("`{key}` must be a shape id"))?;
        if let (Endpoint::Ref { side: s, .. }, Some(raw)) = (&mut e, side) {
            *s = Some(
                raw.as_str()
                    .and_then(Side::parse)
                    .ok_or_else(|| format!("`{key}Side` must be top, right, bottom or left"))?,
            );
        }
        Ok(e)
    };
    let from = end(from, from_side, "from")?;
    let to = end(to, to_side, "to")?;
    if let (Some(a), Some(b)) = (from.ref_id(), to.ref_id()) {
        if a == b {
            return Err("`from` and `to` are the same shape".into());
        }
    }
    let mut raw = JsonMap::new();
    raw.insert("from".into(), from.to_json());
    raw.insert("to".into(), to.to_json());
    if let Some(l) = label {
        raw.insert("label".into(), l.clone());
    }
    if let Some(c) = color {
        raw.insert("color".into(), c.clone());
    }
    let id = match id {
        None => None,
        Some(Value::String(s)) if valid_shape_id(s) => {
            if shapes.contains_key(txn, s) {
                return Err(format!("a shape with id {s} already exists"));
            }
            Some(s.clone())
        }
        Some(_) => return Err("`id` must be 1–64 of A–Z a–z 0–9 _ - : .".into()),
    };
    let mut props = clean_props(ShapeType::Arrow, &raw, txn, shapes, id.as_deref())?;
    props.insert("by".into(), ctx.by.clone().into());
    let id = id.unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    create(txn, shapes, ShapeType::Arrow, &id, &props, ctx);
    let at = anchor_of(txn, shapes, &id, ctx);
    Ok((id, at))
}

// ---- agents -------------------------------------------------------------------------------

/// A change to an agent's presence (unset fields keep their previous value).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentPatch {
    pub name: Option<String>,
    pub color: Option<String>,
    /// `Some(None)` clears the cursor.
    pub cursor: Option<Option<Point>>,
    /// `idle` | `thinking` | `writing`.
    pub status: Option<&'static str>,
}

/// One `agents` entry as read back.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentPresence {
    pub name: String,
    pub color: String,
    pub cursor: Option<Point>,
    pub status: String,
    pub updated_at: f64,
}

/// Tolerant read of one `agents` value (the page's `readAgent`).
#[allow(clippy::cast_precision_loss)]
pub fn read_agent(v: &Value) -> Option<AgentPresence> {
    let o = v.as_object()?;
    let cursor = o.get("cursor").and_then(Value::as_object).and_then(|c| {
        Some(Point {
            x: c.get("x").and_then(json_f64)?,
            y: c.get("y").and_then(json_f64)?,
        })
    });
    let updated = match o.get("updatedAt") {
        Some(Value::Number(n)) => n
            .as_f64()
            .map_or(0.0, |t| if t < 1e12 { t * 1000.0 } else { t }),
        Some(Value::String(s)) => {
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .map_or(0.0, |t| (t.unix_timestamp_nanos() / 1_000_000) as f64)
        }
        _ => 0.0,
    };
    Some(AgentPresence {
        name: o
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("Agent")
            .to_owned(),
        color: o
            .get("color")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        cursor,
        status: o
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| AGENT_STATUSES.contains(s))
            .unwrap_or("idle")
            .to_owned(),
        updated_at: updated,
    })
}

/// Merges `patch` into agent `id`'s entry and writes it whole (the page's `writeAgent`).
pub fn write_agent(
    txn: &mut TransactionMut,
    agents: &MapRef,
    id: &str,
    patch: &AgentPatch,
    now_ms: i64,
) -> Value {
    let prev = agents
        .get(txn, id)
        .map(|o| crate::schema::out_to_json(txn, &o))
        .and_then(|v| read_agent(&v));
    let cursor = match patch.cursor {
        None => prev.as_ref().and_then(|p| p.cursor),
        Some(None) => None,
        Some(Some(p)) => Some(Point {
            x: js_round(p.x),
            y: js_round(p.y),
        }),
    };
    let name = patch
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .or_else(|| prev.as_ref().map(|p| p.name.clone()))
        .unwrap_or_else(|| "Agent".into());
    let color = patch
        .color
        .clone()
        .filter(|c| !c.is_empty())
        .or_else(|| prev.as_ref().map(|p| p.color.clone()))
        .unwrap_or_default();
    let status = patch
        .status
        .map(str::to_owned)
        .or_else(|| prev.as_ref().map(|p| p.status.clone()))
        .unwrap_or_else(|| "idle".into());
    let value = json!({
        "name": js_slice(&name, 60),
        "color": js_slice(&color, 32),
        "cursor": cursor.map(|c| json!({ "x": jnum(c.x), "y": jnum(c.y) })),
        "status": status,
        "updatedAt": now_ms,
    });
    agents.insert(txn, id, json_to_any(&value));
    value
}

/// Who an ops call acts as (`as` in the request), parsed like the page's `parseApplyInput`.
#[derive(Debug, Clone, PartialEq)]
pub struct Actor {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
}

impl Actor {
    /// `"name"` or `{id?, name?, color?}`; `Ok(None)` when absent or null.
    pub fn parse(v: Option<&Value>) -> Result<Option<Self>, String> {
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(Self {
                id: format!("agent:{s}"),
                name: s.clone(),
                color: None,
            })),
            Some(Value::Object(o)) => {
                let name = o
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map_or_else(|| "Agent".to_owned(), |n| js_slice(n, 60).to_owned());
                let id = o
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map_or_else(
                        || format!("agent:{}", name.to_lowercase()),
                        |n| js_slice(n, 64).to_owned(),
                    );
                let color = o
                    .get("color")
                    .and_then(Value::as_str)
                    .filter(|c| !c.is_empty())
                    .map(|c| js_slice(c, 32).to_owned());
                Ok(Some(Self { id, name, color }))
            }
            Some(_) => Err("`as` must be {id,name,color}".into()),
        }
    }

    /// The presence written right after this actor's ops landed.
    pub fn writing(&self, at: Option<Point>) -> AgentPatch {
        AgentPatch {
            name: Some(self.name.clone()),
            color: Some(self.color.clone().unwrap_or_else(|| color_for(&self.id))),
            cursor: at.map(Some),
            status: Some("writing"),
        }
    }
}

#[cfg(test)]
mod tests;
