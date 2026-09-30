//! Thumbnail layout of the overview. Pure: integer rectangles in, integer rectangles out.
//!
//! An output shows a grid of workspace panels (each the shape of the output, like a small
//! copy of it) and inside each panel a grid of window thumbnails, every thumbnail fitted into
//! its cell at the window's own aspect ratio.
use aurora_layout::{Point, Rect, WinId};

/// Gap between panels and the margin around all of them, in logical pixels.
pub const PANEL_GAP: i32 = 24;
/// Gap between thumbnails.
pub const TILE_GAP: i32 = 10;
/// Padding inside a panel.
pub const PANEL_PAD: i32 = 12;

/// One workspace to lay out: its windows with their current size (aspect ratio source).
#[derive(Clone, Debug)]
pub struct WsInput {
    pub ws: u32,
    pub windows: Vec<(WinId, i32, i32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelLayout {
    pub ws: u32,
    pub rect: Rect,
    pub tiles: Vec<(WinId, Rect)>,
}

/// What a point lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Tile(u32, WinId),
    Panel(u32),
}

/// `n` equal cells of shape `aspect` (width, height) inside `bounds`, in rows, the last row
/// centred. Picks the column count that makes the cells biggest. The whole block is centred
/// in `bounds`. When nothing fits, the cells are empty rectangles.
pub fn grid(n: usize, bounds: Rect, aspect: (i32, i32), gap: i32) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    let (aw, ah) = (aspect.0.max(1) as f64, aspect.1.max(1) as f64);
    let empty = Rect::new(bounds.x + bounds.w / 2, bounds.y + bounds.h / 2, 0, 0);
    if bounds.w <= 0 || bounds.h <= 0 {
        return vec![empty; n];
    }
    let mut best: Option<(usize, i32, i32)> = None;
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let cw = f64::from(bounds.w - gap * (cols as i32 - 1)) / cols as f64;
        let ch = f64::from(bounds.h - gap * (rows as i32 - 1)) / rows as f64;
        if cw < 1.0 || ch < 1.0 {
            continue;
        }
        let scale = (cw / aw).min(ch / ah);
        let (w, h) = ((aw * scale).floor() as i32, (ah * scale).floor() as i32);
        if w < 1 || h < 1 {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, bw, bh)) => i64::from(w) * i64::from(h) > i64::from(bw) * i64::from(bh),
        };
        if better {
            best = Some((cols, w, h));
        }
    }
    let Some((cols, w, h)) = best else {
        return vec![empty; n];
    };
    let rows = n.div_ceil(cols);
    let total_h = rows as i32 * h + (rows as i32 - 1) * gap;
    let top = bounds.y + (bounds.h - total_h) / 2;
    (0..n)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            let in_row = if row == rows - 1 { n - row * cols } else { cols };
            let row_w = in_row as i32 * w + (in_row as i32 - 1) * gap;
            let left = bounds.x + (bounds.w - row_w) / 2;
            Rect::new(
                left + col as i32 * (w + gap),
                top + row as i32 * (h + gap),
                w,
                h,
            )
        })
        .collect()
}

/// `size` scaled down to fit `cell` (never up), centred in it.
pub fn fit(size: (i32, i32), cell: Rect) -> Rect {
    let (w, h) = (size.0.max(1) as f64, size.1.max(1) as f64);
    let scale = (f64::from(cell.w) / w)
        .min(f64::from(cell.h) / h)
        .min(1.0);
    let fw = ((w * scale).round() as i32).clamp(1, cell.w.max(1));
    let fh = ((h * scale).round() as i32).clamp(1, cell.h.max(1));
    Rect::new(cell.x + (cell.w - fw) / 2, cell.y + (cell.h - fh) / 2, fw, fh)
}

/// Panels for `inputs` inside `area`, each holding its window thumbnails.
pub fn layout(area: Rect, inputs: &[WsInput]) -> Vec<PanelLayout> {
    let panels = grid(
        inputs.len(),
        area.shrink(PANEL_GAP),
        (area.w, area.h),
        PANEL_GAP,
    );
    inputs
        .iter()
        .zip(panels)
        .map(|(input, rect)| {
            let inner = rect.shrink(PANEL_PAD);
            let cells = grid(input.windows.len(), inner, (rect.w, rect.h), TILE_GAP);
            let tiles = input
                .windows
                .iter()
                .zip(cells)
                .map(|(&(id, w, h), cell)| (id, fit((w, h), cell)))
                .collect();
            PanelLayout {
                ws: input.ws,
                rect,
                tiles,
            }
        })
        .collect()
}

