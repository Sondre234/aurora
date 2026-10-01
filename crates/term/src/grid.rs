//! Cell grid geometry: how many cells fit a surface and where each one sits. Pure.
//!
//! All positions of the grid are whole device pixels (cell sizes come from
//! [`CellMetrics`], padding is rounded), converted to logical coordinates only at the
//! painter boundary so every cell edge lands on a pixel at any fractional scale.

use aurora_ui::{CellMetrics, Point, Rect};

/// Padding around the grid in logical pixels.
pub const PADDING: f32 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub cell_w: u32,
    pub cell_h: u32,
    /// Baseline offset inside a cell, device px.
    pub baseline: u32,
    pub scale: f32,
    /// Padding on every side, device px.
    pub pad: u32,
    pub cols: usize,
    pub rows: usize,
}

/// Whole cells that fit `logical` (w, h) after padding; at least 1x1.
pub fn grid_size(m: &CellMetrics, logical: (f32, f32), padding: f32) -> (usize, usize) {
    let pad = (padding * m.scale).round().max(0.0) as u32;
    let fit = |len: f32, cell: u32| {
        let dev = (len * m.scale).round().max(0.0) as u32;
        (dev.saturating_sub(2 * pad) / cell).max(1) as usize
    };
    (fit(logical.0, m.width), fit(logical.1, m.height))
}

impl Geometry {
    pub fn new(m: &CellMetrics, logical: (f32, f32), padding: f32) -> Self {
        let (cols, rows) = grid_size(m, logical, padding);
        Self {
            cell_w: m.width,
            cell_h: m.height,
            baseline: m.baseline,
            scale: m.scale,
            pad: (padding * m.scale).round().max(0.0) as u32,
            cols,
            rows,
        }
    }

    fn lx(&self, dev: u32) -> f32 {
        dev as f32 / self.scale
    }

    /// Top-left of a cell, logical.
    pub fn origin(&self, col: usize, row: usize) -> Point {
        Point::new(
            self.lx(self.pad + col as u32 * self.cell_w),
            self.lx(self.pad + row as u32 * self.cell_h),
        )
    }

    /// `cols` cells starting at (`col`, `row`), logical.
    pub fn span(&self, col: usize, row: usize, cols: usize) -> Rect {
        let o = self.origin(col, row);
        Rect::new(
            o.x,
            o.y,
            self.lx(cols as u32 * self.cell_w),
            self.lx(self.cell_h),
        )
    }

    /// The whole grid, logical.
    pub fn grid_rect(&self) -> Rect {
        let o = self.origin(0, 0);
        Rect::new(
            o.x,
            o.y,
            self.lx(self.cols as u32 * self.cell_w),
            self.lx(self.rows as u32 * self.cell_h),
        )
    }

    /// The cell holding a logical point (clamped into the grid) and whether the point is
    /// in the right half of it (selections start on the nearer side of a cell).
    pub fn hit(&self, p: Point) -> (usize, usize, bool) {
        let to_cell = |v: f32, cell: u32, n: usize| {
            let dev = (v * self.scale - self.pad as f32).max(0.0);
            let idx = (dev / cell as f32) as usize;
            let frac = dev - idx as f32 * cell as f32;
            if idx >= n {
                (n - 1, true)
            } else {
                (idx, frac >= cell as f32 / 2.0)
            }
        };
        let (col, right) = to_cell(p.x, self.cell_w, self.cols);
        let (row, _) = to_cell(p.y, self.cell_h, self.rows);
        (col, row, right)
    }

    /// Rows and columns (inclusive) whose cells touch `area`, widened by one column each
    /// side so glyphs overhanging a damaged edge are repainted. `None` if the area misses
    /// the grid.
    pub fn cells_in(&self, area: Rect) -> Option<(usize, usize, usize, usize)> {
        let g = self.grid_rect().intersect(&area)?;
        let cw = self.cell_w as f32 / self.scale;
        let ch = self.cell_h as f32 / self.scale;
        let o = self.origin(0, 0);
        let c0 = (((g.x - o.x) / cw).floor().max(0.0) as usize).saturating_sub(1);
        let c1 = (((g.right() - o.x) / cw).ceil() as usize).min(self.cols);
        let r0 = ((g.y - o.y) / ch).floor().max(0.0) as usize;
        let r1 = (((g.bottom() - o.y) / ch).ceil() as usize).min(self.rows);
        (c1 > 0 && r1 > r0).then(|| (r0, r1 - 1, c0, (c1 + 1).min(self.cols) - 1))
    }

    /// Logical rect covering columns `left..=right` of `row`.
    pub fn damage(&self, row: usize, left: usize, right: usize) -> Rect {
        self.span(left, row, right + 1 - left)
    }
}

/// Turns wheel pixels into whole lines, carrying the remainder to the next event.
#[derive(Debug, Default)]
pub struct LineAccum(f32);

