//! Boundary geometry (design §3). Pure functions over display rectangles.
//!
//! Conventions:
//! - `Rect` is half-open in the design sense: a display covers columns
//!   `x..x+w` and rows `y..y+h`. Internally all arithmetic is `i64` so that
//!   `x + w` and `p + 1` cannot overflow at `i32` extremes.
//! - `exit_frac` is evaluated on the sending machine, with `edge` being the
//!   edge it leaves through.
//! - `entry_point` is evaluated on the receiving machine, with `edge` being the
//!   receiver's own edge through which the cursor enters (for a sender that
//!   exits Right, the receiver is entered through its Left edge).

use crate::proto::{Edge, Point, Rect};

/// Entry point is pushed this many px inside the receiving display (§3).
pub const ENTRY_INSET: i32 = 24;
/// Corner dead zone radius (§3).
pub const CORNER_R: i64 = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeomError {
    NoDisplays,
    EmptyRect,
    NoOuterSegment,
}

pub fn validate_displays(displays: &[Rect]) -> Result<(), GeomError> {
    if displays.is_empty() {
        return Err(GeomError::NoDisplays);
    }
    if displays.iter().any(|r| r.w <= 0 || r.h <= 0) {
        return Err(GeomError::EmptyRect);
    }
    Ok(())
}

fn right(r: &Rect) -> i64 {
    r.x as i64 + r.w as i64
}

fn bottom(r: &Rect) -> i64 {
    r.y as i64 + r.h as i64
}

pub fn contains(r: &Rect, p: Point) -> bool {
    contains_xy(r, p.x as i64, p.y as i64)
}

fn contains_xy(r: &Rect, x: i64, y: i64) -> bool {
    r.x as i64 <= x && x < right(r) && r.y as i64 <= y && y < bottom(r)
}

fn any_contains(displays: &[Rect], x: i64, y: i64) -> bool {
    displays.iter().any(|r| contains_xy(r, x, y))
}

/// (dx, dy) one step outward through `edge`.
fn step(edge: Edge) -> (i64, i64) {
    match edge {
        Edge::Left => (-1, 0),
        Edge::Right => (1, 0),
        Edge::Top => (0, -1),
        Edge::Bottom => (0, 1),
    }
}

/// Coordinate across the boundary (the boundary's own axis value).
fn perp(edge: Edge, x: i64, y: i64) -> i64 {
    match edge {
        Edge::Left | Edge::Right => x,
        Edge::Top | Edge::Bottom => y,
    }
}

/// Coordinate along the boundary.
fn along(edge: Edge, x: i64, y: i64) -> i64 {
    match edge {
        Edge::Left | Edge::Right => y,
        Edge::Top | Edge::Bottom => x,
    }
}

/// The boundary line a display presents on `edge`: its outermost cell on that side.
fn line_of(r: &Rect, edge: Edge) -> i64 {
    match edge {
        Edge::Left => r.x as i64,
        Edge::Right => right(r) - 1,
        Edge::Top => r.y as i64,
        Edge::Bottom => bottom(r) - 1,
    }
}

/// Extent of a display along the boundary.
fn span(r: &Rect, edge: Edge) -> (i64, i64) {
    match edge {
        Edge::Left | Edge::Right => (r.y as i64, bottom(r)),
        Edge::Top | Edge::Bottom => (r.x as i64, right(r)),
    }
}

/// Does `q` sit on the far side of boundary line `line`, i.e. cover the cell
/// one step outward from it? Such a display makes that stretch an interior seam.
fn covers_outside(q: &Rect, edge: Edge, line: i64) -> bool {
    let (dx, dy) = step(edge);
    let o = line + perp(edge, dx, dy);
    match edge {
        Edge::Left | Edge::Right => q.x as i64 <= o && o < right(q),
        Edge::Top | Edge::Bottom => q.y as i64 <= o && o < bottom(q),
    }
}

