//! Geometry helpers for focus movement and pointer-driven edits.

use crate::{Dir, Edges, Point, Rect, Side, Size, WinId};

/// The window to focus when moving from `from` in `dir`: strictly beyond the source's
/// edge and overlapping it on the other axis. Nearest wins, then the larger overlap,
/// then the most recently focused (`mru` is most recent first).
pub fn neighbor(rects: &[(WinId, Rect)], from: WinId, dir: Dir, mru: &[WinId]) -> Option<WinId> {
    let src = rects.iter().find(|(id, _)| *id == from)?.1;
    let rank = |id: WinId| mru.iter().position(|&m| m == id).unwrap_or(usize::MAX);
    rects
        .iter()
        .filter(|(id, _)| *id != from)
        .filter_map(|&(id, r)| {
            let (dist, overlap) = match dir {
                Dir::Left => (
                    src.x - r.right(),
                    span(src.y, src.bottom(), r.y, r.bottom()),
                ),
                Dir::Right => (
                    r.x - src.right(),
                    span(src.y, src.bottom(), r.y, r.bottom()),
                ),
                Dir::Up => (src.y - r.bottom(), span(src.x, src.right(), r.x, r.right())),
                Dir::Down => (r.y - src.bottom(), span(src.x, src.right(), r.x, r.right())),
            };
            (dist >= 0 && overlap > 0).then_some((id, dist, overlap))
        })
        .min_by_key(|&(id, dist, overlap)| (dist, std::cmp::Reverse(overlap), rank(id)))
        .map(|(id, ..)| id)
}

fn span(a0: i32, a1: i32, b0: i32, b1: i32) -> i32 {
    a1.min(b1) - a0.max(b0)
}

/// Which corner-ish edges a press at `p` grabs: the nearest horizontal and vertical edge.
pub fn edges_for_point(r: Rect, p: Point) -> Edges {
    let c = r.center();
    let h = if p.x < c.x { Edges::LEFT } else { Edges::RIGHT };
    let v = if p.y < c.y { Edges::TOP } else { Edges::BOTTOM };
    h | v
}

/// Which side of `r` a window dropped at `p` lands on, along the axis a new split of
/// `r` would use (wide boxes split left/right, tall ones top/bottom).
pub fn drop_side(r: Rect, p: Point) -> Side {
    let c = r.center();
    let first = if r.h > r.w { p.y < c.y } else { p.x < c.x };
    if first { Side::First } else { Side::Second }
}

/// The rectangle after dragging `edges` of `start` by the total pointer movement `(dx, dy)`.
/// Edges not being dragged stay put, also when a size limit stops the drag. `min` and `max`
/// are outer sizes; 0 means unbounded (max) and at least one pixel (min).
pub fn resize_rect(start: Rect, edges: Edges, dx: i32, dy: i32, min: Size, max: Size) -> Rect {
    let axis = |pos: i32, len: i32, lo: bool, hi: bool, d: i32, min: i32, max: i32| {
        let want = if hi {
            len + d
        } else if lo {
            len - d
        } else {
            len
        };
        let min = min.max(1);
        let mut l = want.max(min);
        if max > 0 {
            l = l.min(max.max(min));
        }
        (if lo && !hi { pos + len - l } else { pos }, l)
    };
    let (x, w) = axis(
        start.x,
        start.w,
        edges.contains(Edges::LEFT),
        edges.contains(Edges::RIGHT),
        dx,
        min.w,
        max.w,
    );
    let (y, h) = axis(
        start.y,
        start.h,
        edges.contains(Edges::TOP),
        edges.contains(Edges::BOTTOM),
        dy,
        min.h,
        max.h,
    );
    Rect { x, y, w, h }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: u64) -> WinId {
        WinId(n)
    }

    /// 1 | 2 over 3 on the right; 4 far below 1.
    fn scene() -> Vec<(WinId, Rect)> {
        vec![
            (w(1), Rect::new(0, 0, 100, 200)),
            (w(2), Rect::new(100, 0, 100, 100)),
            (w(3), Rect::new(100, 100, 100, 100)),
            (w(4), Rect::new(0, 300, 100, 100)),
        ]
    }

    #[test]
    fn neighbor_needs_perpendicular_overlap() {
        let s = scene();
        assert_eq!(neighbor(&s, w(1), Dir::Down, &[]), Some(w(4)));
        assert_eq!(neighbor(&s, w(2), Dir::Down, &[]), Some(w(3)));
        assert_eq!(neighbor(&s, w(3), Dir::Up, &[]), Some(w(2)));
        assert_eq!(neighbor(&s, w(2), Dir::Left, &[]), Some(w(1)));
        assert_eq!(neighbor(&s, w(4), Dir::Right, &[]), None);
        assert_eq!(neighbor(&s, w(1), Dir::Left, &[]), None);
        assert_eq!(neighbor(&s, w(9), Dir::Left, &[]), None);
    }

    #[test]
    fn neighbor_tie_breaks_overlap_then_mru() {
        let s = scene();
        // From 1 going right, 2 and 3 are equally near with equal overlap: MRU decides.
        assert_eq!(neighbor(&s, w(1), Dir::Right, &[w(3), w(2)]), Some(w(3)));
        assert_eq!(neighbor(&s, w(1), Dir::Right, &[w(2)]), Some(w(2)));
        // A window overlapping more beats MRU.
        let s = vec![
            (w(1), Rect::new(0, 0, 100, 100)),
            (w(2), Rect::new(100, 0, 100, 80)),
            (w(3), Rect::new(100, 80, 100, 100)),
        ];
        assert_eq!(neighbor(&s, w(1), Dir::Right, &[w(3)]), Some(w(2)));
    }

    #[test]
    fn edges_and_drop_side() {
        let r = Rect::new(0, 0, 100, 100);
        assert_eq!(
            edges_for_point(r, Point { x: 10, y: 90 }),
            Edges::LEFT | Edges::BOTTOM
        );
        assert_eq!(
            edges_for_point(r, Point { x: 90, y: 10 }),
            Edges::RIGHT | Edges::TOP
        );
        assert_eq!(
            drop_side(Rect::new(0, 0, 200, 100), Point { x: 20, y: 90 }),
            Side::First
        );
        assert_eq!(
            drop_side(Rect::new(0, 0, 100, 200), Point { x: 20, y: 190 }),
            Side::Second
        );
    }

    #[test]
    fn resize_keeps_the_opposite_edge() {
        let r = Rect::new(100, 100, 200, 200);
        let none = Size::default();
        let min = Size { w: 50, h: 50 };
        // Dragging the left edge right shrinks from the left; the right edge stays.
        let a = resize_rect(r, Edges::LEFT | Edges::BOTTOM, 40, 30, none, none);
        assert_eq!(a, Rect::new(140, 100, 160, 230));
        // Past the minimum the right edge still does not move.
        let b = resize_rect(r, Edges::LEFT | Edges::TOP, 500, 500, min, none);
        assert_eq!((b.right(), b.bottom(), b.w, b.h), (300, 300, 50, 50));
        // The maximum stops growth, again anchored on the far edge.
        let c = resize_rect(r, Edges::LEFT, -500, 0, none, Size { w: 250, h: 0 });
        assert_eq!((c.right(), c.w), (300, 250));
    }
}
