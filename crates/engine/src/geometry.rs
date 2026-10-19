//! Board geometry shared with the canvas page (`Canvas/src/canvas/geometry.ts`,
//! `arrows.ts`, `placement.ts`): boxes, side anchors, arrow paths and free-space placement.
//! Pure functions, no Yjs.

/// A point in world coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// An axis-aligned box (top-left + size) in world coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxF {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A side of a box an arrow end can be pinned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

impl Side {
    /// Parses `top|right|bottom|left`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "top" => Some(Self::Top),
            "right" => Some(Self::Right),
            "bottom" => Some(Self::Bottom),
            "left" => Some(Self::Left),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Right => "right",
            Self::Bottom => "bottom",
            Self::Left => "left",
        }
    }

    fn normal(self) -> Point {
        match self {
            Self::Top => Point { x: 0.0, y: -1.0 },
            Self::Right => Point { x: 1.0, y: 0.0 },
            Self::Bottom => Point { x: 0.0, y: 1.0 },
            Self::Left => Point { x: -1.0, y: 0.0 },
        }
    }
}

/// JavaScript `Math.round` (halves round toward +∞).
pub fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

pub fn center(b: &BoxF) -> Point {
    Point {
        x: b.x + b.w / 2.0,
        y: b.y + b.h / 2.0,
    }
}

/// Where the ray from the box's centre toward `toward` leaves the box.
pub fn exit_point(b: &BoxF, toward: Point) -> Point {
    let c = center(b);
    let (dx, dy) = (toward.x - c.x, toward.y - c.y);
    if dx == 0.0 && dy == 0.0 {
        return c;
    }
    let tx = if dx == 0.0 {
        f64::INFINITY
    } else {
        b.w / 2.0 / dx.abs()
    };
    let ty = if dy == 0.0 {
        f64::INFINITY
    } else {
        b.h / 2.0 / dy.abs()
    };
    let t = tx.min(ty);
    Point {
        x: c.x + dx * t,
        y: c.y + dy * t,
    }
}

/// The middle of one side of a box.
pub fn side_point(b: &BoxF, side: Side) -> Point {
    match side {
        Side::Top => Point {
            x: b.x + b.w / 2.0,
            y: b.y,
        },
        Side::Right => Point {
            x: b.x + b.w,
            y: b.y + b.h / 2.0,
        },
        Side::Bottom => Point {
            x: b.x + b.w / 2.0,
            y: b.y + b.h,
        },
        Side::Left => Point {
            x: b.x,
            y: b.y + b.h / 2.0,
        },
    }
}

/// `inner` lies entirely inside `outer`.
pub fn contains_box(outer: &BoxF, inner: &BoxF) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.w <= outer.x + outer.w
        && inner.y + inner.h <= outer.y + outer.h
}

/// Two boxes closer than `gap` on both axes.
pub fn boxes_overlap(a: &BoxF, b: &BoxF, gap: f64) -> bool {
    a.x < b.x + b.w + gap && b.x < a.x + a.w + gap && a.y < b.y + b.h + gap && b.y < a.y + a.h + gap
}

/// Centre-to-centre segment between two boxes, clipped to both borders (`None` when they
/// overlap).
fn box_segment(a: &BoxF, b: &BoxF) -> Option<(Point, Point)> {
    let (ca, cb) = (center(a), center(b));
    if (ca.x - cb.x).abs() < f64::midpoint(a.w, b.w)
        && (ca.y - cb.y).abs() < f64::midpoint(a.h, b.h)
    {
        return None;
    }
    Some((exit_point(a, cb), exit_point(b, ca)))
}

/// An arrow end resolved against the current boxes.
#[derive(Debug, Clone, Copy)]
pub enum End {
    /// A free point.
    Point(Point),
    /// A shape's box, optionally pinned to a side.
    Box(BoxF, Option<Side>),
}

/// The drawn path of an arrow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArrowPath {
    pub p: Point,
    pub q: Point,
    /// Where the label sits.
    pub mid: Point,
    /// Bezier control points when an end is pinned to a side.
    pub curve: Option<(Point, Point)>,
}

fn cubic_at(p0: Point, c1: Point, c2: Point, p3: Point, t: f64) -> Point {
    let u = 1.0 - t;
    Point {
        x: u * u * u * p0.x + 3.0 * u * u * t * c1.x + 3.0 * u * t * t * c2.x + t * t * t * p3.x,
        y: u * u * u * p0.y + 3.0 * u * u * t * c1.y + 3.0 * u * t * t * c2.y + t * t * t * p3.y,
    }
}

