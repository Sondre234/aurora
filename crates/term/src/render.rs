//! The cell grid to pixels, written against `aurora_ui::Painter`.
//!
//! [`View`] is the state the canvas needs to paint (emulator, geometry, colors, fonts,
//! glyph cache). Painting is damage driven: the runtime hands over the rectangles to
//! repaint and only the rows and columns touching them are drawn. Per cell: background
//! runs first (one fill per run of equal color), then glyphs (shaped once per
//! character and style into a byte-budgeted LRU), then decorations and the cursor.
//! Cell edges are whole device pixels at any scale (see [`crate::grid`]).

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Instant;

use aurora_theme::{Rgba, Theme};
use aurora_ui::cache::LruCache;
use aurora_ui::{CellMetrics, Color, FontFamily, Painter, Rect, ShapedText, TextStyle, TextSystem};

use crate::backend::{Backend, CellView, CursorKind, Damage};
use crate::colors::{BACKGROUND, ColorRef, Scheme};
use crate::grid::{Geometry, PADDING};

/// Budget of each glyph shaping cache.
pub const GLYPH_BUDGET: usize = 16 << 20;

/// Line height as a multiple of the font size.
const LINE_HEIGHT: f32 = 1.25;

pub fn to_color(c: Rgba) -> Color {
    let [r, g, b, a] = c.0;
    Color::rgba(r, g, b, a)
}

pub fn family(name: &str) -> FontFamily {
    match name.trim().to_ascii_lowercase().as_str() {
        "" | "monospace" | "mono" => FontFamily::Monospace,
        "sans-serif" | "sans" => FontFamily::SansSerif,
        "serif" => FontFamily::Serif,
        _ => FontFamily::Named(name.trim().to_string()),
    }
}

/// The four faces of the theme's mono font. Sizes are points at scale 1 converted to
/// logical px at 96 dpi, like the other services.
#[derive(Debug, Clone, PartialEq)]
pub struct Fonts {
    pub regular: TextStyle,
    pub bold: TextStyle,
    pub italic: TextStyle,
    pub bold_italic: TextStyle,
}

impl Fonts {
    pub fn from_theme(theme: &Theme) -> Self {
        let regular = TextStyle {
            family: family(&theme.fonts.mono_family),
            line_height: LINE_HEIGHT,
            ..TextStyle::sized(theme.fonts.mono_size.clamp(4.0, 96.0) * 4.0 / 3.0)
        };
        let bold = TextStyle {
            weight: 700,
            ..regular.clone()
        };
        let italic = TextStyle {
            italic: true,
            ..regular.clone()
        };
        let bold_italic = TextStyle {
            italic: true,
            ..bold.clone()
        };
        Self {
            regular,
            bold,
            italic,
            bold_italic,
        }
    }

    fn style(&self, bold: bool, italic: bool) -> &TextStyle {
        match (bold, italic) {
            (false, false) => &self.regular,
            (true, false) => &self.bold,
            (false, true) => &self.italic,
            (true, true) => &self.bold_italic,
        }
    }
}

/// Shaped glyph runs per character and face. Owner: the [`View`]; invalidated by a theme
/// or scale change (everything is shaped for one scale and font); budgeted by
/// [`GLYPH_BUDGET`] with LRU eviction; hit rate and size appear as `perf` lines.
pub struct GlyphCache {
    single: LruCache<(char, u8), Arc<ShapedText>>,
    cluster: LruCache<(String, u8), Arc<ShapedText>>,
}

impl GlyphCache {
    pub fn new() -> Self {
        Self {
            single: LruCache::new("term-glyphs", GLYPH_BUDGET),
            cluster: LruCache::new("term-clusters", GLYPH_BUDGET / 4),
        }
    }

    pub fn clear(&mut self) {
        self.single.clear();
        self.cluster.clear();
    }

