//! Geometry and look of the window: where everything is and what a point hits. Pure, so it
//! is tested without painting. The whole window is one `Canvas`; this module is its
//! layout, [`crate::scene`] its painting.

use std::ops::Range;

use aurora_theme::{Rgba, Theme};
use aurora_ui::{Color, FontFamily, Point, Rect, Size, TextStyle};

use crate::model::SortKey;

fn color(c: Rgba) -> Color {
    let [r, g, b, a] = c.0;
    Color::rgba(r, g, b, a)
}

fn family(name: &str) -> FontFamily {
    match name.trim().to_ascii_lowercase().as_str() {
        "" | "sans-serif" | "sans" => FontFamily::SansSerif,
        "serif" => FontFamily::Serif,
        "monospace" | "mono" => FontFamily::Monospace,
        _ => FontFamily::Named(name.trim().to_string()),
    }
}

/// Colors and type derived from the live theme.
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    pub bg: Color,
    pub surface: Color,
    pub selected: Color,
    pub hover: Color,
    pub fg: Color,
    pub fg_dim: Color,
    pub accent: Color,
    pub accent_fg: Color,
    pub urgent: Color,
    pub border: Color,
    pub shadow: Color,
    pub radius: f32,
    pub family: FontFamily,
    /// Base text size in logical px.
    pub base: f32,
}

impl Look {
    pub fn from_theme(t: &Theme) -> Self {
        let p = &t.palette;
        Self {
            bg: color(p.bg),
            surface: color(p.surface),
            selected: color(p.accent).fade(0.28),
            hover: color(p.surface_alt).fade(0.7),
            fg: color(p.fg),
            fg_dim: color(p.fg_dim),
            accent: color(p.accent),
            accent_fg: color(p.accent_fg),
            urgent: color(p.urgent),
            border: color(p.border),
            shadow: color(p.shadow),
            radius: (t.shape.radius as f32).min(12.0),
            family: family(&t.fonts.family),
            // Theme sizes are points at scale 1.
            base: (t.fonts.size * 4.0 / 3.0).clamp(8.0, 48.0),
        }
    }

    /// A text style `k` times the base size.
    pub fn text(&self, k: f32, weight: u16) -> TextStyle {
        TextStyle {
            family: self.family.clone(),
            weight,
            ..TextStyle::sized(self.base * k)
        }
    }
}

impl Default for Look {
    fn default() -> Self {
        Self::from_theme(&Theme::default())
    }
}

/// What a point on the window is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    None,
    Back,
    Forward,
    Up,
    PathBar,
    Place(usize),
    Header(SortKey),
    Row(usize),
    /// The list area below the last row.
    EmptyList,
    Status,
    ModalButton(usize),
}

/// Columns of the list, in logical px of the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Columns {
    pub icon: Rect,
    pub name: Rect,
    /// Right-aligned size column, `None` when the window is too narrow.
    pub size: Option<Rect>,
    pub modified: Option<Rect>,
}

/// All rectangles of the window for a size and a base text size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub size: Size,
    pub row_h: f32,
    pub header_h: f32,
    pub toolbar_h: f32,
    pub status_h: f32,
    pub sidebar_w: f32,
    pub place_h: f32,
    pub pad: f32,
    pub icon: f32,
}

impl Metrics {
    pub fn new(size: Size, base: f32) -> Self {
        let r = |v: f32| v.round();
        Self {
            size,
            row_h: r(base * 2.0).max(22.0),
            header_h: r(base * 1.7).max(20.0),
            toolbar_h: r(base * 3.0).max(36.0),
            status_h: r(base * 2.0).max(22.0),
            sidebar_w: r(base * 13.0).min(r(size.w * 0.35)).max(0.0),
            place_h: r(base * 2.1).max(24.0),
            pad: 8.0,
            icon: r(base * 1.25).max(16.0),
        }
    }

    pub fn toolbar(&self) -> Rect {
        Rect::new(0.0, 0.0, self.size.w, self.toolbar_h)
    }

    pub fn status(&self) -> Rect {
        Rect::new(0.0, self.size.h - self.status_h, self.size.w, self.status_h)
    }

    pub fn sidebar(&self) -> Rect {
        let top = self.toolbar_h;
        Rect::new(
            0.0,
            top,
            self.sidebar_w,
            (self.size.h - top - self.status_h).max(0.0),
        )
    }

