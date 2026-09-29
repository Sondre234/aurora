//! Hyprland-style dwindle: a binary split tree. Inserting splits the target leaf in
//! two; the split axis is decided once (from the target's box) and then stored.

use std::collections::HashMap;

use crate::tiling::{AxisCtl, InsertHint, ResizeHandle, TilingLayout};
use crate::{
    Axis, Constraints, Edges, Kind, LayoutParams, NewWindowSide, Placement, Rect, Side, WinId,
};

const NONE: usize = usize::MAX;
const MIN_RATIO: f32 = 0.05;

enum Body {
    Free,
    Leaf {
        win: WinId,
        constraints: Constraints,
    },
    Split {
        axis: Axis,
        ratio: f32,
        a: usize,
        b: usize,
    },
}

struct Node {
    parent: usize,
    /// The tree box: gaps and border are applied on top of it when placing.
    rect: Rect,
    /// Smallest box size that fits the subtree (bottom-up, valid after `relayout`).
    min: (i32, i32),
    body: Body,
}

pub struct Dwindle {
    nodes: Vec<Node>,
    free: Vec<usize>,
    root: usize,
    index: HashMap<WinId, usize>,
    last: Option<WinId>,
    area: Rect,
    params: LayoutParams,
    dirty: bool,
    epoch: u64,
    /// Bumped when the tree's shape changes; invalidates resize handles.
    shape: u64,
}

impl Default for Dwindle {
    fn default() -> Self {
        Self::new()
    }
}

