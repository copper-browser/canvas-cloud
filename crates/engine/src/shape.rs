//! Tolerant reads of shapes from the Yjs document (mirrors the page's `readShape`), plus
//! the value normalisation used when writing props (`storeValue`).

use serde_json::{json, Value};
use yrs::{Any, Map, MapRef, Out, ReadTxn};

use crate::checklist::{read_checklist, Checklist};
use crate::geometry::{arrow_box, arrow_path, js_round, BoxF, End, Point, Side};
use crate::schema::{
    is_valid_color, jnum, json_f64, out_f64, out_string, out_to_json, ShapeType, SHAPES,
};

/// `shape:` prefix some callers put on ids.
pub fn bare_id(s: &str) -> &str {
    s.strip_prefix("shape:").unwrap_or(s)
}

/// An arrow end as stored: a free point, or a shape optionally pinned to a side.
#[derive(Debug, Clone, PartialEq)]
pub enum Endpoint {
    Point { x: f64, y: f64 },
    Ref { id: String, side: Option<Side> },
}

impl Endpoint {
    /// Any accepted spelling — `{x,y}`, `{ref, side?}`, `{id, side?}`, `"id"` or `"shape:id"`
    /// — or `None` (the page's `readEndpoint`). Points are rounded.
    pub fn read(v: &Value) -> Option<Self> {
        if let Some(s) = v.as_str() {
            let t = s.trim();
            return (!t.is_empty()).then(|| Self::Ref {
                id: bare_id(t).to_owned(),
                side: None,
            });
        }
        let o = v.as_object()?;
        let side = o.get("side").and_then(Value::as_str).and_then(Side::parse);
        for key in ["ref", "id"] {
            if let Some(r) = o.get(key).and_then(Value::as_str) {
                let t = r.trim();
                if !t.is_empty() {
                    return Some(Self::Ref {
                        id: bare_id(t).to_owned(),
                        side,
                    });
                }
            }
        }
        let x = o.get("x").and_then(json_f64)?;
        let y = o.get("y").and_then(json_f64)?;
        Some(Self::Point {
            x: js_round(x),
            y: js_round(y),
        })
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::Point { x, y } => json!({ "x": jnum(*x), "y": jnum(*y) }),
            Self::Ref { id, side: Some(s) } => json!({ "ref": id, "side": s.as_str() }),
            Self::Ref { id, side: None } => json!({ "ref": id }),
        }
    }

    pub fn ref_id(&self) -> Option<&str> {
        match self {
            Self::Ref { id, .. } => Some(id),
            Self::Point { .. } => None,
        }
    }
}

/// `{src, naturalW, naturalH}` of a frame image.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageRef {
    pub src: String,
    pub natural_w: f64,
    pub natural_h: f64,
}

impl ImageRef {
    pub fn read(v: &Value) -> Option<Self> {
        let o = v.as_object()?;
        let src = o
            .get("src")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?;
        Some(Self {
            src: src.to_owned(),
            natural_w: o.get("naturalW").and_then(json_f64).unwrap_or(0.0),
            natural_h: o.get("naturalH").and_then(json_f64).unwrap_or(0.0),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({ "src": self.src, "naturalW": jnum(self.natural_w), "naturalH": jnum(self.natural_h) })
    }
}

/// A plain read of one shape (defaults filled in like the page does).
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub id: String,
    pub kind: ShapeType,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub color: String,
    pub by: Option<String>,
    pub created_at: f64,
    pub updated_at: f64,
    pub z: f64,
    pub text: String,
    pub title: String,
    pub font_size: Option<f64>,
    pub align: Option<String>,
    pub image: Option<ImageRef>,
    pub from: Option<Endpoint>,
    pub to: Option<Endpoint>,
    pub label: String,
    pub src: String,
    pub natural_w: f64,
    pub natural_h: f64,
    pub url: String,
    /// A checklist's columns, rows and picks (`None` for other types).
    pub checklist: Option<Checklist>,
}

impl Shape {
    pub fn bbox(&self) -> BoxF {
        BoxF {
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
        }
    }

    pub fn is_arrow(&self) -> bool {
        self.kind == ShapeType::Arrow
    }

