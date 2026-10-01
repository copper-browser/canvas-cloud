//! `canvas_read` format (spec §4), identical to the page's `readCanvas`
//! (`Canvas/src/ops.ts`): a compact, agent-friendly summary of a canvas document.

use serde::Serialize;
use serde_json::Value;
use yrs::{Doc, Map as _, ReadTxn, Transact as _};

use crate::geometry::{contains_box, js_round};
use crate::ops::read_agent;
use crate::schema::{jnum, js_len, js_slice, out_to_json, ser_num, ser_opt_num, ShapeType, AGENTS};
use crate::shape::{arrow_bbox, box_lookup, doc_shapes, Endpoint, Shape};

/// Text, titles and labels are cut to this many characters unless `full`.
pub const READ_TEXT_LIMIT: usize = 500;
/// Agents not written to for this long are hidden.
pub const AGENT_TTL_MS: f64 = 2.0 * 60.0 * 1000.0;

/// `{id, name, kind}` of the canvas being read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CanvasRef {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// `{src, naturalW, naturalH}` with `src` described rather than echoed when it is data.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageSummary {
    pub src: String,
    #[serde(rename = "naturalW", serialize_with = "ser_num")]
    pub natural_w: f64,
    #[serde(rename = "naturalH", serialize_with = "ser_num")]
    pub natural_h: f64,
}

/// One shape in the read format.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShapeSummary {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(serialize_with = "ser_num")]
    pub x: f64,
    #[serde(serialize_with = "ser_num")]
    pub y: f64,
    #[serde(serialize_with = "ser_num")]
    pub w: f64,
    #[serde(serialize_with = "ser_num")]
    pub h: f64,
    pub color: String,
    #[serde(serialize_with = "ser_num")]
    pub z: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(
        rename = "fontSize",
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_num"
    )]
    pub font_size: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Arrow ends: `{x,y}` or `{ref, side?}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    #[serde(
        rename = "naturalW",
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_num"
    )]
    pub natural_w: Option<f64>,
    #[serde(
        rename = "naturalH",
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_num"
    )]
    pub natural_h: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The innermost frame this shape sits in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
}

/// One live agent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub color: String,
    pub cursor: Option<Value>,
    pub status: String,
    #[serde(rename = "updatedAt", serialize_with = "ser_num")]
    pub updated_at: f64,
}

/// `{canvas, viewport?, shapes, agents, selection, count}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReadResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canvas: Option<CanvasRef>,
    /// The server has no viewport; always omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Value>,
    pub shapes: Vec<ShapeSummary>,
    pub agents: Vec<AgentSummary>,
    /// The server has no selection; always empty.
    pub selection: Vec<String>,
    /// Every readable shape in the document (before `ids`/`types` filtering).
    pub count: usize,
}

/// Read options (`full`, `ids`, `types`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadOpts {
    /// Whole text instead of the first 500 characters.
    pub full: bool,
    /// Only these shapes.
    pub ids: Option<Vec<String>>,
    /// Only these types.
    pub types: Option<Vec<String>>,
    /// "Now" for the agent liveness window (ms epoch; 0 = current time).
    pub now_ms: i64,
}

fn clip(s: &str, full: bool) -> String {
    if full || js_len(s) <= READ_TEXT_LIMIT {
        s.to_owned()
    } else {
        format!("{}…", js_slice(s, READ_TEXT_LIMIT))
    }
}

/// A data URL is never echoed whole: its kind and size are enough to reason about.
#[allow(clippy::cast_precision_loss)]
pub fn describe_src(src: &str, full: bool) -> String {
    if !src.starts_with("data:") || (full && src.len() <= 4096) {
        return src.to_owned();
    }
    let kind = src["data:".len()..]
        .split([';', ','])
        .next()
        .filter(|k| !k.is_empty())
        .unwrap_or("data");
    let comma = src.find(',').map_or(-1.0, |i| i as f64);
    let bytes = js_round((src.len() as f64 - comma - 1.0) * 3.0 / 4.0);
    let size = if bytes >= 1024.0 * 1024.0 {
        format!("{:.1} MB", bytes / 1024.0 / 1024.0)
    } else {
        format!("{} KB", js_round(bytes / 1024.0).max(1.0))
    };
    format!("data:{kind} ({size})")
}

/// Reads every shape and agent from `doc` (text cut to 500 characters unless `full`).
pub fn read_shapes(doc: &Doc, full: bool) -> ReadResult {
    let txn = doc.transact();
    read_canvas_txn(
        &txn,
        &ReadOpts {
            full,
            ..ReadOpts::default()
        },
    )
}