/// The path of an arrow between two resolved ends; `None` when there is no line to draw.
#[allow(clippy::many_single_char_names)]
pub fn arrow_path(a: End, b: End) -> Option<ArrowPath> {
    let point_of = |e: &End| match *e {
        End::Point(p) => Some(p),
        End::Box(bx, Some(side)) => Some(side_point(&bx, side)),
        End::Box(_, None) => None,
    };
    let (pa, pb) = (point_of(&a), point_of(&b));
    let (p, q) = match (pa, pb, a, b) {
        (Some(p), Some(q), _, _) => (p, q),
        (Some(p), None, _, End::Box(bb, _)) => (p, exit_point(&bb, p)),
        (None, Some(q), End::Box(ba, _), _) => (exit_point(&ba, q), q),
        (None, None, End::Box(ba, _), End::Box(bb, _)) => box_segment(&ba, &bb)?,
        _ => return None,
    };
    let len = (q.x - p.x).hypot(q.y - p.y);
    if len < 1.0 {
        return None;
    }
    let side_of = |e: &End| match *e {
        End::Box(_, s) => s,
        End::Point(_) => None,
    };
    let (sa, sb) = (side_of(&a), side_of(&b));
    if sa.is_none() && sb.is_none() {
        return Some(ArrowPath {
            p,
            q,
            mid: Point {
                x: f64::midpoint(p.x, q.x),
                y: f64::midpoint(p.y, q.y),
            },
            curve: None,
        });
    }
    let reach = (len / 2.2).clamp(24.0, 160.0);
    let towards = |from: Point, to: Point| {
        let d = (to.x - from.x).hypot(to.y - from.y);
        let d = if d == 0.0 { 1.0 } else { d };
        Point {
            x: (to.x - from.x) / d,
            y: (to.y - from.y) / d,
        }
    };
    let na = sa.map_or_else(|| towards(p, q), Side::normal);
    let nb = sb.map_or_else(|| towards(q, p), Side::normal);
    let c1 = Point {
        x: p.x + na.x * reach,
        y: p.y + na.y * reach,
    };
    let c2 = Point {
        x: q.x + nb.x * reach,
        y: q.y + nb.y * reach,
    };
    Some(ArrowPath {
        p,
        q,
        mid: cubic_at(p, c1, c2, q, 0.5),
        curve: Some((c1, c2)),
    })
}

/// Bounding box of an arrow's path (curves sampled, as the page does).
pub fn arrow_box(path: &ArrowPath) -> BoxF {
    let mut xs = vec![path.p.x, path.q.x];
    let mut ys = vec![path.p.y, path.q.y];
    if let Some((c1, c2)) = path.curve {
        for i in 1..8 {
            let pt = cubic_at(path.p, c1, c2, path.q, f64::from(i) / 8.0);
            xs.push(pt.x);
            ys.push(pt.y);
        }
    }
    let min = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = |v: &[f64]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (x, y) = (min(&xs), min(&ys));
    BoxF {
        x,
        y,
        w: max(&xs) - x,
        h: max(&ys) - y,
    }
}

/// Clear space kept around an auto-placed shape.
pub const PLACEMENT_GAP: f64 = 24.0;
/// Grid step of the placement search.
pub const PLACEMENT_STEP: f64 = 32.0;
/// The search gives up this far from the centre (and uses the centre).
pub const PLACEMENT_MAX_RADIUS: f64 = 6000.0;