impl Dwindle {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            free: Vec::new(),
            root: NONE,
            index: HashMap::new(),
            last: None,
            area: Rect::default(),
            params: LayoutParams::default(),
            dirty: false,
            epoch: 0,
            shape: 0,
        }
    }

    fn alloc(&mut self, node: Node) -> usize {
        match self.free.pop() {
            Some(i) => {
                self.nodes[i] = node;
                i
            }
            None => {
                self.nodes.push(node);
                self.nodes.len() - 1
            }
        }
    }

    fn release(&mut self, i: usize) {
        self.nodes[i].body = Body::Free;
        self.free.push(i);
    }

    fn touch(&mut self, structural: bool) {
        self.dirty = true;
        self.epoch += 1;
        if structural {
            self.shape += 1;
        }
    }

    /// Makes `new` take `old`'s place under `parent` (or as root).
    fn replace_child(&mut self, parent: usize, old: usize, new: usize) {
        if parent == NONE {
            self.root = new;
        } else if let Body::Split { a, b, .. } = &mut self.nodes[parent].body {
            if *a == old {
                *a = new;
            } else if *b == old {
                *b = new;
            }
        }
        self.nodes[new].parent = parent;
    }

    fn leftmost_leaf(&self) -> Option<usize> {
        let mut n = self.root;
        while n != NONE {
            match self.nodes[n].body {
                Body::Split { a, .. } => n = a,
                Body::Leaf { .. } => return Some(n),
                Body::Free => return None,
            }
        }
        None
    }

    fn pick_target(&self, hint: &InsertHint) -> Option<usize> {
        if let Some(&i) = hint.after.and_then(|id| self.index.get(&id)) {
            return Some(i);
        }
        if let Some(p) = hint.pointer
            && let Some(&i) = self
                .index
                .values()
                .find(|&&i| self.nodes[i].rect.contains(p))
        {
            return Some(i);
        }
        if let Some(&i) = self.last.and_then(|id| self.index.get(&id)) {
            return Some(i);
        }
        self.leftmost_leaf()
    }

    /// Splits leaf `target`, putting the already-allocated `leaf` on `first` or second.
    fn split_leaf(&mut self, target: usize, leaf: usize, first: bool, forced: Option<Axis>) {
        let trect = self.nodes[target].rect;
        let axis = if let Some(axis) = forced {
            axis
        } else if trect.h > trect.w {
            Axis::Vertical
        } else {
            Axis::Horizontal
        };
        let parent = self.nodes[target].parent;
        let (a, b) = if first {
            (leaf, target)
        } else {
            (target, leaf)
        };
        let split = self.alloc(Node {
            parent,
            rect: trect,
            min: (0, 0),
            body: Body::Split {
                axis,
                ratio: 0.5,
                a,
                b,
            },
        });
        self.replace_child(parent, target, split);
        self.nodes[a].parent = split;
        self.nodes[b].parent = split;
        // Seed boxes so consecutive inserts before a relayout still see sane shapes.
        let (ra, rb) = split_rect(trect, axis, 0.5, 0, 0);
        self.nodes[a].rect = ra;
        self.nodes[b].rect = rb;
    }

    fn leaf_placement(&self, i: usize) -> Option<Placement> {
        let Body::Leaf {
            win,
            constraints: c,
        } = self.nodes[i].body
        else {
            return None;
        };
        let (gi, border) = (self.params.gaps.inner, self.params.border);
        let avail = self.nodes[i].rect.shrink(gi).shrink(border);
        // Max size is soft: the window is centred in its box instead of stretched.
        let cw = if c.max.w > 0 {
            avail.w.min(c.max.w)
        } else {
            avail.w
        };
        let ch = if c.max.h > 0 {
            avail.h.min(c.max.h)
        } else {
            avail.h
        };
        let content = Rect::new(
            avail.x + (avail.w - cw) / 2,
            avail.y + (avail.h - ch) / 2,
            cw,
            ch,
        );
        Some(Placement {
            id: win,
            outer: content.shrink(-border),
            content,
            kind: Kind::Tiled,
        })
    }

    fn compute_min(&mut self, n: usize, overhead: i32) -> (i32, i32) {
        let min = match self.nodes[n].body {
            Body::Leaf { constraints: c, .. } => {
                (c.min.w.max(0) + overhead, c.min.h.max(0) + overhead)
            }
            Body::Split { axis, a, b, .. } => {
                let (ma, mb) = (self.compute_min(a, overhead), self.compute_min(b, overhead));
                match axis {
                    Axis::Horizontal => (ma.0 + mb.0, ma.1.max(mb.1)),
                    Axis::Vertical => (ma.0.max(mb.0), ma.1 + mb.1),
                }
            }
            Body::Free => (0, 0),
        };
        self.nodes[n].min = min;
        min
    }

    fn place(&mut self, n: usize, rect: Rect) {
        self.nodes[n].rect = rect;
        if let Body::Split { axis, ratio, a, b } = self.nodes[n].body {
            let (ma, mb) = (self.nodes[a].min, self.nodes[b].min);
            let (min_a, min_b) = match axis {
                Axis::Horizontal => (ma.0, mb.0),
                Axis::Vertical => (ma.1, mb.1),
            };
            let (ra, rb) = split_rect(rect, axis, ratio, min_a, min_b);
            self.place(a, ra);
            self.place(b, rb);
        }
    }

    fn collect(&self, n: usize, out: &mut Vec<Placement>) {
        match self.nodes[n].body {
            Body::Leaf { .. } => out.extend(self.leaf_placement(n)),
            Body::Split { a, b, .. } => {
                self.collect(a, out);
                self.collect(b, out);
            }
            Body::Free => {}
        }
    }

    /// The split whose boundary coincides with the given edge of `leaf`.
    fn boundary(&self, leaf: usize, axis: Axis, leaf_in_second: bool) -> Option<usize> {
        let (mut child, mut p) = (leaf, self.nodes[leaf].parent);
        while p != NONE {
            if let Body::Split { axis: ax, b, .. } = self.nodes[p].body
                && ax == axis
                && (b == child) == leaf_in_second
            {
                return Some(p);
            }
            child = p;
            p = self.nodes[p].parent;
        }
        None
    }

    fn ctl(&self, leaf: usize, axis: Axis, prefer_first: bool) -> Option<AxisCtl> {
        // Dragging the left/top edge means the leaf is the second child of that split.
        let node = self
            .boundary(leaf, axis, prefer_first)
            .or_else(|| self.boundary(leaf, axis, !prefer_first))?;
        let Body::Split { ratio, .. } = self.nodes[node].body else {
            return None;
        };
        let r = self.nodes[node].rect;
        let len = if axis == Axis::Horizontal { r.w } else { r.h };
        (len > 0).then_some(AxisCtl {
            axis,
            node,
            start: ratio,
            len,
        })
    }

    /// `forced` overrides the split axis that the target box shape would pick.
    fn insert_axis(&mut self, id: WinId, hint: InsertHint, c: Constraints, forced: Option<Axis>) {
        self.remove(id);
        let target = self.pick_target(&hint);
        let leaf = self.alloc(Node {
            parent: NONE,
            rect: self.area,
            min: (0, 0),
            body: Body::Leaf {
                win: id,
                constraints: c,
            },
        });
        self.index.insert(id, leaf);
        self.last = Some(id);
        self.touch(true);
        let Some(target) = target else {
            self.root = leaf;
            return;
        };
        let first = match hint.side {
            NewWindowSide::First => true,
            NewWindowSide::Second => false,
            NewWindowSide::Pointer => hint.pointer.is_some_and(|p| {
                let r = self.nodes[target].rect;
                let c = r.center();
                if r.h > r.w { p.y < c.y } else { p.x < c.x }
            }),
        };
        self.split_leaf(target, leaf, first, forced);
    }
}