    fn shaped(
        &mut self,
        text: &TextSystem,
        fonts: &Fonts,
        scale: f32,
        cell: &CellView<'_>,
    ) -> Arc<ShapedText> {
        let face = cell.attrs.bold as u8 | (cell.attrs.italic as u8) << 1;
        let style = fonts.style(cell.attrs.bold, cell.attrs.italic);
        let bytes = |s: &ShapedText| 96 + s.glyph_count() * 40;
        if cell.zerowidth.is_empty() {
            let key = (cell.ch, face);
            if let Some(s) = self.single.get(&key) {
                return s;
            }
            let mut buf = [0u8; 4];
            let s = text.shape(cell.ch.encode_utf8(&mut buf), style, scale, None);
            self.single.insert(key, s.clone(), bytes(&s));
            s
        } else {
            let mut string = String::from(cell.ch);
            string.extend(cell.zerowidth);
            let key = (string, face);
            if let Some(s) = self.cluster.get(&key) {
                return s;
            }
            let s = text.shape(&key.0, style, scale, None);
            let b = bytes(&s) + key.0.len();
            self.cluster.insert(key, s.clone(), b);
            s
        }
    }
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Final colors of one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPaint {
    pub fg: Rgba,
    /// What to fill behind the cell; the translucent theme background for default cells.
    pub bg: Rgba,
}

/// Decide a cell's colors: palette lookup, dim, inverse, hidden, then selection and block
/// cursor on top.
pub fn cell_paint(
    c: &CellView<'_>,
    scheme: &Scheme,
    over: &dyn Fn(u16) -> Option<Rgba>,
    selected: bool,
    block_cursor: bool,
) -> CellPaint {
    let mut fg = scheme.resolve(c.fg, over);
    if c.attrs.dim {
        fg = scheme.dim(fg);
    }
    let default_bg = c.bg == ColorRef::Index(BACKGROUND);
    let bg = scheme.resolve(c.bg, over);
    // Text drawn over the default background must not inherit its translucency.
    let bg_text = if default_bg { scheme.bg_solid } else { bg };
    let (mut fg, mut bg) = if c.attrs.inverse {
        (bg_text, Rgba([fg.0[0], fg.0[1], fg.0[2], 255]))
    } else {
        (fg, bg)
    };
    if c.attrs.hidden {
        fg = bg;
    }
    if selected {
        bg = scheme.selection;
    }
    if block_cursor {
        bg = scheme.cursor;
        fg = scheme.cursor_text;
    }
    CellPaint { fg, bg }
}

/// A run of equally colored cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub start: usize,
    pub len: usize,
    pub color: Rgba,
}

/// Merge neighbours with the same background into runs.
pub fn bg_runs(colors: impl IntoIterator<Item = Rgba>) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for (i, color) in colors.into_iter().enumerate() {
        match out.last_mut() {
            Some(r) if r.color == color => r.len += 1,
            _ => out.push(Run {
                start: i,
                len: 1,
                color,
            }),
        }
    }
    out
}

/// Everything the canvas paints from, plus the emulator.
pub struct View {
    pub backend: Backend,
    pub geom: Geometry,
    pub scheme: Scheme,
    pub metrics: CellMetrics,
    fonts: Fonts,
    text: TextSystem,
    glyphs: RefCell<GlyphCache>,
    /// Logical size of the surface and its scale.
    size: (f32, f32),
    scale: f32,
    pub focused: bool,
    /// False during the dark half of a cursor blink.
    pub cursor_on: bool,
}

impl View {
    pub fn new(
        text: TextSystem,
        theme: &Theme,
        size: (f32, f32),
        scale: f32,
        scrollback: usize,
    ) -> Self {
        let fonts = Fonts::from_theme(theme);
        let metrics = text.cell_metrics(&fonts.regular, scale);
        let geom = Geometry::new(&metrics, size, PADDING);
        let mut backend = Backend::new(geom.cols, geom.rows, scrollback);
        backend.resize(geom.cols, geom.rows, geom.cell_w, geom.cell_h);
        Self {
            backend,
            geom,
            scheme: Scheme::from_palette(&theme.palette),
            metrics,
            fonts,
            text,
            glyphs: RefCell::new(GlyphCache::new()),
            size,
            scale,
            focused: false,
            cursor_on: true,
        }
    }

    /// Family name for logs.
    pub fn font_name(&self) -> String {
        match &self.fonts.regular.family {
            FontFamily::Named(n) => n.clone(),
            FontFamily::Monospace => "monospace".into(),
            FontFamily::SansSerif => "sans-serif".into(),
            FontFamily::Serif => "serif".into(),
        }
    }

    /// Apply a new theme: colors, fonts, metrics. Returns true when the grid size changed.
    pub fn set_theme(&mut self, theme: &Theme) -> bool {
        self.scheme = Scheme::from_palette(&theme.palette);
        self.fonts = Fonts::from_theme(theme);
        self.glyphs.borrow_mut().clear();
        self.relayout(self.size, self.scale)
    }

