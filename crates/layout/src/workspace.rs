//! One workspace: a tiled layout, a floating stack, at most one fullscreen/maximized
//! window and focus history. Fullscreen and maximized windows stay in their layout.

use std::collections::HashMap;

use crate::tiling::{InsertHint, TilingLayout};
use crate::{Constraints, Dir, Dwindle, FsMode, Kind, LayoutParams, Placement, Rect, WinId, geom};

pub struct Workspace {
    pub tiling: Box<dyn TilingLayout>,
    /// Outer rectangles, bottom to top.
    floating: Vec<(WinId, Rect)>,
    fullscreen: Option<(WinId, FsMode)>,
    /// Most recently focused first.
    mru: Vec<WinId>,
    parents: HashMap<WinId, WinId>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new(Box::new(Dwindle::new()))
    }
}

impl Workspace {
    pub fn new(tiling: Box<dyn TilingLayout>) -> Self {
        Self {
            tiling,
            floating: Vec::new(),
            fullscreen: None,
            mru: Vec::new(),
            parents: HashMap::new(),
        }
    }

    pub fn contains(&self, id: WinId) -> bool {
        self.is_floating(id) || self.tiling.contains(id)
    }

    pub fn is_floating(&self, id: WinId) -> bool {
        self.floating.iter().any(|&(f, _)| f == id)
    }