    /// The area right of the sidebar, between toolbar and status bar.
    pub fn main(&self) -> Rect {
        let top = self.toolbar_h;
        Rect::new(
            self.sidebar_w,
            top,
            (self.size.w - self.sidebar_w).max(0.0),
            (self.size.h - top - self.status_h).max(0.0),
        )
    }

    pub fn header(&self) -> Rect {
        let m = self.main();
        Rect::new(m.x, m.y, m.w, self.header_h)
    }

    /// The scrolling viewport of rows.
    pub fn list(&self) -> Rect {
        let m = self.main();
        Rect::new(
            m.x,
            m.y + self.header_h,
            m.w,
            (m.h - self.header_h).max(0.0),
        )
    }

    fn button(&self, i: usize) -> Rect {
        let s = (self.toolbar_h - 2.0 * self.pad).max(16.0);
        Rect::new(
            self.pad + i as f32 * (s + 4.0),
            (self.toolbar_h - s) / 2.0,
            s,
            s,
        )
    }

    pub fn back_button(&self) -> Rect {
        self.button(0)
    }

    pub fn forward_button(&self) -> Rect {
        self.button(1)
    }

    pub fn up_button(&self) -> Rect {
        self.button(2)
    }

    pub fn path_bar(&self) -> Rect {
        let up = self.up_button();
        let x = up.right() + self.pad;
        let h = up.h;
        Rect::new(x, up.y, (self.size.w - x - self.pad).max(0.0), h)
    }

    pub fn place_rect(&self, i: usize) -> Rect {
        let s = self.sidebar();
        Rect::new(
            s.x + 4.0,
            s.y + self.pad + i as f32 * self.place_h,
            (s.w - 8.0).max(0.0),
            self.place_h,
        )
    }

    pub fn columns(&self) -> Columns {
        let m = self.main();
        let pad = self.pad + 4.0;
        let size_w = (self.row_h * 3.6).round();
        let mod_w = (self.row_h * 5.6).round();
        let show_mod = m.w >= 520.0;
        let show_size = m.w >= 340.0;
        let mut right = m.right() - pad;
        let modified = show_mod.then(|| {
            let r = Rect::new(right - mod_w, m.y, mod_w, self.row_h);
            right = r.x - 8.0;
            r
        });
        let size = show_size.then(|| {
            let r = Rect::new(right - size_w, m.y, size_w, self.row_h);
            right = r.x - 12.0;
            r
        });
        let icon = Rect::new(m.x + pad, m.y, self.icon, self.row_h);
        let name_x = icon.right() + 8.0;
        Columns {
            icon,
            name: Rect::new(name_x, m.y, (right - name_x).max(0.0), self.row_h),
            size,
            modified,
        }
    }

    pub fn total_height(&self, rows: usize) -> f32 {
        rows as f32 * self.row_h
    }

    pub fn max_scroll(&self, rows: usize) -> f32 {
        (self.total_height(rows) - self.list().h).max(0.0)
    }

    pub fn clamp_scroll(&self, scroll: f32, rows: usize) -> f32 {
        scroll.clamp(0.0, self.max_scroll(rows))
    }

    /// Rows that intersect the viewport.
    pub fn visible_rows(&self, scroll: f32, rows: usize) -> Range<usize> {
        let h = self.list().h;
        let first = (scroll / self.row_h).floor().max(0.0) as usize;
        let last = (((scroll + h) / self.row_h).ceil().max(0.0) as usize).min(rows);
        first.min(rows)..last
    }

    /// The rect of row `i` on screen (may be outside the viewport).
    pub fn row_rect(&self, i: usize, scroll: f32) -> Rect {
        let l = self.list();
        Rect::new(l.x, l.y + i as f32 * self.row_h - scroll, l.w, self.row_h)
    }

    /// The scroll offset that brings row `i` fully into view with the least movement.
    pub fn scroll_to_row(&self, i: usize, scroll: f32, rows: usize) -> f32 {
        let top = i as f32 * self.row_h;
        let bottom = top + self.row_h;
        let h = self.list().h;
        let s = if top < scroll {
            top
        } else if bottom > scroll + h {
            bottom - h
        } else {
            scroll
        };
        self.clamp_scroll(s, rows)
    }

    /// Which header title a point in the header row is over.
    fn header_key(&self, x: f32) -> SortKey {
        let c = self.columns();
        if c.modified.is_some_and(|r| x >= r.x - 4.0) {
            SortKey::Modified
        } else if c.size.is_some_and(|r| x >= r.x - 8.0) {
            SortKey::Size
        } else {
            SortKey::Name
        }
    }