    /// The surface changed size or scale. Returns true when the grid size changed.
    pub fn relayout(&mut self, size: (f32, f32), scale: f32) -> bool {
        if (scale - self.scale).abs() > f32::EPSILON {
            self.glyphs.borrow_mut().clear();
        }
        self.size = size;
        self.scale = scale;
        self.metrics = self.text.cell_metrics(&self.fonts.regular, scale);
        let old = (self.geom.cols, self.geom.rows);
        self.geom = Geometry::new(&self.metrics, size, PADDING);
        self.backend.resize(
            self.geom.cols,
            self.geom.rows,
            self.geom.cell_w,
            self.geom.cell_h,
        );
        old != (self.geom.cols, self.geom.rows)
    }

    /// Logical rects to damage for emulator damage.
    pub fn damage_rects(&mut self) -> Vec<Rect> {
        match self.backend.take_damage() {
            Damage::Full => vec![Rect::new(0.0, 0.0, self.size.0, self.size.1)],
            Damage::Rows(rows) => rows
                .into_iter()
                .filter(|r| r.row < self.geom.rows && r.left < self.geom.cols)
                .map(|r| {
                    self.geom
                        .damage(r.row, r.left, r.right.min(self.geom.cols - 1))
                })
                .collect(),
        }
    }

    /// Logical rect of one cell (two for a wide one is not needed: the cursor rect uses
    /// its own width).
    pub fn cursor_rect(&self) -> Option<Rect> {
        let c = self.backend.cursor()?;
        Some(
            self.geom
                .span(c.pos.col, c.pos.row, if c.wide { 2 } else { 1 }),
        )
    }

    fn dev(&self, x: u32, y: u32, w: u32, h: u32) -> Rect {
        let s = self.scale;
        Rect::new(x as f32 / s, y as f32 / s, w as f32 / s, h as f32 / s)
    }