    /// Whether one of this arrow's ends is bound to a shape in `ids`.
    pub fn touches(&self, ids: &[&str]) -> bool {
        let hit = |e: &Option<Endpoint>| {
            e.as_ref()
                .and_then(Endpoint::ref_id)
                .is_some_and(|r| ids.contains(&r))
        };
        self.is_arrow() && (hit(&self.from) || hit(&self.to))
    }
}

/// Reads one shape map; `None` when it is not a readable shape.
pub fn read_shape<T: ReadTxn>(txn: &T, id: &str, out: &Out) -> Option<Shape> {
    let Out::YMap(m) = out else { return None };
    let get = |k: &str| m.get(txn, k);
    let num = |k: &str| out_f64(get(k).as_ref());
    let s = |k: &str| out_string(txn, get(k).as_ref());
    let kind = ShapeType::parse(&s("type")?)?;
    let (dw, dh) = kind.default_size();
    let color = s("color")
        .filter(|c| is_valid_color(c))
        .unwrap_or_else(|| kind.default_color().to_owned());
    let json = |k: &str| get(k).map(|o| out_to_json(txn, &o));
    let w = num("w").unwrap_or(dw).max(0.0);
    let h = num("h").unwrap_or(dh).max(0.0);
    let is = |t: ShapeType| kind == t;
    Some(Shape {
        id: id.to_owned(),
        x: num("x").unwrap_or(0.0),
        y: num("y").unwrap_or(0.0),
        w,
        h,
        color,
        by: s("by").filter(|b| !b.is_empty()),
        created_at: num("createdAt").unwrap_or(0.0),
        updated_at: num("updatedAt").unwrap_or(0.0),
        z: num("z").unwrap_or(0.0),
        text: s("text").unwrap_or_default(),
        title: s("title").unwrap_or_default(),
        font_size: num("fontSize").filter(|f| *f > 0.0),
        align: s("align").filter(|a| matches!(a.as_str(), "left" | "center" | "right")),
        image: if is(ShapeType::Frame) {
            json("image").as_ref().and_then(ImageRef::read)
        } else {
            None
        },
        from: if is(ShapeType::Arrow) {
            json("from").as_ref().and_then(Endpoint::read)
        } else {
            None
        },
        to: if is(ShapeType::Arrow) {
            json("to").as_ref().and_then(Endpoint::read)
        } else {
            None
        },
        label: s("label").unwrap_or_default(),
        src: s("src").unwrap_or_default(),
        natural_w: num("naturalW").unwrap_or(w),
        natural_h: num("naturalH").unwrap_or(h),
        url: s("url").unwrap_or_default(),
        checklist: is(ShapeType::Checklist).then(|| read_checklist(&json)),
        kind,
    })
}

/// Every readable shape of the document, in map order.
pub fn all_shapes<T: ReadTxn>(txn: &T, shapes: &MapRef) -> Vec<Shape> {
    shapes
        .iter(txn)
        .filter_map(|(id, out)| read_shape(txn, id, &out))
        .collect()
}

/// One shape by id.
pub fn live_shape<T: ReadTxn>(txn: &T, shapes: &MapRef, id: &str) -> Option<Shape> {
    shapes.get(txn, id).and_then(|o| read_shape(txn, id, &o))
}

/// Every readable shape (when the `shapes` root map exists).
pub fn doc_shapes<T: ReadTxn>(txn: &T) -> Vec<Shape> {
    txn.get_map(SHAPES)
        .map(|m| all_shapes(txn, &m))
        .unwrap_or_default()
}

/// Resolves an endpoint against the current boxes (arrows are not boxes).
pub fn resolve_end(e: Option<&Endpoint>, box_of: &dyn Fn(&str) -> Option<BoxF>) -> Option<End> {
    match e? {
        Endpoint::Point { x, y } => Some(End::Point(Point { x: *x, y: *y })),
        Endpoint::Ref { id, side } => box_of(id).map(|b| End::Box(b, *side)),
    }
}

/// Bounding box of an arrow given the current boxes (`0,0,0,0` when it cannot be drawn).
pub fn arrow_bbox(arrow: &Shape, box_of: &dyn Fn(&str) -> Option<BoxF>) -> BoxF {
    let path = resolve_end(arrow.from.as_ref(), box_of)
        .zip(resolve_end(arrow.to.as_ref(), box_of))
        .and_then(|(a, b)| arrow_path(a, b));
    path.as_ref().map_or(
        BoxF {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        },
        arrow_box,
    )
}

/// Midpoint of an arrow's path, if it can be drawn.
pub fn arrow_mid(arrow: &Shape, box_of: &dyn Fn(&str) -> Option<BoxF>) -> Option<Point> {
    let a = resolve_end(arrow.from.as_ref(), box_of)?;
    let b = resolve_end(arrow.to.as_ref(), box_of)?;
    arrow_path(a, b).map(|p| p.mid)
}

/// A box lookup over a set of shapes (arrows excluded).
pub fn box_lookup(shapes: &[Shape]) -> impl Fn(&str) -> Option<BoxF> + '_ {
    move |id: &str| {
        shapes
            .iter()
            .find(|s| s.id == id && !s.is_arrow())
            .map(Shape::bbox)
    }
}

/// The value as stored for `key` (the page's `storeValue`): boxes rounded, endpoints and
/// images normalised. `None` means "do not store".
pub fn store_value(key: &str, value: &Value) -> Option<Any> {
    match key {
        "x" | "y" | "w" | "h" => match json_f64(value) {
            Some(n) => Some(crate::schema::num_any(js_round(n))),
            None => Some(crate::schema::json_to_any(value)),
        },
        "from" | "to" => Endpoint::read(value).map(|e| crate::schema::json_to_any(&e.to_json())),
        "image" => ImageRef::read(value).map(|i| crate::schema::json_to_any(&i.to_json())),
        _ => Some(crate::schema::json_to_any(value)),
    }
}

/// The current `text` of a shape map as a plain string.
pub fn map_text<T: ReadTxn>(txn: &T, m: &MapRef) -> Option<String> {
    out_string(txn, m.get(txn, "text").as_ref())
}

/// Highest `z` (at least 0), like the page's `maxZ`.
pub fn max_z(shapes: &[Shape]) -> f64 {
    shapes.iter().map(|s| s.z).fold(0.0, f64::max)
}

/// Whether `Any` is a string equal to `s`.
pub fn any_is_str(a: &Any, s: &str) -> bool {
    matches!(a, Any::String(v) if &**v == s)
}