    /// Hit test of the main window (no modal open).
    pub fn hit(&self, p: Point, scroll: f32, rows: usize, places: usize) -> Hit {
        if self.toolbar().contains(p) {
            return if self.back_button().contains(p) {
                Hit::Back
            } else if self.forward_button().contains(p) {
                Hit::Forward
            } else if self.up_button().contains(p) {
                Hit::Up
            } else if self.path_bar().contains(p) {
                Hit::PathBar
            } else {
                Hit::None
            };
        }
        if self.status().contains(p) {
            return Hit::Status;
        }
        if self.sidebar().contains(p) {
            return (0..places)
                .find(|&i| self.place_rect(i).contains(p))
                .map_or(Hit::None, Hit::Place);
        }
        if self.header().contains(p) {
            return Hit::Header(self.header_key(p.x));
        }
        let l = self.list();
        if l.contains(p) {
            let row = ((p.y - l.y + scroll) / self.row_h).floor();
            return if row >= 0.0 && (row as usize) < rows {
                Hit::Row(row as usize)
            } else {
                Hit::EmptyList
            };
        }
        Hit::None
    }

    /// The card of a modal and its buttons (left to right).
    pub fn modal(&self, buttons: usize, lines: usize) -> (Rect, Vec<Rect>) {
        let w = (self.size.w - 40.0).clamp(0.0, 520.0);
        let line_h = self.row_h;
        let h = line_h * (lines as f32 + 1.0) + self.row_h * 1.6;
        let card = Rect::new(
            ((self.size.w - w) / 2.0).round(),
            ((self.size.h - h) / 3.0).round().max(0.0),
            w,
            h,
        );
        let n = buttons.max(1) as f32;
        let gap = 8.0;
        let bw = ((w - 2.0 * self.pad - gap * (n - 1.0)) / n).max(0.0);
        let by = card.bottom() - self.pad - self.row_h * 1.2;
        let rects = (0..buttons)
            .map(|i| {
                Rect::new(
                    card.x + self.pad + i as f32 * (bw + gap),
                    by,
                    bw,
                    self.row_h * 1.2,
                )
            })
            .collect();
        (card, rects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Metrics {
        Metrics::new(Size::new(1000.0, 700.0), 14.0)
    }

    #[test]
    fn regions_tile_the_window() {
        let m = m();
        assert_eq!(m.toolbar().bottom(), m.sidebar().y);
        assert_eq!(m.sidebar().bottom(), m.status().y);
        assert_eq!(m.status().bottom(), 700.0);
        assert_eq!(m.sidebar().right(), m.main().x);
        assert_eq!(m.main().right(), 1000.0);
        assert_eq!(m.header().bottom(), m.list().y);
        assert_eq!(m.list().bottom(), m.status().y);
        let p = m.path_bar();
        assert!(p.x > m.up_button().right() && p.right() < 1000.0);
    }

    #[test]
    fn narrow_windows_drop_columns_then_the_sidebar_shrinks() {
        let wide = Metrics::new(Size::new(1000.0, 600.0), 14.0).columns();
        assert!(wide.size.is_some() && wide.modified.is_some());
        let mid = Metrics::new(Size::new(700.0, 600.0), 14.0).columns();
        assert!(mid.size.is_some() && mid.modified.is_none());
        let tiny = Metrics::new(Size::new(300.0, 600.0), 14.0);
        assert!(tiny.columns().size.is_none());
        assert!(tiny.sidebar_w <= 105.0);
        assert!(tiny.columns().name.w > 0.0);
        // Name column never overlaps the others.
        let c = wide;
        assert!(c.name.right() <= c.size.unwrap().x);
        assert!(c.size.unwrap().right() <= c.modified.unwrap().x);
    }

    #[test]
    fn virtualization_math() {
        let m = m();
        let rows = 100_000;
        let list_h = m.list().h;
        assert_eq!(m.visible_rows(0.0, rows).start, 0);
        let v = m.visible_rows(0.0, rows);
        assert_eq!(v.end, (list_h / m.row_h).ceil() as usize);
        assert!(v.end < 40, "only a screenful is laid out: {v:?}");
        let s = m.max_scroll(rows);
        assert_eq!(m.visible_rows(s, rows).end, rows);
        assert_eq!(m.clamp_scroll(-5.0, rows), 0.0);
        assert_eq!(m.clamp_scroll(1e12, rows), s);
        assert_eq!(m.max_scroll(3), 0.0, "short lists do not scroll");
        assert_eq!(m.visible_rows(0.0, 3), 0..3);
        assert_eq!(m.visible_rows(0.0, 0), 0..0);
        // Fractional scroll keeps the partly visible rows.
        let v = m.visible_rows(m.row_h * 10.5, rows);
        assert_eq!(v.start, 10);
    }

    #[test]
    fn scroll_to_row_moves_minimally() {
        let m = m();
        let rows = 1000;
        assert_eq!(m.scroll_to_row(0, 0.0, rows), 0.0);
        let h = m.list().h;
        let below = m.scroll_to_row(100, 0.0, rows);
        assert_eq!(below, 101.0 * m.row_h - h);
        assert_eq!(m.scroll_to_row(100, below, rows), below, "already visible");
        assert_eq!(m.scroll_to_row(5, below, rows), 5.0 * m.row_h);
    }

    #[test]
    fn hit_testing() {
        let m = m();
        let mid = |r: Rect| Point::new(r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert_eq!(m.hit(mid(m.back_button()), 0.0, 10, 3), Hit::Back);
        assert_eq!(m.hit(mid(m.forward_button()), 0.0, 10, 3), Hit::Forward);
        assert_eq!(m.hit(mid(m.up_button()), 0.0, 10, 3), Hit::Up);
        assert_eq!(m.hit(mid(m.path_bar()), 0.0, 10, 3), Hit::PathBar);
        assert_eq!(m.hit(mid(m.place_rect(2)), 0.0, 10, 3), Hit::Place(2));
        assert_eq!(m.hit(mid(m.place_rect(3)), 0.0, 10, 3), Hit::None);
        assert_eq!(m.hit(mid(m.status()), 0.0, 10, 3), Hit::Status);
        let cols = m.columns();
        let hy = m.header().y + 2.0;
        assert_eq!(
            m.hit(Point::new(cols.name.x + 5.0, hy), 0.0, 10, 3),
            Hit::Header(SortKey::Name)
        );
        assert_eq!(
            m.hit(Point::new(mid(cols.size.unwrap()).x, hy), 0.0, 10, 3),
            Hit::Header(SortKey::Size)
        );
        assert_eq!(
            m.hit(Point::new(cols.modified.unwrap().x + 5.0, hy), 0.0, 10, 3),
            Hit::Header(SortKey::Modified)
        );
        assert_eq!(m.hit(mid(m.row_rect(4, 0.0)), 0.0, 10, 3), Hit::Row(4));
        // Scrolled by one and a half rows.
        let sc = m.row_h * 1.5;
        assert_eq!(m.hit(mid(m.row_rect(4, sc)), sc, 10, 3), Hit::Row(4));
        assert_eq!(m.hit(mid(m.row_rect(12, 0.0)), 0.0, 10, 3), Hit::EmptyList);
        assert_eq!(m.hit(Point::new(-1.0, -1.0), 0.0, 10, 3), Hit::None);
    }

    #[test]
    fn modal_buttons_sit_inside_the_card() {
        let m = m();
        let (card, buttons) = m.modal(4, 2);
        assert_eq!(buttons.len(), 4);
        for b in &buttons {
            assert!(
                b.x >= card.x && b.right() <= card.right() + 0.01,
                "{b:?} {card:?}"
            );
            assert!(b.y >= card.y && b.bottom() <= card.bottom());
        }
        assert!(buttons[0].right() < buttons[1].x);
        let tiny = Metrics::new(Size::new(30.0, 30.0), 14.0);
        let (_, b) = tiny.modal(2, 1);
        assert!(b.iter().all(|r| r.w >= 0.0));
    }

    #[test]
    fn look_follows_the_theme() {
        let mut t = Theme::default();
        t.palette.accent = Rgba::rgb(1, 2, 3);
        t.fonts.family = "Inter".into();
        t.fonts.size = 12.0;
        let look = Look::from_theme(&t);
        assert_eq!((look.accent.r, look.accent.g, look.accent.b), (1, 2, 3));
        assert_eq!(look.family, FontFamily::Named("Inter".into()));
        assert_eq!(look.base, 16.0);
        assert_eq!(look.text(0.5, 700).size, 8.0);
        assert_eq!(family("Sans-Serif"), FontFamily::SansSerif);
    }
}