    /// Paint the parts of the canvas inside `area` (logical, within `node`).
    pub fn paint(&self, p: &mut dyn Painter, node: Rect, area: Rect) {
        let started = Instant::now();
        self.paint_margins(p, node, area);
        if let Some((r0, r1, c0, c1)) = self.geom.cells_in(area) {
            let sel = self.backend.selection_view();
            let cursor = self
                .backend
                .cursor()
                .filter(|c| self.cursor_on || !c.blinking || !self.focused);
            let over = |i: u16| self.backend.color_override(i);
            let mut paints: Vec<CellPaint> = Vec::with_capacity(c1 - c0 + 1);
            for row in r0..=r1 {
                paints.clear();
                let cells: Vec<Option<CellView<'_>>> =
                    (c0..=c1).map(|col| self.backend.cell(row, col)).collect();
                for (i, cell) in cells.iter().enumerate() {
                    let col = c0 + i;
                    let Some(cell) = cell else {
                        paints.push(CellPaint {
                            fg: self.scheme.fg,
                            bg: self.scheme.bg,
                        });
                        continue;
                    };
                    let selected = sel.is_some_and(|s| s.contains(row, col));
                    let block = self.focused
                        && cursor.is_some_and(|c| {
                            c.kind == CursorKind::Block
                                && c.pos.row == row
                                && (c.pos.col == col || (c.wide && c.pos.col + 1 == col))
                        });
                    paints.push(cell_paint(cell, &self.scheme, &over, selected, block));
                }
                self.paint_row(p, row, c0, &cells, &paints);
                if let Some(c) = cursor.filter(|c| c.pos.row == row) {
                    self.paint_cursor(p, &c);
                }
            }
        }
        if tracing::enabled!(target: "term", tracing::Level::DEBUG) {
            tracing::debug!(
                target: "term",
                "term: frame ms={:.2}",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    /// The padding and the remainder strip around the grid, in the default background.
    fn paint_margins(&self, p: &mut dyn Painter, node: Rect, area: Rect) {
        let g = self.geom.grid_rect();
        let strips = [
            Rect::new(node.x, node.y, node.w, g.y - node.y),
            Rect::new(node.x, g.bottom(), node.w, node.bottom() - g.bottom()),
            Rect::new(node.x, g.y, g.x - node.x, g.h),
            Rect::new(g.right(), g.y, node.right() - g.right(), g.h),
        ];
        for s in strips {
            if let Some(r) = s.intersect(&area) {
                p.fill_rect(r, to_color(self.scheme.bg));
            }
        }
    }

    fn paint_row(
        &self,
        p: &mut dyn Painter,
        row: usize,
        c0: usize,
        cells: &[Option<CellView<'_>>],
        paints: &[CellPaint],
    ) {
        for run in bg_runs(paints.iter().map(|c| c.bg)) {
            let r = self.geom.span(c0 + run.start, row, run.len);
            p.fill_rect(r, to_color(run.color));
        }
        let g = &self.geom;
        let thick = (self.scale.round() as u32).max(1);
        let top = g.pad + row as u32 * g.cell_h;
        for (i, (cell, paint)) in cells.iter().zip(paints).enumerate() {
            let Some(cell) = cell else { continue };
            if cell.attrs.spacer {
                continue;
            }
            let col = c0 + i;
            let color = to_color(paint.fg);
            if cell.ch != ' ' && cell.ch != '\0' && paint.fg.0[3] > 0 {
                let shaped =
                    self.glyphs
                        .borrow_mut()
                        .shaped(&self.text, &self.fonts, self.scale, cell);
                p.draw_text(&shaped, g.origin(col, row), color);
            }
            let x = g.pad + col as u32 * g.cell_w;
            let w = g.cell_w * if cell.attrs.wide { 2 } else { 1 };
            if cell.attrs.underline > 0 {
                let y = top + (g.baseline + 1).min(g.cell_h.saturating_sub(thick));
                p.fill_rect(self.dev(x, y, w, thick), color);
                if cell.attrs.underline > 1 {
                    let y2 = (y + 2 * thick).min(top + g.cell_h.saturating_sub(thick));
                    p.fill_rect(self.dev(x, y2, w, thick), color);
                }
            }
            if cell.attrs.strike {
                let y = top + (g.baseline as f32 * 0.62) as u32;
                p.fill_rect(self.dev(x, y, w, thick), color);
            }
        }
    }

    /// Cursor shapes other than the focused block (which is a cell color): hollow block
    /// when unfocused, underline, beam.
    fn paint_cursor(&self, p: &mut dyn Painter, c: &crate::backend::CursorView) {
        let g = &self.geom;
        let cols = if c.wide { 2 } else { 1 };
        let (x, y) = (
            g.pad + c.pos.col as u32 * g.cell_w,
            g.pad + c.pos.row as u32 * g.cell_h,
        );
        let w = g.cell_w * cols;
        let thick = (self.scale.round() as u32).max(1);
        let color = to_color(self.scheme.cursor);
        match c.kind {
            CursorKind::Block if self.focused => {}
            CursorKind::Block => {
                let r = self.dev(x, y, w, g.cell_h);
                p.stroke_rounded_rect(r, 0.0, thick as f32 / self.scale, color);
            }
            CursorKind::Underline => {
                let t = 2 * thick;
                p.fill_rect(self.dev(x, y + g.cell_h.saturating_sub(t), w, t), color);
            }
            CursorKind::Beam => p.fill_rect(self.dev(x, y, 2 * thick, g.cell_h), color),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Attrs;
    use aurora_theme::Palette;

    fn scheme() -> Scheme {
        Scheme::from_palette(&Palette::default())
    }

    fn cell(fg: ColorRef, bg: ColorRef, attrs: Attrs) -> CellView<'static> {
        CellView {
            ch: 'x',
            zerowidth: &[],
            fg,
            bg,
            attrs,
            link: false,
        }
    }

    const NONE: &dyn Fn(u16) -> Option<Rgba> = &|_| None;

    fn plain() -> CellView<'static> {
        cell(
            ColorRef::Index(crate::colors::FOREGROUND),
            ColorRef::Index(BACKGROUND),
            Attrs::default(),
        )
    }

    #[test]
    fn default_cells_use_the_translucent_background() {
        let s = scheme();
        let p = cell_paint(&plain(), &s, NONE, false, false);
        assert_eq!((p.fg, p.bg), (s.fg, s.bg));
        assert!(p.bg.0[3] < 255);
    }

    #[test]
    fn explicit_colors_are_opaque_backgrounds() {
        let s = scheme();
        let c = cell(ColorRef::Index(2), ColorRef::Index(4), Attrs::default());
        let p = cell_paint(&c, &s, NONE, false, false);
        assert_eq!((p.fg, p.bg), (s.ansi[2], s.ansi[4]));
    }

    #[test]
    fn inverse_swaps_and_keeps_text_opaque() {
        let s = scheme();
        let mut c = plain();
        c.attrs.inverse = true;
        let p = cell_paint(&c, &s, NONE, false, false);
        // Background becomes the foreground color, text the (opaque) background.
        assert_eq!(&p.bg.0[..3], &s.fg.0[..3]);
        assert_eq!(p.bg.0[3], 255);
        assert_eq!(p.fg, s.bg_solid);
    }

    #[test]
    fn dim_hidden_selection_and_cursor_layer_in_order() {
        let s = scheme();
        let mut c = plain();
        c.attrs.dim = true;
        let dimmed = cell_paint(&c, &s, NONE, false, false);
        assert!(dimmed.fg.0[0] < s.fg.0[0]);

        let mut h = plain();
        h.attrs.hidden = true;
        let hidden = cell_paint(&h, &s, NONE, false, false);
        assert_eq!(hidden.fg, hidden.bg);

        let sel = cell_paint(&plain(), &s, NONE, true, false);
        assert_eq!((sel.fg, sel.bg), (s.fg, s.selection));

        // A block cursor wins over a selection.
        let cur = cell_paint(&plain(), &s, NONE, true, true);
        assert_eq!((cur.fg, cur.bg), (s.cursor_text, s.cursor));
    }

    #[test]
    fn program_set_colors_win() {
        let s = scheme();
        let red = Rgba::rgb(255, 0, 0);
        let over = |i: u16| (i == BACKGROUND).then_some(red);
        let p = cell_paint(&plain(), &s, &over, false, false);
        assert_eq!(p.bg, red);
    }

    #[test]
    fn equal_neighbours_merge_into_runs() {
        let a = Rgba::rgb(1, 1, 1);
        let b = Rgba::rgb(2, 2, 2);
        let runs = bg_runs([a, a, b, a, a, a]);
        assert_eq!(
            runs,
            [
                Run {
                    start: 0,
                    len: 2,
                    color: a
                },
                Run {
                    start: 2,
                    len: 1,
                    color: b
                },
                Run {
                    start: 3,
                    len: 3,
                    color: a
                },
            ]
        );
        assert!(bg_runs([]).is_empty());
    }

    #[test]
    fn fonts_follow_the_theme() {
        let mut theme = Theme::default();
        theme.fonts.mono_family = "Fira Code".into();
        theme.fonts.mono_size = 12.0;
        let f = Fonts::from_theme(&theme);
        assert_eq!(f.regular.family, FontFamily::Named("Fira Code".into()));
        assert_eq!(f.regular.size, 16.0);
        assert_eq!((f.bold.weight, f.bold.italic), (700, false));
        assert_eq!((f.bold_italic.weight, f.bold_italic.italic), (700, true));
        assert_eq!(family("monospace"), FontFamily::Monospace);
        assert_eq!(family(" Hack "), FontFamily::Named("Hack".into()));
    }

    // Headless pixel tests: a real font, the toolkit's software painter, no display.

    type Headless = (std::rc::Rc<RefCell<View>>, aurora_ui::Ui);

    fn headless(w: f32, h: f32, scale: f32) -> Option<Headless> {
        let text = TextSystem::new();
        if !text.has_fonts() {
            return None;
        }
        let theme = Theme::default();
        let view = std::rc::Rc::new(RefCell::new(View::new(
            text.clone(),
            &theme,
            (w, h),
            scale,
            100,
        )));
        let v = view.clone();
        let root = aurora_ui::Node::canvas(move |p, node, area| v.borrow().paint(p, node, area))
            .size(aurora_ui::Dim::Fill(1.0), aurora_ui::Dim::Fill(1.0));
        let mut ui = aurora_ui::Ui::new(text, root);
        ui.set_size(aurora_ui::Size::new(w, h));
        ui.set_scale(scale);
        Some((view, ui))
    }

    fn draw(ui: &mut aurora_ui::Ui, buf: &mut aurora_ui::PixelBuffer, scale: f32) -> Vec<Rect> {
        let mut caches = aurora_ui::PaintCaches::new(ui.text().clone());
        ui.draw(&mut buf.painter(scale, &mut caches))
    }

    /// Any pixel of the cell (col, row) that differs from `base`.
    fn cell_differs(
        buf: &aurora_ui::PixelBuffer,
        g: &Geometry,
        col: usize,
        row: usize,
        base: [u8; 4],
    ) -> bool {
        let (x0, y0) = (g.pad + col as u32 * g.cell_w, g.pad + row as u32 * g.cell_h);
        (y0..y0 + g.cell_h)
            .flat_map(|y| (x0..x0 + g.cell_w).map(move |x| (x, y)))
            .any(|(x, y)| buf.argb(x, y) != base)
    }

    /// Premultiplied ARGB of a straight color over a transparent buffer.
    fn premul(c: Rgba) -> [u8; 4] {
        let m = |v: u8| ((v as u32 * c.0[3] as u32 + 127) / 255) as u8;
        [c.0[3], m(c.0[0]), m(c.0[1]), m(c.0[2])]
    }

    #[test]
    fn text_selection_and_cursor_reach_the_pixels() {
        let Some((view, mut ui)) = headless(400.0, 200.0, 1.0) else {
            return;
        };
        view.borrow_mut().backend.feed(b"Hi\r\n  x");
        let mut buf = aurora_ui::PixelBuffer::new(400, 200);
        draw(&mut ui, &mut buf, 1.0);
        let (g, bg, sel) = {
            let v = view.borrow();
            (v.geom, premul(v.scheme.bg), v.scheme.selection)
        };
        // The margin and an empty cell are the plain background.
        assert_eq!(buf.argb(1, 1), bg);
        assert_eq!(buf.argb(g.pad + 10 * g.cell_w, g.pad + 5 * g.cell_h), bg);
        // 'H' and 'i' left glyph pixels, the blank cells did not.
        assert!(cell_differs(&buf, &g, 0, 0, bg));
        assert!(cell_differs(&buf, &g, 1, 0, bg));
        assert!(!cell_differs(&buf, &g, 5, 0, bg));
        assert!(!cell_differs(&buf, &g, 0, 1, bg));
        // Unfocused: the cursor (after 'x') is an outline, so that cell is not empty.
        assert!(cell_differs(&buf, &g, 3, 1, bg));

        // Selecting the first row paints the selection color behind the text.
        {
            let mut v = view.borrow_mut();
            let pos = |col| crate::backend::Pos { row: 0, col };
            v.backend
                .select_begin(crate::select::SelKind::Simple, pos(0), false);
            v.backend.select_update(pos(3), true);
        }
        ui.invalidate_all();
        draw(&mut ui, &mut buf, 1.0);
        let px = buf.argb(g.pad + 3 * g.cell_w + 1, g.pad + 1);
        assert_eq!(px, [255, sel.0[0], sel.0[1], sel.0[2]]);
    }

    #[test]
    fn damage_repaints_only_the_changed_row() {
        let Some((view, mut ui)) = headless(400.0, 200.0, 1.0) else {
            return;
        };
        let mut buf = aurora_ui::PixelBuffer::new(400, 200);
        draw(&mut ui, &mut buf, 1.0);
        {
            let mut v = view.borrow_mut();
            let _ = v.damage_rects();
            v.backend.feed(b"\x1b[3;1Hzz");
            for r in v.damage_rects() {
                ui.damage(r);
            }
        }
        let d = draw(&mut ui, &mut buf, 1.0);
        let g = view.borrow().geom;
        assert!(!d.is_empty());
        let row2_top = (g.pad + 2 * g.cell_h) as f32;
        let row3_top = (g.pad + 3 * g.cell_h) as f32;
        // Only row 2 (the new text) and the cursor's old cell on row 0 are repainted.
        for r in &d {
            let on_row2 = r.y >= row2_top - 2.0 && r.bottom() <= row3_top + 2.0;
            let on_row0 = r.bottom() <= (g.pad + g.cell_h) as f32 + 2.0;
            assert!(on_row2 || on_row0, "{r:?}");
        }
        assert!(cell_differs(
            &buf,
            &g,
            0,
            2,
            premul(view.borrow().scheme.bg)
        ));
    }

    #[test]
    fn cells_stay_pixel_aligned_at_a_fractional_scale() {
        let Some((view, mut ui)) = headless(300.0, 100.0, 1.5) else {
            return;
        };
        view.borrow_mut().backend.feed(b"\x1b[41m  \x1b[42m  ");
        let mut buf = aurora_ui::PixelBuffer::new(450, 150);
        draw(&mut ui, &mut buf, 1.5);
        let v = view.borrow();
        let g = v.geom;
        let opaque = |c: Rgba| [255, c.0[0], c.0[1], c.0[2]];
        let y = g.pad + g.cell_h / 2;
        let at = |x: u32| buf.argb(x, y);
        let (x0, x2, x4) = (g.pad, g.pad + 2 * g.cell_w, g.pad + 4 * g.cell_w);
        // The color boundaries fall exactly on cell edges, with no blended pixels.
        assert_eq!(at(x0), opaque(v.scheme.ansi[1]));
        assert_eq!(at(x2 - 1), at(x0));
        assert_eq!(at(x2), opaque(v.scheme.ansi[2]));
        assert_eq!(at(x4 - 1), at(x2));
        assert_ne!(at(x4), at(x4 - 1));
    }
}