fn summarize(s: &Shape, all: &[Shape], frames: &[&Shape], full: bool) -> ShapeSummary {
    let bx = if s.is_arrow() {
        arrow_bbox(s, &box_lookup(all))
    } else {
        s.bbox()
    };
    let mut sum = ShapeSummary {
        id: s.id.clone(),
        kind: s.kind.as_str().to_owned(),
        x: js_round(bx.x),
        y: js_round(bx.y),
        w: js_round(bx.w),
        h: js_round(bx.h),
        color: s.color.clone(),
        z: s.z,
        by: s.by.clone(),
        text: None,
        font_size: None,
        align: None,
        title: None,
        image: None,
        label: None,
        from: None,
        to: None,
        src: None,
        natural_w: None,
        natural_h: None,
        url: None,
        frame: None,
    };
    match s.kind {
        ShapeType::Sticky | ShapeType::Text => {
            sum.text = Some(clip(&s.text, full));
            sum.font_size = s.font_size;
            sum.align.clone_from(&s.align);
        }
        ShapeType::Frame => {
            sum.title = Some(clip(&s.title, full));
            sum.image = s.image.as_ref().map(|i| ImageSummary {
                src: describe_src(&i.src, full),
                natural_w: i.natural_w,
                natural_h: i.natural_h,
            });
        }
        ShapeType::Arrow => {
            sum.label = Some(clip(&s.label, full));
            sum.from = s.from.as_ref().map(Endpoint::to_json);
            sum.to = s.to.as_ref().map(Endpoint::to_json);
        }
        ShapeType::Image => {
            sum.src = Some(describe_src(&s.src, full));
            sum.natural_w = Some(s.natural_w);
            sum.natural_h = Some(s.natural_h);
        }
        ShapeType::Link => {
            sum.url = Some(s.url.clone());
            sum.title = Some(clip(&s.title, full));
        }
    }
    if !s.is_arrow() {
        let inner = s.bbox();
        let mut best: Option<&Shape> = None;
        for f in frames {
            if f.id == s.id || !contains_box(&f.bbox(), &inner) {
                continue;
            }
            if best.is_none_or(|b| f.w * f.h < b.w * b.h) {
                best = Some(f);
            }
        }
        sum.frame = best.map(|f| f.id.clone());
    }
    sum
}