impl LineAccum {
    /// Add `px` of scrolling at `line_px` pixels per line; whole lines now due
    /// (positive: towards later lines).
    pub fn add(&mut self, px: f32, line_px: f32) -> i32 {
        self.0 += px;
        let lines = (self.0 / line_px).trunc();
        self.0 -= lines * line_px;
        lines as i32
    }

    pub fn reset(&mut self) {
        self.0 = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(scale: f32) -> CellMetrics {
        CellMetrics::snap(8.0, 17.0, 13.0, scale)
    }

    #[test]
    fn grid_snaps_to_whole_cells_after_padding() {
        // 804 x 400 logical at scale 1: 4 px padding each side leaves 796 x 392.
        assert_eq!(grid_size(&metrics(1.0), (804.0, 400.0), 4.0), (99, 23));
        // A window smaller than one cell still has a 1x1 grid.
        assert_eq!(grid_size(&metrics(1.0), (5.0, 5.0), 4.0), (1, 1));
    }

    #[test]
    fn fractional_scale_uses_device_pixels() {
        let m = CellMetrics::snap(12.0, 25.0, 19.0, 1.5);
        // 800 x 600 logical = 1200 x 900 device, padding 6: 1188 / 12 = 99, 888 / 25 = 35.
        assert_eq!(grid_size(&m, (800.0, 600.0), 4.0), (99, 35));
    }

    #[test]
    fn cell_edges_are_whole_device_pixels() {
        let m = CellMetrics::snap(12.0, 25.0, 19.0, 1.5);
        let g = Geometry::new(&m, (800.0, 600.0), 4.0);
        for col in [0usize, 1, 7, 98] {
            let r = g.span(col, 3, 1);
            let dev = |v: f32| (v * 1.5 * 1000.0).round() / 1000.0;
            assert_eq!(dev(r.x).fract(), 0.0, "x of col {col}");
            assert_eq!(dev(r.w), 12.0);
            assert_eq!(dev(r.y).fract(), 0.0);
        }
        // The grid ends inside the surface.
        assert!(g.grid_rect().right() <= 800.0 && g.grid_rect().bottom() <= 600.0);
    }

    #[test]
    fn hit_test_clamps_and_picks_the_side() {
        let g = Geometry::new(&metrics(1.0), (804.0, 400.0), 4.0);
        // Padding area maps to the first cell, left half.
        assert_eq!(g.hit(Point::new(0.0, 0.0)), (0, 0, false));
        // x = 4 + 8 + 5: second cell, right half.
        assert_eq!(g.hit(Point::new(17.0, 5.0)), (1, 0, true));
        assert_eq!(g.hit(Point::new(13.0, 5.0)), (1, 0, false));
        // Past the right and bottom edges: last cell.
        let (c, r, right) = g.hit(Point::new(5000.0, 5000.0));
        assert_eq!((c, r, right), (98, 22, true));
        // Row 2 starts at 4 + 2 * 17.
        assert_eq!(g.hit(Point::new(10.0, 38.0)).1, 2);
        assert_eq!(g.hit(Point::new(10.0, 37.0)).1, 1);
    }

    #[test]
    fn cells_in_widens_by_a_column() {
        let g = Geometry::new(&metrics(1.0), (804.0, 400.0), 4.0);
        // One cell at col 5, row 2.
        let r = g.span(5, 2, 1);
        assert_eq!(g.cells_in(r), Some((2, 2, 4, 6)));
        // At the left edge the widening is clamped.
        assert_eq!(g.cells_in(g.span(0, 0, 1)), Some((0, 0, 0, 1)));
        // Entirely in the padding: nothing to paint.
        assert_eq!(g.cells_in(Rect::new(0.0, 0.0, 3.0, 400.0)), None);
        // The full surface covers the whole grid.
        assert_eq!(
            g.cells_in(Rect::new(0.0, 0.0, 804.0, 400.0)),
            Some((0, 22, 0, 98))
        );
    }

    #[test]
    fn damage_rect_spans_columns() {
        let g = Geometry::new(&metrics(1.0), (804.0, 400.0), 4.0);
        let r = g.damage(1, 2, 4);
        assert_eq!((r.x, r.y, r.w, r.h), (20.0, 21.0, 24.0, 17.0));
    }

    #[test]
    fn scroll_accumulates_remainders() {
        let mut a = LineAccum::default();
        assert_eq!(a.add(10.0, 18.0), 0);
        assert_eq!(a.add(10.0, 18.0), 1);
        assert_eq!(a.add(-40.0, 18.0), -2);
        assert_eq!(a.add(-2.0, 18.0), 0);
        assert_eq!(a.add(-16.0, 18.0), -1);
        a.reset();
        assert_eq!(a.add(17.0, 18.0), 0);
    }
}