fn merge(mut v: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    v.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (s, e) in v {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// Remove the covered intervals from `seg`. `covers` must be sorted and merged.
fn subtract(seg: (i64, i64), covers: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let (s, e) = seg;
    let mut out = Vec::new();
    let mut cur = s;
    for &(cs, ce) in covers {
        if ce <= cur || cs >= e {
            continue;
        }
        if cs > cur {
            out.push((cur, cs));
        }
        cur = cur.max(ce);
        if cur >= e {
            break;
        }
    }
    if cur < e {
        out.push((cur, e));
    }
    out
}

/// Outer (non-seam) pieces of the boundary on line `line`.
fn outer_pieces(displays: &[Rect], edge: Edge, line: i64) -> Vec<(i64, i64)> {
    let aligned = merge(
        displays
            .iter()
            .filter(|r| line_of(r, edge) == line)
            .map(|r| span(r, edge))
            .collect(),
    );
    let covers = merge(
        displays
            .iter()
            .filter(|q| covers_outside(q, edge, line))
            .map(|q| span(q, edge))
            .collect(),
    );
    aligned
        .into_iter()
        .flat_map(|seg| subtract(seg, &covers))
        .collect()
}

/// Map an along-boundary offset to a fraction in 0..=65535 over `s..e`.
fn frac_of(s: i64, e: i64, a: i64) -> u16 {
    let len = e - s;
    if len <= 1 {
        return 0;
    }
    ((a - s) * 65535 / (len - 1)).clamp(0, 65535) as u16
}

/// Inverse of `frac_of`, rounded to the nearest pixel.
fn offset_of(s: i64, e: i64, frac: u16) -> i64 {
    let len = e - s;
    if len <= 1 {
        return s;
    }
    s + (frac as i64 * (len - 1) + 32767) / 65535
}

/// If the cursor at `p` leaves the display set through `edge`, return the
/// fraction along that outer boundary segment. Returns `None` when there is
/// no switch: `p` is not on the edge, the cell beyond is another display
/// (interior seam), or `p` sits in the corner dead zone.
pub fn exit_frac(displays: &[Rect], p: Point, edge: Edge) -> Option<u16> {
    let (x, y) = (p.x as i64, p.y as i64);
    if !any_contains(displays, x, y) {
        return None;
    }
    let (dx, dy) = step(edge);
    if any_contains(displays, x + dx, y + dy) {
        return None;
    }
    let line = perp(edge, x, y);
    let a = along(edge, x, y);
    let (s, e) = outer_pieces(displays, edge, line)
        .into_iter()
        .find(|&(s, e)| s <= a && a < e)?;

    // Corner: within CORNER_R of a segment end that has no neighbor continuing past it.
    let beyond = |pos: i64| {
        let (px, py) = match edge {
            Edge::Left | Edge::Right => (x, pos),
            Edge::Top | Edge::Bottom => (pos, y),
        };
        any_contains(displays, px, py)
    };
    let near_start = a - s < CORNER_R && !beyond(s - 1);
    let near_end = e - 1 - a < CORNER_R && !beyond(e);
    if near_start || near_end {
        return None;
    }
    Some(frac_of(s, e, a))
}

/// Is `p` inside the exit strip of `edge`: within `strip_px` of the outer boundary of the display
/// holding it, on a stretch that really leaves the display set (not an interior seam or a corner
/// dead zone, same rule as `exit_frac`). This is the dwell zone for the edge switch.
pub fn in_exit_strip(displays: &[Rect], p: Point, edge: Edge, strip_px: i32) -> bool {
    if strip_px <= 0 {
        return false;
    }
    let (x, y) = (p.x as i64, p.y as i64);
    let Some(r) = displays.iter().find(|r| contains_xy(r, x, y)) else {
        return false;
    };
    let line = line_of(r, edge);
    let depth = (perp(edge, x, y) - line).abs();
    if depth >= strip_px as i64 {
        return false;
    }
    let (qx, qy) = match edge {
        Edge::Left | Edge::Right => (line, y),
        Edge::Top | Edge::Bottom => (x, line),
    };
    exit_frac(displays, Point { x: qx as i32, y: qy as i32 }, edge).is_some()
}

/// Where the cursor lands on the receiving side when it enters through `edge`.
pub fn entry_point(displays: &[Rect], edge: Edge, frac: u16) -> Result<Point, GeomError> {
    validate_displays(displays)?;
    let mut lines: Vec<i64> = displays.iter().map(|r| line_of(r, edge)).collect();
    lines.sort_unstable();
    lines.dedup();

    // Pick the longest outer segment; ties go to the lowest line, then start.
    // ponytail: longest outer piece wins because Enter carries only frac, no
    // origin y; add a sender-side y hint if multi-segment layouts need it.
    let mut best: Option<(i64, i64, i64)> = None; // (len, line, start)
    let mut best_seg = (0i64, 0i64);
    for &line in &lines {
        for (s, e) in outer_pieces(displays, edge, line) {
            let len = e - s;
            let better = match best {
                None => true,
                Some((bl, bline, bs)) => len > bl || (len == bl && (line, s) < (bline, bs)),
            };
            if better {
                best = Some((len, line, s));
                best_seg = (s, e);
            }
        }
    }
    let (_, line, _) = best.ok_or(GeomError::NoOuterSegment)?;
    let pos = offset_of(best_seg.0, best_seg.1, frac);
    let inset = ENTRY_INSET as i64;
    let (x, y) = match edge {
        Edge::Left => (line + inset, pos),
        Edge::Right => (line - inset, pos),
        Edge::Top => (pos, line + inset),
        Edge::Bottom => (pos, line - inset),
    };
    if any_contains(displays, x, y) {
        return Ok(Point {
            x: x as i32,
            y: y as i32,
        });
    }
    Ok(clamp_to_nearest(displays, x, y))
}

/// `p` itself when a display holds it; otherwise the nearest point inside a display's inset rectangle.
/// `None` when there are no displays at all.
pub fn clamp_inside(displays: &[Rect], p: Point) -> Option<Point> {
    if displays.is_empty() {
        return None;
    }
    if displays.iter().any(|r| contains(r, p)) {
        return Some(p);
    }
    Some(clamp_to_nearest(displays, p.x as i64, p.y as i64))
}

/// `p` itself when a display holds it, else the nearest point on a display (edges included, no inset):
/// pushing past the edge leaves the cursor on the edge, where the return strip is. `p` when there are no displays.
pub fn clamp_to_displays(displays: &[Rect], p: Point) -> Point {
    let mut best: Option<(i64, Point)> = None;
    for r in displays {
        let x = (p.x as i64).clamp(r.x as i64, r.x as i64 + r.w as i64 - 1);
        let y = (p.y as i64).clamp(r.y as i64, r.y as i64 + r.h as i64 - 1);
        let d = (x - p.x as i64).pow(2) + (y - p.y as i64).pow(2);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, Point { x: x as i32, y: y as i32 }));
        }
    }
    best.map_or(p, |(_, q)| q)
}