/// Size of the first child: honours the ratio, clamped to both minimums; when the
/// minimums cannot both fit, the space is shared in proportion to them.
fn split_len(total: i32, ratio: f32, min_a: i32, min_b: i32) -> i32 {
    let total = total.max(0);
    if min_a + min_b > total {
        return (i64::from(total) * i64::from(min_a) / i64::from(min_a + min_b)) as i32;
    }
    (((total as f32) * ratio).round() as i32).clamp(min_a, total - min_b)
}

fn split_rect(r: Rect, axis: Axis, ratio: f32, min_a: i32, min_b: i32) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let w = split_len(r.w, ratio, min_a, min_b);
            (
                Rect::new(r.x, r.y, w, r.h),
                Rect::new(r.x + w, r.y, r.w.max(0) - w, r.h),
            )
        }
        Axis::Vertical => {
            let h = split_len(r.h, ratio, min_a, min_b);
            (
                Rect::new(r.x, r.y, r.w, h),
                Rect::new(r.x, r.y + h, r.w, r.h.max(0) - h),
            )
        }
    }
}

impl TilingLayout for Dwindle {
    fn insert(&mut self, id: WinId, hint: InsertHint, c: Constraints) {
        self.insert_axis(id, hint, c, None);
    }

    fn remove(&mut self, id: WinId) -> bool {
        let Some(leaf) = self.index.remove(&id) else {
            return false;
        };
        if self.last == Some(id) {
            self.last = None;
        }
        let parent = self.nodes[leaf].parent;
        self.release(leaf);
        if parent == NONE {
            self.root = NONE;
        } else if let Body::Split { a, b, .. } = self.nodes[parent].body {
            let sibling = if a == leaf { b } else { a };
            let grand = self.nodes[parent].parent;
            self.nodes[sibling].rect = self.nodes[parent].rect;
            self.replace_child(grand, parent, sibling);
            self.release(parent);
        }
        self.touch(true);
        true
    }

    fn contains(&self, id: WinId) -> bool {
        self.index.contains_key(&id)
    }

    fn len(&self) -> usize {
        self.index.len()
    }