/// Top-left of a `w`×`h` box near `around` that overlaps none of `taken` (with
/// [`PLACEMENT_GAP`]): the centred spot if free, else the nearest free spot ring by ring on
/// a [`PLACEMENT_STEP`] grid, ties going right, then down, then left, then up — the same
/// walk as the page's `findFreeSpot`.
pub fn find_free_spot(taken: &[BoxF], w: f64, h: f64, around: Point) -> Point {
    let (w, h) = (w.max(1.0), h.max(1.0));
    let origin = Point {
        x: js_round(around.x - w / 2.0),
        y: js_round(around.y - h / 2.0),
    };
    let free_in = |set: &[BoxF], p: Point| {
        let b = BoxF {
            x: p.x,
            y: p.y,
            w,
            h,
        };
        !set.iter().any(|t| boxes_overlap(&b, t, PLACEMENT_GAP))
    };
    if free_in(taken, origin) {
        return origin;
    }
    let reach = PLACEMENT_MAX_RADIUS + w.max(h);
    let near: Vec<BoxF> = taken
        .iter()
        .filter(|t| {
            t.x < origin.x + w + reach
                && t.x + t.w > origin.x - reach
                && t.y < origin.y + h + reach
                && t.y + t.h > origin.y - reach
        })
        .copied()
        .collect();
    #[allow(clippy::cast_possible_truncation)]
    let rings = (PLACEMENT_MAX_RADIUS / PLACEMENT_STEP).ceil() as i32;
    let mut cells: Vec<(Point, f64, f64)> = Vec::new();
    for k in 1..=rings {
        cells.clear();
        for i in -k..=k {
            for j in -k..=k {
                if i.abs().max(j.abs()) != k {
                    continue;
                }
                let dx = f64::from(i) * PLACEMENT_STEP;
                let dy = f64::from(j) * PLACEMENT_STEP;
                let mut a = dy.atan2(dx);
                if a < 0.0 {
                    a += std::f64::consts::TAU;
                }
                cells.push((
                    Point {
                        x: origin.x + dx,
                        y: origin.y + dy,
                    },
                    dx.hypot(dy),
                    a,
                ));
            }
        }
        cells.sort_by(|m, n| m.1.total_cmp(&n.1).then(m.2.total_cmp(&n.2)));
        if let Some((p, _, _)) = cells.iter().find(|c| free_in(&near, c.0)) {
            return *p;
        }
    }
    origin
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    const B: BoxF = BoxF {
        x: 0.0,
        y: 0.0,
        w: 100.0,
        h: 100.0,
    };

    #[test]
    fn rounding_matches_js() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(-2.6), -3.0);
        assert_eq!(js_round(1.2), 1.0);
    }

    #[test]
    fn straight_arrow_between_boxes() {
        let other = BoxF { x: 300.0, ..B };
        let path = arrow_path(End::Box(B, None), End::Box(other, None)).unwrap();
        assert_eq!(path.p, Point { x: 100.0, y: 50.0 });
        assert_eq!(path.q, Point { x: 300.0, y: 50.0 });
        assert_eq!(path.mid, Point { x: 200.0, y: 50.0 });
        let bx = arrow_box(&path);
        assert_eq!((bx.x, bx.w, bx.h), (100.0, 200.0, 0.0));
        // Overlapping boxes have no line.
        assert!(arrow_path(End::Box(B, None), End::Box(BoxF { x: 50.0, ..B }, None)).is_none());
    }

    #[test]
    fn side_pinned_arrow_curves() {
        let other = BoxF { x: 300.0, ..B };
        let path = arrow_path(
            End::Box(B, Some(Side::Bottom)),
            End::Box(other, Some(Side::Top)),
        )
        .unwrap();
        assert_eq!(path.p, Point { x: 50.0, y: 100.0 });
        assert_eq!(path.q, Point { x: 350.0, y: 0.0 });
        assert!(path.curve.is_some());
        let bx = arrow_box(&path);
        assert!(bx.h > 100.0, "curve overshoots the chord");
    }

    #[test]
    fn point_to_box() {
        let path = arrow_path(End::Point(Point { x: -100.0, y: 50.0 }), End::Box(B, None)).unwrap();
        assert_eq!(path.q, Point { x: 0.0, y: 50.0 });
    }

    #[test]
    fn placement() {
        assert_eq!(
            find_free_spot(&[], 200.0, 200.0, Point { x: 0.0, y: 0.0 }),
            Point {
                x: -100.0,
                y: -100.0
            }
        );
        let taken = [BoxF {
            x: -100.0,
            y: -100.0,
            w: 200.0,
            h: 200.0,
        }];
        let p = find_free_spot(&taken, 200.0, 200.0, Point { x: 0.0, y: 0.0 });
        let b = BoxF {
            x: p.x,
            y: p.y,
            w: 200.0,
            h: 200.0,
        };
        assert!(!boxes_overlap(&b, &taken[0], PLACEMENT_GAP));
        // Ties go right first.
        assert!(p.x > 0.0 && p.y == -100.0, "{p:?}");
    }
}