/// Clamp into the nearest display's inset rectangle (display itself if the
/// inset would be empty). Nearest by squared distance; ties go to input order.
fn clamp_to_nearest(displays: &[Rect], x: i64, y: i64) -> Point {
    let inset = ENTRY_INSET as i64;
    let mut best: Option<(i64, Point)> = None;
    for r in displays {
        let (mut x0, mut y0) = (r.x as i64 + inset, r.y as i64 + inset);
        let (mut x1, mut y1) = (right(r) - 1 - inset, bottom(r) - 1 - inset);
        if x0 > x1 || y0 > y1 {
            x0 = r.x as i64;
            y0 = r.y as i64;
            x1 = right(r) - 1;
            y1 = bottom(r) - 1;
        }
        let cx = x.clamp(x0, x1);
        let cy = y.clamp(y0, y1);
        let d = (cx - x).pow(2) + (cy - y).pow(2);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((
                d,
                Point {
                    x: cx as i32,
                    y: cy as i32,
                },
            ));
        }
    }
    best.map(|(_, p)| p).expect("validate_displays ensures non-empty")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    fn p(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    #[test]
    fn rect_is_half_open() {
        let d = r(0, 0, 1920, 1080);
        assert!(contains(&d, p(0, 0)));
        assert!(contains(&d, p(1919, 1079)));
        assert!(!contains(&d, p(1920, 0)));
        assert!(!contains(&d, p(0, 1080)));
        assert!(!contains(&d, p(-1, 0)));
    }

    #[test]
    fn internal_seam_is_not_an_exit() {
        let displays = [r(0, 0, 100, 100), r(100, 0, 100, 100)];
        assert_eq!(exit_frac(&displays, p(99, 50), Edge::Right), None);
        // The right display's own outer right edge is a real exit.
        assert!(exit_frac(&displays, p(199, 50), Edge::Right).is_some());
    }

    #[test]
    fn exit_frac_uses_the_outer_piece_not_the_whole_aligned_span() {
        // A's right edge at x=99 is partly a seam against C (y 50..150).
        // Only y 0..50 is outer, so the fraction is measured over that piece.
        let displays = [r(0, 0, 100, 100), r(100, 50, 100, 100)];
        assert_eq!(exit_frac(&displays, p(99, 20), Edge::Right), Some((20 * 65535 / 49) as u16));
        assert_eq!(exit_frac(&displays, p(99, 60), Edge::Right), None, "seam against C");
    }

    #[test]
    fn exit_on_b_uses_bs_segment() {
        let displays = [r(0, 0, 100, 100), r(100, 0, 100, 100)];
        // B's segment is y 0..100 (len 100): frac of y=50 is 50*65535/99.
        assert_eq!(exit_frac(&displays, p(199, 50), Edge::Right), Some((50 * 65535 / 99) as u16));
    }

    #[test]
    fn negative_origin_second_monitor_exits_on_its_left() {
        let primary = r(0, 0, 1920, 1080);
        let second = r(-1920, -1080, 1920, 1080);
        let displays = [primary, second];
        assert!(exit_frac(&displays, p(-1920, -500), Edge::Left).is_some());
        // Row -500 does not reach the primary, so the second's right edge is outer.
        assert!(exit_frac(&displays, p(-1, -500), Edge::Right).is_some());
    }

    #[test]
    fn negative_origin_overlap_is_a_seam_only_where_both_cover() {
        let primary = r(0, 0, 1920, 1080);
        let second = r(-1920, -540, 1920, 1080); // rows -540..540
        let displays = [primary, second];
        assert_eq!(exit_frac(&displays, p(-1, 100), Edge::Right), None, "seam into primary");
        // Row -300 is outside primary, so it exits. Its outer piece is -540..0.
        assert_eq!(
            exit_frac(&displays, p(-1, -300), Edge::Right),
            Some((240 * 65535 / 539) as u16)
        );
    }

    #[test]
    fn corner_dead_zone_suppresses_exit_when_no_neighbor_continues() {
        let d = [r(0, 0, 1920, 1080)];
        assert_eq!(exit_frac(&d, p(1919, 3), Edge::Right), None, "top corner");
        assert_eq!(exit_frac(&d, p(1919, 1076), Edge::Right), None, "bottom corner");
        assert!(exit_frac(&d, p(1919, 20), Edge::Right).is_some(), "just outside corner");
    }

    #[test]
    fn stacked_neighbor_makes_the_corner_a_normal_edge() {
        // A display stacked above continues the right boundary, so y=3 is not a corner.
        let d = [r(0, 0, 1920, 1080), r(0, -1080, 1920, 1080)];
        assert!(exit_frac(&d, p(1919, 3), Edge::Right).is_some());
    }

    #[test]
    fn top_and_bottom_exits() {
        let d = [r(0, 0, 1920, 1080)];
        assert!(exit_frac(&d, p(500, 0), Edge::Top).is_some());
        assert!(exit_frac(&d, p(500, 1079), Edge::Bottom).is_some());
        assert_eq!(exit_frac(&d, p(500, 1), Edge::Top), None, "not on the edge");
    }

    #[test]
    fn point_outside_all_displays_never_exits() {
        let d = [r(0, 0, 100, 100)];
        assert_eq!(exit_frac(&d, p(100, 50), Edge::Right), None);
    }

    #[test]
    fn exit_entry_round_trip_is_within_one_pixel() {
        let sender = [r(0, 0, 1920, 1080)];
        let receiver = [r(0, 0, 1920, 1080)];
        for y in 16..1064 {
            let f = exit_frac(&sender, p(1919, y), Edge::Right).expect("non-corner row");
            let back = entry_point(&receiver, Edge::Left, f).unwrap();
            assert!((back.y - y).abs() <= 1, "y={y} came back as {}", back.y);
        }
    }

    #[test]
    fn entry_lands_inset_from_the_boundary() {
        let q = [r(0, 0, 1920, 1080)];
        assert_eq!(entry_point(&q, Edge::Left, 0).unwrap(), p(ENTRY_INSET, 0));
        assert_eq!(entry_point(&q, Edge::Left, 65535).unwrap(), p(ENTRY_INSET, 1079));
        assert_eq!(entry_point(&q, Edge::Right, 0).unwrap(), p(1919 - ENTRY_INSET, 0));
        assert_eq!(entry_point(&q, Edge::Top, 0).unwrap(), p(0, ENTRY_INSET));
        assert_eq!(entry_point(&q, Edge::Bottom, 0).unwrap(), p(0, 1079 - ENTRY_INSET));
    }

    #[test]
    fn entry_on_a_seam_is_avoided() {
        // Primary sits right of the second monitor. The primary's left edge is a seam,
        // so the entry must use the second monitor's outer left edge.
        let q = [r(0, 0, 1920, 1080), r(-1920, 0, 1920, 1080)];
        assert_eq!(entry_point(&q, Edge::Left, 0).unwrap(), p(-1920 + ENTRY_INSET, 0));
    }

    #[test]
    fn entry_into_a_narrow_display_clamps_to_its_rect() {
        // Width 10 is smaller than the inset, so the inset rect is empty and
        // the clamp falls back to the display rect itself.
        let q = [r(0, 0, 10, 100)];
        assert_eq!(entry_point(&q, Edge::Left, 0).unwrap(), p(9, 0));
    }

    #[test]
    fn entry_fails_loud_without_displays_or_with_empty_rect() {
        assert_eq!(entry_point(&[], Edge::Left, 0), Err(GeomError::NoDisplays));
        assert_eq!(
            entry_point(&[r(0, 0, 0, 10)], Edge::Left, 0),
            Err(GeomError::EmptyRect)
        );
    }

    #[test]
    fn extreme_coordinates_do_not_overflow() {
        // x + w reaches i32::MAX exactly; the half-open edge must still hold.
        let d = [r(i32::MAX - 100, -5, 100, 10)];
        assert!(contains(&d[0], p(i32::MAX - 1, 0)));
        assert!(!contains(&d[0], p(i32::MAX, 0)));
        let _ = exit_frac(&d, p(i32::MAX - 1, 0), Edge::Right);
        let _ = entry_point(&d, Edge::Right, 65535);
    }

    #[test]
    fn strip_covers_each_of_four_edges_and_stops_at_depth() {
        let d = [r(0, 0, 1000, 800)];
        // Mid-edge, on the outermost pixel and strip_px-1 inside: in. strip_px inside: out.
        assert!(in_exit_strip(&d, p(0, 400), Edge::Left, 2));
        assert!(in_exit_strip(&d, p(1, 400), Edge::Left, 2));
        assert!(!in_exit_strip(&d, p(2, 400), Edge::Left, 2));
        assert!(in_exit_strip(&d, p(999, 400), Edge::Right, 2));
        assert!(in_exit_strip(&d, p(998, 400), Edge::Right, 2));
        assert!(!in_exit_strip(&d, p(997, 400), Edge::Right, 2));
        assert!(in_exit_strip(&d, p(500, 0), Edge::Top, 2));
        assert!(in_exit_strip(&d, p(500, 1), Edge::Top, 2));
        assert!(!in_exit_strip(&d, p(500, 2), Edge::Top, 2));
        assert!(in_exit_strip(&d, p(500, 799), Edge::Bottom, 2));
        assert!(in_exit_strip(&d, p(500, 798), Edge::Bottom, 2));
        assert!(!in_exit_strip(&d, p(500, 797), Edge::Bottom, 2));
        // The strip belongs to its own edge only.
        assert!(!in_exit_strip(&d, p(0, 400), Edge::Right, 2));
        assert!(!in_exit_strip(&d, p(500, 0), Edge::Bottom, 2));
    }

    #[test]
    fn strip_excludes_corners_outside_and_nonpositive_width() {
        let d = [r(0, 0, 1000, 800)];
        assert!(!in_exit_strip(&d, p(0, 0), Edge::Left, 2), "corner dead zone");
        assert!(!in_exit_strip(&d, p(-1, 400), Edge::Left, 2), "outside every display");
        assert!(!in_exit_strip(&d, p(0, 400), Edge::Left, 0));
        assert!(!in_exit_strip(&[], p(0, 0), Edge::Left, 2));
    }

    #[test]
    fn strip_ignores_interior_seam_between_displays() {
        // B sits to the right of A: A's right edge is a seam, B's right edge is outer.
        let d = [r(0, 0, 1000, 800), r(1000, 0, 1000, 800)];
        assert!(!in_exit_strip(&d, p(999, 400), Edge::Right, 2), "seam");
        assert!(!in_exit_strip(&d, p(1000, 400), Edge::Left, 2), "seam from B side");
        assert!(in_exit_strip(&d, p(1999, 400), Edge::Right, 2));
        assert!(in_exit_strip(&d, p(0, 400), Edge::Left, 2));
    }

    #[test]
    fn strip_on_stacked_displays_with_offset() {
        // Top display narrower and offset; its right edge is outer where bottom does not extend.
        let d = [r(0, 0, 800, 600), r(0, 600, 1200, 600)];
        assert!(in_exit_strip(&d, p(799, 300), Edge::Right, 2));
        assert!(in_exit_strip(&d, p(1199, 900), Edge::Right, 2));
        assert!(!in_exit_strip(&d, p(0, 600), Edge::Top, 2), "seam between stacked displays");
    }

    #[test]
    fn clamp_picks_the_nearest_display_on_a_two_monitor_desktop() {
        let two = vec![r(0, 0, 1920, 1080), r(1920, 0, 1280, 720)];
        // Below the shorter right monitor: snaps up onto it, not across to the left one.
        assert_eq!(clamp_to_displays(&two, p(2500, 900)), p(2500, 719));
        assert_eq!(clamp_to_displays(&two, p(10, 10)), p(10, 10));
    }
}