/// The board as an agent should see it, over an existing read transaction.
#[allow(clippy::cast_precision_loss)]
pub fn read_canvas_txn<T: ReadTxn>(txn: &T, opts: &ReadOpts) -> ReadResult {
    let all = doc_shapes(txn);
    let frames: Vec<&Shape> = all.iter().filter(|s| s.kind == ShapeType::Frame).collect();
    let mut shapes: Vec<ShapeSummary> = all
        .iter()
        .filter(|s| opts.ids.as_ref().is_none_or(|ids| ids.contains(&s.id)))
        .filter(|s| {
            opts.types
                .as_ref()
                .is_none_or(|t| t.iter().any(|k| k == s.kind.as_str()))
        })
        .map(|s| summarize(s, &all, &frames, opts.full))
        .collect();
    shapes.sort_by(|a, b| {
        a.y.total_cmp(&b.y)
            .then(a.x.total_cmp(&b.x))
            .then_with(|| a.id.cmp(&b.id))
    });

    let now = if opts.now_ms > 0 {
        opts.now_ms as f64
    } else {
        crate::schema::now_ms() as f64
    };
    let mut agents: Vec<AgentSummary> = Vec::new();
    if let Some(map) = txn.get_map(AGENTS) {
        for (id, value) in map.iter(txn) {
            let Some(a) = read_agent(&out_to_json(txn, &value)) else {
                continue;
            };
            if a.updated_at <= 0.0 || now - a.updated_at > AGENT_TTL_MS {
                continue;
            }
            agents.push(AgentSummary {
                id: id.to_owned(),
                name: a.name,
                color: a.color,
                cursor: a
                    .cursor
                    .map(|c| serde_json::json!({ "x": jnum(c.x), "y": jnum(c.y) })),
                status: a.status,
                updated_at: a.updated_at,
            });
        }
    }
    let busy = |a: &AgentSummary| u8::from(a.status == "idle");
    agents.sort_by(|a, b| {
        busy(a)
            .cmp(&busy(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.id.cmp(&b.id))
    });

    ReadResult {
        canvas: None,
        viewport: None,
        shapes,
        agents,
        selection: Vec::new(),
        count: all.len(),
    }
}

/// Reads `meta.name` from a document, if set.
pub fn meta_name<T: ReadTxn>(txn: &T) -> Option<String> {
    let meta = txn.get_map(crate::schema::META)?;
    crate::schema::out_string(txn, meta.get(txn, "name").as_ref())
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::geometry::Point;
    use crate::ops::{apply_json_ops, write_agent, AgentPatch, OpsCtx};
    use crate::schema::SHAPES;
    use serde_json::json;
    use yrs::Doc;

    fn ctx() -> OpsCtx {
        OpsCtx {
            by: "Ann".into(),
            now_ms: 7,
            confirm_clear: false,
            origin: Point { x: 0.0, y: 0.0 },
        }
    }

    #[test]
    fn read_format() {
        let doc = Doc::new();
        let shapes = doc.get_or_insert_map(SHAPES);
        let agents = doc.get_or_insert_map(AGENTS);
        let long = "x".repeat(600);
        let now = crate::schema::now_ms();
        {
            let mut txn = doc.transact_mut();
            let r = apply_json_ops(
                &mut txn,
                &shapes,
                json!([
                    {"op":"add","shape":{"id":"f","type":"frame","x":-50,"y":-50,"w":900,"h":400,"title":"Box"}},
                    {"op":"add","shape":{"id":"s","type":"sticky","x":0,"y":0,"text":long}},
                    {"op":"add","shape":{"id":"l","type":"link","x":400,"y":0,"url":"https://a.b"}},
                    {"op":"add","shape":{"id":"i","type":"image","x":0,"y":600,"src":"data:image/png;base64,AAAA"}},
                    {"op":"connect","from":"s","to":"l","label":"see"}
                ])
                .as_array()
                .unwrap(),
                &ctx(),
            );
            assert_eq!(r.applied, 5, "{r:?}");
            write_agent(
                &mut txn,
                &agents,
                "bot",
                &AgentPatch {
                    name: Some("Bot".into()),
                    status: Some("writing"),
                    ..AgentPatch::default()
                },
                now,
            );
            write_agent(
                &mut txn,
                &agents,
                "old",
                &AgentPatch {
                    name: Some("Old".into()),
                    ..AgentPatch::default()
                },
                now - 10 * 60 * 1000,
            );
        }
        let r = read_shapes(&doc, false);
        assert_eq!(r.count, 5);
        let ids: Vec<&str> = r.shapes.iter().map(|s| s.id.as_str()).collect();
        // Sorted by y, then x.
        assert_eq!(ids[0], "f");
        assert_eq!(*ids.last().unwrap(), "i");
        let s = r.shapes.iter().find(|s| s.id == "s").unwrap();
        assert_eq!(
            s.text.as_ref().unwrap().chars().count(),
            READ_TEXT_LIMIT + 1
        );
        assert_eq!(s.by.as_deref(), Some("Ann"));
        assert_eq!(s.frame.as_deref(), Some("f"));
        let l = r.shapes.iter().find(|s| s.id == "l").unwrap();
        assert_eq!(
            l.title.as_deref(),
            Some("a.b"),
            "default link title is the host"
        );
        assert_eq!(l.url.as_deref(), Some("https://a.b/"));
        let arrow = r.shapes.iter().find(|s| s.kind == "arrow").unwrap();
        assert_eq!(arrow.label.as_deref(), Some("see"));
        assert_eq!(arrow.from, Some(json!({"ref":"s"})));
        // Arrow box comes from the path between the two boxes.
        assert_eq!((arrow.x, arrow.w), (200.0, 200.0));
        let img = r.shapes.iter().find(|s| s.id == "i").unwrap();
        assert_eq!(img.src.as_deref(), Some("data:image/png (1 KB)"));
        assert_eq!(r.agents.len(), 1, "stale agents are hidden");
        assert_eq!(r.agents[0].name, "Bot");
        assert_eq!(r.agents[0].status, "writing");

        let full = read_shapes(&doc, true);
        let s = full.shapes.iter().find(|s| s.id == "s").unwrap();
        assert_eq!(s.text.as_ref().unwrap().len(), 600);
        assert_eq!(
            full.shapes
                .iter()
                .find(|s| s.id == "i")
                .unwrap()
                .src
                .as_deref(),
            Some("data:image/png;base64,AAAA")
        );

        let txn = doc.transact();
        let only = read_canvas_txn(
            &txn,
            &ReadOpts {
                types: Some(vec!["sticky".into()]),
                ..ReadOpts::default()
            },
        );
        assert_eq!(only.shapes.len(), 1);
        assert_eq!(only.count, 5);

        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("canvas").is_none());
        assert_eq!(v["selection"], json!([]));
        assert!(v["shapes"][0].get("text").is_none());
        assert_eq!(v["shapes"][0]["type"], "frame");
    }

    #[test]
    fn describe() {
        assert_eq!(describe_src("https://x/y.png", false), "https://x/y.png");
        let big = format!("data:image/jpeg;base64,{}", "A".repeat(2_000_000));
        assert_eq!(describe_src(&big, true), "data:image/jpeg (1.4 MB)");
    }
}