    fn swap(&mut self, a: WinId, b: WinId) -> bool {
        let (Some(&ia), Some(&ib)) = (self.index.get(&a), self.index.get(&b)) else {
            return false;
        };
        if ia == ib {
            return false;
        }
        let ba = std::mem::replace(&mut self.nodes[ia].body, Body::Free);
        let bb = std::mem::replace(&mut self.nodes[ib].body, ba);
        self.nodes[ia].body = bb;
        self.index.insert(a, ib);
        self.index.insert(b, ia);
        self.touch(true);
        true
    }

    fn move_beside(&mut self, id: WinId, target: WinId, side: Side, axis: Option<Axis>) -> bool {
        let (Some(&li), Some(&lt)) = (self.index.get(&id), self.index.get(&target)) else {
            return false;
        };
        if li == lt {
            return false;
        }
        let Body::Leaf { constraints, .. } = self.nodes[li].body else {
            return false;
        };
        let parent = self.nodes[li].parent;
        if parent != NONE && parent == self.nodes[lt].parent {
            // Siblings: keep the ratio, only put them in the wanted order (and axis).
            if let Body::Split { axis: ax, a, b, .. } = &mut self.nodes[parent].body {
                let (first, second) = if side == Side::First {
                    (li, lt)
                } else {
                    (lt, li)
                };
                let mut changed = false;
                if *a != first {
                    *a = first;
                    *b = second;
                    changed = true;
                }
                if let Some(want) = axis
                    && *ax != want
                {
                    *ax = want;
                    changed = true;
                }
                if changed {
                    self.touch(true);
                }
            }
            return true;
        }
        let hint = InsertHint {
            after: Some(target),
            side: match side {
                Side::First => NewWindowSide::First,
                Side::Second => NewWindowSide::Second,
            },
            pointer: None,
        };
        self.insert_axis(id, hint, constraints, axis);
        true
    }

    fn set_constraints(&mut self, id: WinId, c: Constraints) -> bool {
        let Some(&i) = self.index.get(&id) else {
            return false;
        };
        if let Body::Leaf { constraints, .. } = &mut self.nodes[i].body
            && *constraints != c
        {
            *constraints = c;
            self.touch(false);
        }
        true
    }

    fn toggle_split(&mut self, id: WinId) -> bool {
        let Some(&i) = self.index.get(&id) else {
            return false;
        };
        let p = self.nodes[i].parent;
        if p == NONE {
            return false;
        }
        if let Body::Split { axis, .. } = &mut self.nodes[p].body {
            *axis = match *axis {
                Axis::Horizontal => Axis::Vertical,
                Axis::Vertical => Axis::Horizontal,
            };
            self.touch(false);
            return true;
        }
        false
    }

    fn resize_start(&self, id: WinId, edges: Edges) -> Option<ResizeHandle> {
        let leaf = *self.index.get(&id)?;
        let horiz = (edges.contains(Edges::LEFT) || edges.contains(Edges::RIGHT))
            .then(|| self.ctl(leaf, Axis::Horizontal, edges.contains(Edges::LEFT)))
            .flatten();
        let vert = (edges.contains(Edges::TOP) || edges.contains(Edges::BOTTOM))
            .then(|| self.ctl(leaf, Axis::Vertical, edges.contains(Edges::TOP)))
            .flatten();
        (horiz.is_some() || vert.is_some()).then_some(ResizeHandle {
            shape: self.shape,
            ctls: [horiz, vert],
        })
    }

    fn resize_set(&mut self, h: &ResizeHandle, dx: i32, dy: i32) -> bool {
        if h.shape != self.shape {
            return false;
        }
        let lo = self.params.min_ratio.clamp(MIN_RATIO, 0.5);
        let mut changed = false;
        for ctl in h.ctls.iter().flatten() {
            let d = if ctl.axis == Axis::Horizontal { dx } else { dy };
            let want = (ctl.start + d as f32 / ctl.len as f32).clamp(lo, 1.0 - lo);
            if let Some(Node {
                body: Body::Split { ratio, .. },
                ..
            }) = self.nodes.get_mut(ctl.node)
                && *ratio != want
            {
                *ratio = want;
                changed = true;
            }
        }
        if changed {
            self.touch(false);
        }
        changed
    }