/// The tile (else panel) under `p`. Tiles win over the panel they sit in.
pub fn hit(panels: &[PanelLayout], p: Point) -> Option<Target> {
    let panel = panels.iter().find(|panel| panel.rect.contains(p))?;
    panel
        .tiles
        .iter()
        .find(|(_, r)| r.contains(p))
        .map(|&(id, _)| Target::Tile(panel.ws, id))
        .or(Some(Target::Panel(panel.ws)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn within(inner: Rect, outer: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.right() <= outer.right()
            && inner.bottom() <= outer.bottom()
    }

    fn disjoint(a: Rect, b: Rect) -> bool {
        a.intersect(b).is_none()
    }

    #[test]
    fn grid_cells_fit_and_do_not_overlap() {
        let bounds = Rect::new(10, 20, 1900, 1000);
        for n in 1..=12 {
            let cells = grid(n, bounds, (16, 9), 10);
            assert_eq!(cells.len(), n);
            for (i, a) in cells.iter().enumerate() {
                assert!(within(*a, bounds), "n={n} cell {i} {a:?} outside");
                assert!(a.w > 0 && a.h > 0);
                for b in &cells[i + 1..] {
                    assert!(disjoint(*a, *b), "n={n} {a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn grid_keeps_aspect_and_prefers_big_cells() {
        let one = grid(1, Rect::new(0, 0, 1600, 900), (16, 9), 10);
        assert_eq!(one, vec![Rect::new(0, 0, 1600, 900)]);
        let four = grid(4, Rect::new(0, 0, 1610, 910), (16, 9), 10);
        assert_eq!(four[0].w, 800);
        assert_eq!(four[0].y, four[1].y);
        assert!(four[2].y > four[0].y);
    }

    #[test]
    fn last_row_is_centred() {
        let cells = grid(3, Rect::new(0, 0, 1000, 1000), (1, 1), 0);
        assert_eq!(cells[0].y, cells[1].y);
        assert_eq!(cells[2].x, (1000 - cells[2].w) / 2);
    }

    #[test]
    fn degenerate_bounds_do_not_panic() {
        assert!(grid(0, Rect::new(0, 0, 100, 100), (1, 1), 5).is_empty());
        let cells = grid(3, Rect::new(0, 0, 0, 0), (1, 1), 5);
        assert_eq!(cells.len(), 3);
        assert!(cells.iter().all(|c| c.is_empty()));
        let cells = grid(50, Rect::new(0, 0, 40, 40), (1, 1), 30);
        assert_eq!(cells.len(), 50);
    }

    #[test]
    fn fit_keeps_aspect_never_upscales_and_centres() {
        let cell = Rect::new(100, 100, 400, 300);
        let wide = fit((800, 200), cell);
        assert_eq!((wide.w, wide.h), (400, 100));
        assert_eq!(wide.y, 100 + (300 - 100) / 2);
        let small = fit((100, 50), cell);
        assert_eq!((small.w, small.h), (100, 50));
        assert_eq!(small.center(), cell.center());
        let tall = fit((100, 900), cell);
        assert!(tall.h <= 300 && within(tall, cell));
    }

    fn input(ws: u32, ids: &[u64]) -> WsInput {
        WsInput {
            ws,
            windows: ids.iter().map(|&i| (WinId(i), 1000, 600)).collect(),
        }
    }

    #[test]
    fn layout_nests_tiles_in_panels_in_area() {
        let area = Rect::new(0, 40, 2560, 1400);
        let inputs = [input(1, &[1, 2, 3]), input(2, &[]), input(3, &[4])];
        let panels = layout(area, &inputs);
        assert_eq!(panels.len(), 3);
        for (i, p) in panels.iter().enumerate() {
            assert!(within(p.rect, area));
            assert_eq!(p.ws, inputs[i].ws);
            assert_eq!(p.tiles.len(), inputs[i].windows.len());
            for (_, t) in &p.tiles {
                assert!(within(*t, p.rect), "{t:?} not in {:?}", p.rect);
            }
            for q in &panels[i + 1..] {
                assert!(disjoint(p.rect, q.rect));
            }
        }
    }

    #[test]
    fn hit_prefers_tiles_then_panel_then_nothing() {
        let panels = layout(
            Rect::new(0, 0, 1920, 1080),
            &[input(1, &[7]), input(2, &[])],
        );
        let tile = panels[0].tiles[0].1;
        assert_eq!(
            hit(&panels, tile.center()),
            Some(Target::Tile(1, WinId(7)))
        );
        let corner = Point {
            x: panels[0].rect.x + 1,
            y: panels[0].rect.y + 1,
        };
        assert_eq!(hit(&panels, corner), Some(Target::Panel(1)));
        assert_eq!(
            hit(&panels, panels[1].rect.center()),
            Some(Target::Panel(2))
        );
        assert_eq!(hit(&panels, Point { x: 0, y: 0 }), None);
        assert_eq!(hit(&[], Point { x: 5, y: 5 }), None);
    }
}