    pub fn len(&self) -> usize {
        self.floating.len() + self.tiling.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn floating(&self) -> &[(WinId, Rect)] {
        &self.floating
    }

    pub fn floating_rect(&self, id: WinId) -> Option<Rect> {
        self.floating
            .iter()
            .find(|&&(f, _)| f == id)
            .map(|&(_, r)| r)
    }

    pub fn set_floating_rect(&mut self, id: WinId, rect: Rect) -> bool {
        match self.floating.iter_mut().find(|(f, _)| *f == id) {
            Some(e) => {
                e.1 = rect;
                true
            }
            None => false,
        }
    }

    pub fn fullscreen(&self) -> Option<(WinId, FsMode)> {
        self.fullscreen
    }

    pub fn mru(&self) -> &[WinId] {
        &self.mru
    }

    /// Windows that are transient for `parent` follow it into fullscreen.
    pub fn set_parent(&mut self, id: WinId, parent: Option<WinId>) {
        match parent {
            Some(p) if p != id => {
                self.parents.insert(id, p);
            }
            _ => {
                self.parents.remove(&id);
            }
        }
    }

    pub fn add_tiled(&mut self, id: WinId, hint: InsertHint, c: Constraints) {
        self.floating.retain(|&(f, _)| f != id);
        self.tiling.insert(id, hint, c);
    }

    /// Adds `id` on top of the floating stack (moving it there from the tiling if needed).
    pub fn add_floating(&mut self, id: WinId, rect: Rect) {
        self.tiling.remove(id);
        self.floating.retain(|&(f, _)| f != id);
        self.floating.push((id, rect));
    }

    /// Tiled -> floating, keeping the window's current rectangle unless `rect` is given.
    pub fn set_floating(&mut self, id: WinId, rect: Option<Rect>) -> bool {
        let Some(current) = self.tiling.rect(id) else {
            return false;
        };
        self.add_floating(id, rect.unwrap_or(current));
        true
    }

    /// Floating -> tiled.
    pub fn set_tiled(&mut self, id: WinId, hint: InsertHint, c: Constraints) -> bool {
        if !self.is_floating(id) {
            return false;
        }
        self.add_tiled(id, hint, c);
        true
    }

    pub fn remove(&mut self, id: WinId) -> bool {
        let was = self.contains(id);
        self.tiling.remove(id);
        self.floating.retain(|&(f, _)| f != id);
        self.mru.retain(|&m| m != id);
        self.parents.remove(&id);
        self.parents.retain(|_, p| *p != id);
        if self.fullscreen.is_some_and(|(f, _)| f == id) {
            self.fullscreen = None;
        }
        was
    }

    /// `Some(mode)` makes `id` fullscreen/maximized (replacing any other); `None` clears
    /// it if `id` holds it. False for unknown windows or when nothing changed.
    pub fn set_fullscreen(&mut self, id: WinId, mode: Option<FsMode>) -> bool {
        match mode {
            Some(m) if self.contains(id) => {
                let new = Some((id, m));
                let changed = self.fullscreen != new;
                self.fullscreen = new;
                changed
            }
            None if self.fullscreen.is_some_and(|(f, _)| f == id) => {
                self.fullscreen = None;
                true
            }
            _ => false,
        }
    }

    /// Records focus and raises floating windows.
    pub fn note_focus(&mut self, id: WinId) {
        if !self.contains(id) {
            return;
        }
        self.mru.retain(|&m| m != id);
        self.mru.insert(0, id);
        if let Some(i) = self.floating.iter().position(|&(f, _)| f == id) {
            let e = self.floating.remove(i);
            self.floating.push(e);
        }
    }

    /// What to focus once `closed` is gone: its parent, else the most recently focused,
    /// else anything left. Works before or after `remove`.
    pub fn focus_after_close(&self, closed: WinId) -> Option<WinId> {
        let ok = |id: WinId| id != closed && self.contains(id);
        if let Some(&p) = self.parents.get(&closed)
            && ok(p)
        {
            return Some(p);
        }
        if let Some(&m) = self.mru.iter().find(|&&m| ok(m)) {
            return Some(m);
        }
        if let Some(&(f, _)) = self.floating.iter().rev().find(|&&(f, _)| ok(f)) {
            return Some(f);
        }
        let mut ids = Vec::new();
        self.tiling.windows(&mut ids);
        ids.into_iter().find(|&i| ok(i))
    }

    /// Keeps floating windows in the same relative place when the area changes.
    pub fn rebase(&mut self, old: Rect, new: Rect) {
        if old == new {
            return;
        }
        for (_, r) in &mut self.floating {
            let w = r.w.min(new.w);
            let h = r.h.min(new.h);
            let scale = |c: i32, o0: i32, ol: i32, n0: i32, nl: i32| {
                if ol > 0 {
                    n0 + (i64::from(c - o0) * i64::from(nl) / i64::from(ol)) as i32
                } else {
                    n0 + nl / 2
                }
            };
            let cx = scale(r.x + r.w / 2, old.x, old.w, new.x, new.w);
            let cy = scale(r.y + r.h / 2, old.y, old.h, new.y, new.h);
            let x = (cx - w / 2).clamp(new.x, (new.right() - w).max(new.x));
            let y = (cy - h / 2).clamp(new.y, (new.bottom() - h).max(new.y));
            *r = Rect::new(x, y, w, h);
        }
    }

    /// The window to focus when moving from `from` in `dir`, among windows placed by
    /// the last `placements` call (`placed` is that output).
    pub fn neighbor(&self, placed: &[Placement], from: WinId, dir: Dir) -> Option<WinId> {
        let rects: Vec<_> = placed.iter().map(|p| (p.id, p.outer)).collect();
        geom::neighbor(&rects, from, dir, &self.mru)
    }

    /// Appends this workspace's visible windows, bottom to top. `work` is the area
    /// left by layer surfaces, `full` the whole output.
    pub fn placements(
        &mut self,
        work: Rect,
        full: Rect,
        params: &LayoutParams,
        out: &mut Vec<Placement>,
    ) {
        self.tiling.relayout(work, params);
        let start = out.len();
        self.tiling.placements(out);
        let fs = self.fullscreen;
        let border = params.border.max(0);
        let fs_placement = |id, mode| match mode {
            FsMode::Fullscreen => Placement {
                id,
                outer: full,
                content: full,
                kind: Kind::Fullscreen,
            },
            FsMode::Maximized => Placement {
                id,
                outer: work,
                content: work,
                kind: Kind::Maximized,
            },
        };

        if let Some((_, FsMode::Fullscreen)) = fs {
            out.truncate(start);
        } else if let Some((id, mode @ FsMode::Maximized)) = fs
            && let Some(p) = out[start..].iter_mut().find(|p| p.id == id)
        {
            *p = fs_placement(id, mode);
        }

        // The fullscreen window goes first so its transients stack above it.
        if let Some((id, FsMode::Fullscreen)) = fs
            && self.contains(id)
        {
            out.push(fs_placement(id, FsMode::Fullscreen));
        }

        for &(id, rect) in &self.floating {
            match fs {
                Some((f, FsMode::Fullscreen)) if f == id => continue,
                Some((f, FsMode::Fullscreen)) if !self.is_descendant(id, f) => continue,
                Some((f, mode @ FsMode::Maximized)) if f == id => {
                    out.push(fs_placement(id, mode));
                    continue;
                }
                _ => {}
            }
            out.push(Placement {
                id,
                outer: rect,
                content: rect.shrink(border),
                kind: Kind::Floating,
            });
        }
    }

    fn is_descendant(&self, mut id: WinId, ancestor: WinId) -> bool {
        // Bounded so a parent cycle from a misbehaving client cannot loop.
        for _ in 0..32 {
            match self.parents.get(&id) {
                Some(&p) if p == ancestor => return true,
                Some(&p) => id = p,
                None => return false,
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Gaps;

    const WORK: Rect = Rect::new(0, 30, 1000, 770);
    const FULL: Rect = Rect::new(0, 0, 1000, 800);

    fn w(n: u64) -> WinId {
        WinId(n)
    }

    fn params() -> LayoutParams {
        LayoutParams {
            gaps: Gaps {
                inner: 5,
                outer: 10,
            },
            border: 2,
            ..Default::default()
        }
    }

    fn ws3() -> Workspace {
        let mut ws = Workspace::default();
        ws.add_tiled(w(1), InsertHint::default(), Constraints::default());
        ws.add_tiled(
            w(2),
            InsertHint {
                after: Some(w(1)),
                ..Default::default()
            },
            Constraints::default(),
        );
        ws.add_floating(w(3), Rect::new(100, 100, 300, 200));
        ws
    }

    fn place(ws: &mut Workspace) -> Vec<Placement> {
        let mut out = Vec::new();
        ws.placements(WORK, FULL, &params(), &mut out);
        out
    }

    #[test]
    fn stacking_order_tiled_then_floating() {
        let mut ws = ws3();
        let out = place(&mut ws);
        let kinds: Vec<_> = out.iter().map(|p| (p.id.0, p.kind)).collect();
        assert_eq!(
            kinds,
            [(1, Kind::Tiled), (2, Kind::Tiled), (3, Kind::Floating)]
        );
        assert_eq!(out[2].content, Rect::new(102, 102, 296, 196));
    }

    #[test]
    fn fullscreen_hides_everything_but_transients() {
        let mut ws = ws3();
        ws.add_floating(w(4), Rect::new(0, 0, 50, 50));
        ws.set_parent(w(4), Some(w(1)));
        assert!(ws.set_fullscreen(w(1), Some(FsMode::Fullscreen)));
        let out = place(&mut ws);
        let ids: Vec<_> = out.iter().map(|p| p.id.0).collect();
        assert_eq!(ids, [1, 4]);
        let fs = &out[0];
        assert_eq!(
            (fs.outer, fs.content, fs.kind),
            (FULL, FULL, Kind::Fullscreen)
        );
        // Still in the tree: leaving fullscreen restores the tiling untouched.
        assert!(ws.set_fullscreen(w(1), None));
        assert_eq!(place(&mut ws).len(), 4);
    }

    #[test]
    fn floating_fullscreen_covers_output() {
        let mut ws = ws3();
        assert!(ws.set_fullscreen(w(3), Some(FsMode::Fullscreen)));
        let out = place(&mut ws);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].outer, FULL);
        assert_eq!(ws.floating_rect(w(3)), Some(Rect::new(100, 100, 300, 200)));
    }

    #[test]
    fn maximized_uses_work_area_without_gaps_or_border() {
        let mut ws = ws3();
        assert!(ws.set_fullscreen(w(2), Some(FsMode::Maximized)));
        let out = place(&mut ws);
        let m = out.iter().find(|p| p.id == w(2)).unwrap();
        assert_eq!((m.outer, m.content, m.kind), (WORK, WORK, Kind::Maximized));
        assert_eq!(out.len(), 3);
        assert!(!ws.set_fullscreen(w(99), Some(FsMode::Maximized)));
    }

    #[test]
    fn maximized_floating_is_placed() {
        let mut ws = ws3();
        assert!(ws.set_fullscreen(w(3), Some(FsMode::Maximized)));
        let out = place(&mut ws);
        let m = out.iter().find(|p| p.id == w(3)).unwrap();
        assert_eq!((m.outer, m.kind), (WORK, Kind::Maximized));
        assert_eq!(out.len(), 3);
    }
    #[test]
    fn rebase_scales_floating_and_keeps_it_inside() {
        let mut ws = Workspace::default();
        ws.add_floating(w(1), Rect::new(400, 300, 200, 200));
        ws.rebase(Rect::new(0, 0, 1000, 800), Rect::new(1000, 0, 500, 400));
        assert_eq!(ws.floating_rect(w(1)), Some(Rect::new(1150, 100, 200, 200)));
        ws.rebase(Rect::new(1000, 0, 500, 400), Rect::new(0, 0, 100, 100));
        assert_eq!(ws.floating_rect(w(1)), Some(Rect::new(0, 0, 100, 100)));
    }

    #[test]
    fn focus_history_and_close() {
        let mut ws = ws3();
        ws.note_focus(w(1));
        ws.note_focus(w(3));
        ws.note_focus(w(2));
        assert_eq!(ws.focus_after_close(w(2)), Some(w(3)));
        ws.remove(w(2));
        assert_eq!(ws.focus_after_close(w(2)), Some(w(3)));
        ws.remove(w(3));
        assert_eq!(ws.focus_after_close(w(3)), Some(w(1)));
        ws.remove(w(1));
        assert_eq!(ws.focus_after_close(w(1)), None);
        assert!(ws.is_empty());
        assert!(!ws.remove(w(1)));
    }

    #[test]
    fn toggle_floating_round_trip() {
        let mut ws = ws3();
        let before = ws.tiling.rect(w(1)).unwrap();
        assert!(ws.set_floating(w(1), None));
        assert!(ws.is_floating(w(1)) && !ws.tiling.contains(w(1)));
        assert!(ws.floating_rect(w(1)).is_some());
        assert!(!ws.set_floating(w(1), None));
        assert!(ws.set_tiled(w(1), InsertHint::default(), Constraints::default()));
        assert!(ws.tiling.contains(w(1)) && !ws.is_floating(w(1)));
        let _ = before;
    }
}