    fn relayout(&mut self, area: Rect, params: &LayoutParams) {
        let mut p = *params;
        p.gaps.inner = p.gaps.inner.max(0);
        p.gaps.outer = p.gaps.outer.max(0);
        p.border = p.border.max(0);
        let area = Rect {
            w: area.w.max(0),
            h: area.h.max(0),
            ..area
        };
        if !self.dirty && area == self.area && p == self.params {
            return;
        }
        self.area = area;
        self.params = p;
        self.dirty = false;
        self.epoch += 1;
        if self.root == NONE {
            return;
        }
        self.compute_min(self.root, 2 * (p.border + p.gaps.inner));
        // Every box is later inset by gaps_in, so widening the tree by the difference
        // makes edge windows sit exactly gaps_out from the area.
        self.place(self.root, area.shrink(p.gaps.outer - p.gaps.inner));
    }

    fn rect(&self, id: WinId) -> Option<Rect> {
        self.leaf_placement(*self.index.get(&id)?).map(|p| p.outer)
    }

    fn placements(&self, out: &mut Vec<Placement>) {
        if self.root != NONE {
            self.collect(self.root, out);
        }
    }

    fn windows(&self, out: &mut Vec<WinId>) {
        let start = out.len();
        out.extend(self.index.keys().copied());
        out[start..].sort_unstable();
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect::new(0, 0, 1000, 800);

    fn w(n: u64) -> WinId {
        WinId(n)
    }

    fn after(n: u64) -> InsertHint {
        InsertHint {
            after: Some(w(n)),
            ..Default::default()
        }
    }

    fn params(inner: i32, outer: i32, border: i32) -> LayoutParams {
        LayoutParams {
            gaps: crate::Gaps { inner, outer },
            border,
            ..Default::default()
        }
    }

    fn tree(ids: &[u64], p: &LayoutParams) -> Dwindle {
        let mut d = Dwindle::new();
        for (i, &n) in ids.iter().enumerate() {
            let hint = if i == 0 {
                InsertHint::default()
            } else {
                after(ids[i - 1])
            };
            d.insert(w(n), hint, Constraints::default());
            d.relayout(AREA, p);
        }
        d
    }

    fn r(d: &Dwindle, n: u64) -> Rect {
        d.rect(w(n)).unwrap()
    }

    fn cons(minw: i32, minh: i32, maxw: i32, maxh: i32) -> Constraints {
        Constraints {
            min: crate::Size { w: minw, h: minh },
            max: crate::Size { w: maxw, h: maxh },
        }
    }

    #[test]
    fn insert_direction_follows_box_shape() {
        let p = params(0, 0, 0);
        let d = tree(&[1, 2], &p);
        // 1000x800 is wider than tall: side by side, new window on the right.
        assert_eq!(r(&d, 1), Rect::new(0, 0, 500, 800));
        assert_eq!(r(&d, 2), Rect::new(500, 0, 500, 800));
        // 500x800 is taller than wide: the third is stacked below the second.
        let d = tree(&[1, 2, 3], &p);
        assert_eq!(r(&d, 2), Rect::new(500, 0, 500, 400));
        assert_eq!(r(&d, 3), Rect::new(500, 400, 500, 400));
    }

    #[test]
    fn insert_first_side_and_unknown_target() {
        let p = params(0, 0, 0);
        let mut d = tree(&[1], &p);
        let hint = InsertHint {
            after: Some(w(99)),
            side: NewWindowSide::First,
            pointer: None,
        };
        d.insert(w(2), hint, Constraints::default());
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 2).x, 0);
        assert_eq!(r(&d, 1).x, 500);
    }

    #[test]
    fn stored_axis_is_not_recomputed() {
        let p = params(0, 0, 0);
        let mut d = tree(&[1, 2], &p);
        // Shrink the area so the pair would be "tall"; the split stays horizontal.
        d.relayout(Rect::new(0, 0, 200, 800), &p);
        assert_eq!(r(&d, 1), Rect::new(0, 0, 100, 800));
        assert_eq!(r(&d, 2), Rect::new(100, 0, 100, 800));
    }

    #[test]
    fn remove_collapses_parent() {
        let p = params(0, 0, 0);
        let mut d = tree(&[1, 2, 3], &p);
        assert!(d.remove(w(2)));
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 1), Rect::new(0, 0, 500, 800));
        assert_eq!(r(&d, 3), Rect::new(500, 0, 500, 800));
        assert!(!d.remove(w(2)));
        assert!(d.remove(w(1)));
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 3), AREA);
    }

    #[test]
    fn last_window_removal_and_reuse() {
        let p = params(0, 0, 0);
        let mut d = tree(&[1], &p);
        assert!(d.remove(w(1)));
        assert!(d.is_empty());
        let mut out = Vec::new();
        d.relayout(AREA, &p);
        d.placements(&mut out);
        assert!(out.is_empty());
        d.insert(w(5), InsertHint::default(), Constraints::default());
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 5), AREA);
        assert!(d.rect(w(1)).is_none());
    }

    #[test]
    fn min_size_clamps_ratio() {
        let p = params(0, 0, 0);
        let mut d = Dwindle::new();
        d.insert(w(1), InsertHint::default(), cons(700, 0, 0, 0));
        d.insert(w(2), after(1), Constraints::default());
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 1).w, 700);
        assert_eq!(r(&d, 2).w, 300);
    }

    #[test]
    fn min_overflow_degrades_proportionally() {
        let p = params(0, 0, 0);
        let mut d = Dwindle::new();
        d.insert(w(1), InsertHint::default(), cons(900, 0, 0, 0));
        d.insert(w(2), after(1), cons(300, 0, 0, 0));
        d.relayout(AREA, &p);
        assert_eq!((r(&d, 1).w, r(&d, 2).w), (750, 250));
        d.relayout(Rect::new(0, 0, 0, 0), &p);
        assert!(r(&d, 1).w >= 0 && r(&d, 2).w >= 0 && r(&d, 1).h >= 0);
    }

    #[test]
    fn min_includes_border_and_gaps() {
        let p = params(5, 0, 2);
        let mut d = Dwindle::new();
        d.insert(w(1), InsertHint::default(), cons(600, 0, 0, 0));
        d.insert(w(2), after(1), Constraints::default());
        d.relayout(AREA, &p);
        let mut out = Vec::new();
        d.placements(&mut out);
        assert_eq!(out[0].content.w, 600);
    }

    #[test]
    fn max_size_is_soft_and_centred() {
        let p = params(0, 0, 0);
        let mut d = Dwindle::new();
        d.insert(w(1), InsertHint::default(), cons(0, 0, 400, 300));
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 1), Rect::new(300, 250, 400, 300));
    }

    #[test]
    fn gap_arithmetic() {
        let p = params(6, 10, 0);
        let d = tree(&[1, 2], &p);
        let (a, b) = (r(&d, 1), r(&d, 2));
        assert_eq!(b.x - a.right(), 12);
        assert_eq!(a.x, 10);
        assert_eq!(a.y, 10);
        assert_eq!(b.right(), 1000 - 10);
        assert_eq!(a.bottom(), 800 - 10);
    }

    #[test]
    fn border_shrinks_content() {
        let p = params(0, 0, 3);
        let d = tree(&[1], &p);
        let mut out = Vec::new();
        d.placements(&mut out);
        assert_eq!(out[0].outer, AREA);
        assert_eq!(out[0].content, Rect::new(3, 3, 994, 794));
    }

    #[test]
    fn move_beside_sibling_flips_order() {
        let p = params(0, 0, 0);
        let mut d = tree(&[1, 2], &p);
        assert!(d.move_beside(w(2), w(1), Side::First, None));
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 2), Rect::new(0, 0, 500, 800));
        assert_eq!(r(&d, 1), Rect::new(500, 0, 500, 800));
    }

    #[test]
    fn move_beside_non_sibling_reinserts() {
        let p = params(0, 0, 0);
        let mut d = tree(&[1, 2, 3], &p);
        // 3 (bottom right) goes before 1; 1 is tall (500x800) so 3 lands above it.
        assert!(d.move_beside(w(3), w(1), Side::First, None));
        d.relayout(AREA, &p);
        assert_eq!(d.len(), 3);
        assert_eq!(r(&d, 3), Rect::new(0, 0, 500, 400));
        assert_eq!(r(&d, 1), Rect::new(0, 400, 500, 400));
        assert_eq!(r(&d, 2), Rect::new(500, 0, 500, 800));
        assert!(!d.move_beside(w(3), w(99), Side::First, None));
    }


    #[test]
    fn move_beside_forced_axis() {
        let p = params(0, 0, 0);
        // 1 | (2 over 3): moving 3 left of 2 must give a side-by-side split despite 2 being wide-ish.
        let mut d = tree(&[1, 2, 3], &p);
        assert!(d.move_beside(w(3), w(2), Side::First, Some(Axis::Horizontal)));
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 3).y, r(&d, 2).y);
        assert!(r(&d, 3).x < r(&d, 2).x);
        // Siblings rewrite the axis too.
        let mut d = tree(&[1, 2], &p);
        assert!(d.move_beside(w(2), w(1), Side::First, Some(Axis::Vertical)));
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 2), Rect::new(0, 0, 1000, 400));
    }
    #[test]
    fn resize_ratio_is_absolute_and_clamped() {
        let p = params(0, 0, 0);
        let mut d = tree(&[1, 2], &p);
        let h = d.resize_start(w(1), Edges::RIGHT).unwrap();
        assert!(d.resize_set(&h, 100, 0));
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 1).w, 600);
        // Total movement, not increments: going back to 0 restores the start.
        assert!(d.resize_set(&h, 0, 0));
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 1).w, 500);
        d.resize_set(&h, 10_000, 0);
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 1).w, 900);
        d.resize_set(&h, -10_000, 0);
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 1).w, 100);
    }

    #[test]
    fn resize_edge_selection() {
        let p = params(0, 0, 0);
        let mut d = tree(&[1, 2], &p);
        // The left edge of 1 is the screen edge: falls back to the shared boundary.
        assert!(d.resize_start(w(1), Edges::LEFT).is_some());
        // No vertical split exists at all.
        assert!(d.resize_start(w(1), Edges::TOP).is_none());
        assert!(d.resize_start(w(99), Edges::LEFT).is_none());
        let h = d.resize_start(w(1), Edges::RIGHT).unwrap();
        d.remove(w(2));
        assert!(!d.resize_set(&h, 50, 0));
    }

    #[test]
    fn swap_and_toggle_split() {
        let p = params(0, 0, 0);
        let mut d = tree(&[1, 2], &p);
        assert!(d.swap(w(1), w(2)));
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 2).x, 0);
        assert!(d.toggle_split(w(1)));
        d.relayout(AREA, &p);
        assert_eq!(r(&d, 2), Rect::new(0, 0, 1000, 400));
        assert!(!d.swap(w(1), w(99)));
    }

    #[test]
    fn epoch_moves_only_on_change() {
        let p = params(0, 0, 0);
        let mut d = tree(&[1, 2], &p);
        let e = d.epoch();
        d.relayout(AREA, &p);
        assert_eq!(d.epoch(), e);
        d.relayout(Rect::new(0, 0, 900, 800), &p);
        assert_ne!(d.epoch(), e);
    }
}
