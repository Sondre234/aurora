//! Everything the canvas paints, and the painting itself. The app mutates a [`Scene`] and
//! damages regions; the canvas callback paints from it. Only the rows that are on screen are
//! ever shaped or drawn, however long the directory is.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::PathBuf;

use aurora_launcher::icons::IconCache;
use aurora_ui::{Color, Image, Painter, Point, Rect, Size, TextStyle, TextSystem};

use crate::edit::LineEdit;
use crate::model::{Browser, Entry, Kind, SortKey, format_mtime, format_size};
use crate::places::{Place, active_place};
use crate::system::icon_name;
use crate::view::{Hit, Look, Metrics};

/// What the keyboard is currently typing into, if anything.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Editing {
    #[default]
    None,
    Path(LineEdit),
    Filter(LineEdit),
    /// Inline rename of the entry named `target`.
    Rename {
        target: OsString,
        edit: LineEdit,
    },
    /// Name prompt for a new folder or file.
    New {
        dir: bool,
        edit: LineEdit,
    },
}

impl Editing {
    pub fn edit_mut(&mut self) -> Option<&mut LineEdit> {
        match self {
            Editing::None => None,
            Editing::Path(e) | Editing::Filter(e) => Some(e),
            Editing::Rename { edit, .. } | Editing::New { edit, .. } => Some(edit),
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Editing::None)
    }
}

/// A question that blocks the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modal {
    pub title: String,
    pub lines: Vec<String>,
    pub buttons: Vec<String>,
    /// `Some` on conflicts: the "apply to all" checkbox.
    pub apply_all: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub error: bool,
}

/// A running operation for the status bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Busy {
    pub text: String,
    /// 0..=1, or negative when unknown.
    pub fraction: f32,
}

pub struct Scene {
    pub look: Look,
    pub browser: Browser,
    pub places: Vec<Place>,
    pub editing: Editing,
    pub modal: Option<Modal>,
    pub toast: Option<Toast>,
    pub busy: Option<Busy>,
    pub scroll: f32,
    pub hover: Hit,
    pub size: Size,
    pub text: TextSystem,
    pub icons: IconCache,
    pub focused: bool,
    pub loading: bool,
    /// Entries of `cut_dir` that a pending cut will move: drawn dimmed.
    pub cut_dir: Option<PathBuf>,
    pub cut: HashSet<OsString>,
}

impl Scene {
    pub fn new(look: Look, text: TextSystem, start: PathBuf, icon_budget: usize) -> Self {
        Self {
            look,
            browser: Browser::new(start),
            places: Vec::new(),
            editing: Editing::None,
            modal: None,
            toast: None,
            busy: None,
            scroll: 0.0,
            hover: Hit::None,
            size: Size::new(800.0, 600.0),
            text,
            icons: IconCache::new(icon_budget),
            focused: true,
            loading: true,
            cut_dir: None,
            cut: HashSet::new(),
        }
    }

    pub fn metrics(&self) -> Metrics {
        Metrics::new(self.size, self.look.base)
    }

    pub fn is_cut(&self, name: &OsString) -> bool {
        self.cut_dir.as_deref() == Some(self.browser.path()) && self.cut.contains(name)
    }

    /// Paints the part of the window inside `area` (logical px).
    pub fn paint(&mut self, p: &mut dyn Painter, area: Rect) {
        let m = self.metrics();
        let whole = Rect::new(0.0, 0.0, self.size.w, self.size.h);
        p.fill_rect(whole, self.look.bg);
        if m.toolbar().intersects(&area) {
            self.paint_toolbar(p, &m);
        }
        if m.sidebar().intersects(&area) {
            self.paint_sidebar(p, &m);
        }
        if m.header().intersects(&area) {
            self.paint_header(p, &m);
        }
        if m.list().intersects(&area) {
            self.paint_list(p, &m, area);
        }
        if m.status().intersects(&area) {
            self.paint_status(p, &m);
        }
        if self.modal.is_some() {
            self.paint_modal(p, &m);
        }
    }

    fn style(&self, k: f32, weight: u16) -> TextStyle {
        self.look.text(k, weight)
    }

    fn paint_toolbar(&self, p: &mut dyn Painter, m: &Metrics) {
        let l = &self.look;
        p.fill_rect(m.toolbar(), l.surface.fade(0.6));
        p.fill_rect(Rect::new(0.0, m.toolbar_h - 1.0, m.size.w, 1.0), l.border);
        let h = &self.browser.history;
        for (rect, glyph, hit, enabled) in [
            (m.back_button(), "←", Hit::Back, h.can_back()),
            (m.forward_button(), "→", Hit::Forward, h.can_forward()),
            (
                m.up_button(),
                "↑",
                Hit::Up,
                self.browser.path().parent().is_some(),
            ),
        ] {
            if enabled && self.hover == hit {
                p.fill_rounded_rect(rect, l.radius, l.hover);
            }
            let c = if enabled { l.fg } else { l.fg_dim.fade(0.4) };
            draw_text(
                &self.text,
                p,
                glyph,
                &self.style(1.2, 400),
                c,
                rect,
                rect.h / 2.0,
                Align::Center,
            );
        }

        let bar = m.path_bar();
        p.fill_rounded_rect(bar, l.radius, l.surface);
        let editing = matches!(self.editing, Editing::Path(_));
        let border = if editing { l.accent } else { l.border };
        p.stroke_rounded_rect(bar, l.radius, 1.0, border);
        let inner = bar.inset(aurora_ui::Insets::xy(10.0, 0.0));
        let style = self.style(1.0, 400);
        match &self.editing {
            Editing::Path(e) => self.paint_edit(p, e, inner, bar.y + bar.h / 2.0, &style),
            _ => {
                let path = self.browser.path().display().to_string();
                draw_text(
                    &self.text,
                    p,
                    &path,
                    &style,
                    l.fg,
                    inner,
                    bar.y + bar.h / 2.0,
                    Align::Left,
                );
            }
        }
    }

    /// Text with selection and caret inside `area`, vertically centered on `cy`.
    fn paint_edit(
        &self,
        p: &mut dyn Painter,
        e: &LineEdit,
        area: Rect,
        cy: f32,
        style: &TextStyle,
    ) {
        let l = &self.look;
        let shaped = self.text.shape(e.text(), style, p.scale(), None);
        let h = shaped.height();
        // Keep the caret in view by scrolling the text left when it is long.
        let caret_x = shaped.cursor_x(e.cursor());
        let shift = (caret_x - (area.w - 6.0)).max(0.0);
        p.save();
        p.clip_rect(area);
        if let Some((a, b)) = e.selection() {
            let (xa, xb) = (shaped.cursor_x(a), shaped.cursor_x(b));
            p.fill_rect(
                Rect::new(area.x + xa - shift, cy - h / 2.0, xb - xa, h),
                l.accent.fade(0.4),
            );
        }
        p.draw_text(&shaped, Point::new(area.x - shift, cy - h / 2.0), l.fg);
        p.fill_rect(
            Rect::new(area.x + caret_x - shift, cy - h / 2.0, 1.5, h),
            l.accent,
        );
        p.restore();
    }

    fn paint_sidebar(&mut self, p: &mut dyn Painter, m: &Metrics) {
        let l = self.look.clone();
        let s = m.sidebar();
        p.fill_rect(s, l.surface.fade(0.35));
        p.fill_rect(Rect::new(s.right() - 1.0, s.y, 1.0, s.h), l.border);
        let active = active_place(&self.places, self.browser.path());
        p.save();
        p.clip_rect(s);
        for i in 0..self.places.len() {
            let r = m.place_rect(i);
            if r.y > s.bottom() {
                break;
            }
            if active == Some(i) {
                p.fill_rounded_rect(r, l.radius, l.selected);
            } else if self.hover == Hit::Place(i) {
                p.fill_rounded_rect(r, l.radius, l.hover);
            }
            let icon = Rect::new(r.x + 8.0, r.y + (r.h - m.icon) / 2.0, m.icon, m.icon);
            let name = self.places[i].icon;
            let img = self.icons.get(name).flatten();
            draw_icon(p, &l, img, icon, IconShape::Place);
            let label_area = Rect::new(
                icon.right() + 8.0,
                r.y,
                (r.right() - icon.right() - 14.0).max(0.0),
                r.h,
            );
            draw_text(
                &self.text,
                p,
                &self.places[i].label,
                &self.style(1.0, 400),
                l.fg,
                label_area,
                r.y + r.h / 2.0,
                Align::Left,
            );
        }
        p.restore();
    }

    fn paint_header(&self, p: &mut dyn Painter, m: &Metrics) {
        let l = &self.look;
        let h = m.header();
        p.fill_rect(h, l.surface.fade(0.25));
        p.fill_rect(Rect::new(h.x, h.bottom() - 1.0, h.w, 1.0), l.border);
        let c = m.columns();
        let sort = self.browser.sort;
        let arrow = if sort.descending { " ▼" } else { " ▲" };
        let title = |key: SortKey, name: &str| {
            if sort.key == key {
                format!("{name}{arrow}")
            } else {
                name.to_string()
            }
        };
        let cy = h.y + h.h / 2.0;
        let style = self.style(0.88, 600);
        let color = |key: SortKey| if sort.key == key { l.fg } else { l.fg_dim };
        draw_text(
            &self.text,
            p,
            &title(SortKey::Name, "Name"),
            &style,
            color(SortKey::Name),
            c.name,
            cy,
            Align::Left,
        );
        if let Some(r) = c.size {
            draw_text(
                &self.text,
                p,
                &title(SortKey::Size, "Size"),
                &style,
                color(SortKey::Size),
                r,
                cy,
                Align::Right,
            );
        }
        if let Some(r) = c.modified {
            draw_text(
                &self.text,
                p,
                &title(SortKey::Modified, "Modified"),
                &style,
                color(SortKey::Modified),
                r,
                cy,
                Align::Left,
            );
        }
    }

    fn paint_list(&mut self, p: &mut dyn Painter, m: &Metrics, area: Rect) {
        let l = self.look.clone();
        let list = m.list();
        p.save();
        p.clip_rect(list);
        let rows = self.browser.len();
        if rows == 0 {
            self.paint_empty(p, m);
        }
        let c = m.columns();
        let focused = self.focused;
        let cursor = self.browser.selection.cursor();
        let text_style = self.style(1.0, 400);
        let small = self.style(0.9, 400);
        for r in m.visible_rows(self.scroll, rows) {
            let rr = m.row_rect(r, self.scroll);
            if !rr.intersects(&area) {
                continue;
            }
            let Some(e) = self.browser.entry(r) else {
                continue;
            };
            let selected = self.browser.selection.contains(&e.name);
            let band = rr.inset(aurora_ui::Insets::xy(4.0, 1.0));
            if selected {
                p.fill_rounded_rect(band, l.radius.min(8.0), l.selected);
            } else if self.hover == Hit::Row(r) {
                p.fill_rounded_rect(band, l.radius.min(8.0), l.hover);
            }
            if focused && cursor == Some(r) {
                p.stroke_rounded_rect(band, l.radius.min(8.0), 1.0, l.accent.fade(0.8));
            }
            let dim = self.is_cut(&e.name);
            let fade = |col: Color| if dim { col.fade(0.45) } else { col };
            let icon_rect = Rect::new(c.icon.x, rr.y + (rr.h - m.icon) / 2.0, m.icon, m.icon);
            let img: Option<Image> = self.icons.get(icon_name(e)).flatten();
            let shape = if e.is_dir {
                IconShape::Folder
            } else {
                IconShape::File
            };
            draw_icon(p, &l, img, icon_rect, shape);
            let name_color = match (e.kind, e.hidden) {
                (Kind::Broken, _) => l.urgent,
                (_, true) => l.fg_dim,
                _ => l.fg,
            };
            let cy = rr.y + rr.h / 2.0;
            let renaming = match &self.editing {
                Editing::Rename { target, edit } if *target == e.name => Some(edit),
                _ => None,
            };
            if let Some(edit) = renaming {
                let field = Rect::new(c.name.x - 4.0, rr.y + 2.0, c.name.w + 4.0, rr.h - 4.0);
                p.fill_rounded_rect(field, 4.0, l.surface);
                p.stroke_rounded_rect(field, 4.0, 1.0, l.accent);
                self.paint_edit(
                    p,
                    edit,
                    field.inset(aurora_ui::Insets::xy(4.0, 0.0)),
                    cy,
                    &text_style,
                );
            } else {
                draw_text(
                    &self.text,
                    p,
                    &e.display_name(),
                    &text_style,
                    fade(name_color),
                    c.name,
                    cy,
                    Align::Left,
                );
            }
            if let Some(sr) = c.size
                && !e.is_dir
            {
                draw_text(
                    &self.text,
                    p,
                    &format_size(e.size),
                    &small,
                    fade(l.fg_dim),
                    Rect::new(sr.x, rr.y, sr.w, rr.h),
                    cy,
                    Align::Right,
                );
            }
            if let Some(mr) = c.modified {
                draw_text(
                    &self.text,
                    p,
                    &format_mtime(e.mtime),
                    &small,
                    fade(l.fg_dim),
                    Rect::new(mr.x, rr.y, mr.w, rr.h),
                    cy,
                    Align::Left,
                );
            }
        }
        // Scrollbar thumb.
        let total = m.total_height(rows);
        if total > list.h {
            let track = list.h;
            let thumb_h = (track * list.h / total).max(24.0);
            let max = m.max_scroll(rows);
            let y = list.y + (track - thumb_h) * (self.scroll / max);
            p.fill_rounded_rect(
                Rect::new(list.right() - 6.0, y, 4.0, thumb_h),
                2.0,
                l.fg_dim.fade(0.45),
            );
        }
        p.restore();
    }

    fn paint_empty(&self, p: &mut dyn Painter, m: &Metrics) {
        let l = &self.look;
        let list = m.list();
        let (text, color) = if let Some(err) = &self.browser.error {
            (err.clone(), l.urgent)
        } else if self.loading {
            ("Loading…".to_string(), l.fg_dim)
        } else if !self.browser.filter.is_empty() {
            ("No matching files".to_string(), l.fg_dim)
        } else {
            ("Empty folder".to_string(), l.fg_dim)
        };
        let area = list.inset(aurora_ui::Insets::xy(24.0, 0.0));
        draw_text(
            &self.text,
            p,
            &text,
            &self.style(1.05, 400),
            color,
            area,
            list.y + list.h * 0.35,
            Align::Center,
        );
    }

    fn paint_status(&self, p: &mut dyn Painter, m: &Metrics) {
        let l = &self.look;
        let r = m.status();
        p.fill_rect(r, l.surface.fade(0.6));
        p.fill_rect(Rect::new(0.0, r.y, r.w, 1.0), l.border);
        let cy = r.y + r.h / 2.0;
        let style = self.style(0.92, 400);
        let area = r.inset(aurora_ui::Insets::xy(12.0, 0.0));
        let right = counts_text(&self.browser);
        let right_w = self.text.shape(&right, &style, p.scale(), None).width();
        let left_area = Rect::new(area.x, area.y, (area.w - right_w - 16.0).max(0.0), area.h);
        draw_text(
            &self.text,
            p,
            &right,
            &style,
            l.fg_dim,
            area,
            cy,
            Align::Right,
        );
        match (&self.editing, &self.busy, &self.toast) {
            (Editing::Filter(e), ..) => {
                let prompt = "/ ";
                let pw = self.text.shape(prompt, &style, p.scale(), None).width();
                draw_text(
                    &self.text,
                    p,
                    prompt,
                    &style,
                    l.accent,
                    left_area,
                    cy,
                    Align::Left,
                );
                let field = Rect::new(
                    left_area.x + pw,
                    left_area.y,
                    (left_area.w - pw).max(0.0),
                    left_area.h,
                );
                self.paint_edit(p, e, field, cy, &style);
            }
            (Editing::New { dir, edit }, ..) => {
                let prompt = if *dir { "New folder: " } else { "New file: " };
                let pw = self.text.shape(prompt, &style, p.scale(), None).width();
                draw_text(
                    &self.text,
                    p,
                    prompt,
                    &style,
                    l.accent,
                    left_area,
                    cy,
                    Align::Left,
                );
                let field = Rect::new(
                    left_area.x + pw,
                    left_area.y,
                    (left_area.w - pw).max(0.0),
                    left_area.h,
                );
                self.paint_edit(p, edit, field, cy, &style);
            }
            (_, Some(b), _) => {
                draw_text(
                    &self.text,
                    p,
                    &b.text,
                    &style,
                    l.fg,
                    left_area,
                    cy,
                    Align::Left,
                );
                if b.fraction >= 0.0 {
                    let bar =
                        Rect::new(r.x, r.bottom() - 3.0, r.w * b.fraction.clamp(0.0, 1.0), 3.0);
                    p.fill_rect(bar, l.accent);
                }
            }
            (_, None, Some(t)) => {
                let c = if t.error { l.urgent } else { l.fg };
                draw_text(
                    &self.text,
                    p,
                    &t.text,
                    &style,
                    c,
                    left_area,
                    cy,
                    Align::Left,
                );
            }
            _ => {}
        }
    }

    fn paint_modal(&self, p: &mut dyn Painter, m: &Metrics) {
        let Some(modal) = &self.modal else { return };
        let l = &self.look;
        p.fill_rect(
            Rect::new(0.0, 0.0, m.size.w, m.size.h),
            Color::rgba(0, 0, 0, 120),
        );
        let extra = usize::from(modal.apply_all.is_some());
        let (card, buttons) = m.modal(modal.buttons.len(), modal.lines.len() + extra + 1);
        p.shadow(card, l.radius + 4.0, 32.0, Point::new(0.0, 10.0), l.shadow);
        p.fill_rounded_rect(
            card,
            l.radius + 4.0,
            l.surface.fade(1.0).mix(Color::BLACK, 0.1),
        );
        p.stroke_rounded_rect(card, l.radius + 4.0, 1.0, l.border);
        let inner = card.inset(aurora_ui::Insets::xy(m.pad + 6.0, 0.0));
        let mut y = card.y + m.row_h;
        draw_text(
            &self.text,
            p,
            &modal.title,
            &self.style(1.1, 700),
            l.fg,
            inner,
            y,
            Align::Left,
        );
        for line in &modal.lines {
            y += m.row_h;
            draw_text(
                &self.text,
                p,
                line,
                &self.style(0.95, 400),
                l.fg_dim,
                inner,
                y,
                Align::Left,
            );
        }
        if let Some(all) = modal.apply_all {
            y += m.row_h;
            let mark = if all {
                "[x] Apply to all (a)"
            } else {
                "[ ] Apply to all (a)"
            };
            draw_text(
                &self.text,
                p,
                mark,
                &self.style(0.95, 400),
                l.fg,
                inner,
                y,
                Align::Left,
            );
        }
        for (i, (rect, label)) in buttons.iter().zip(&modal.buttons).enumerate() {
            let primary = i == 0 && modal.buttons.len() > 1 && modal.apply_all.is_none();
            let bg = if self.hover == Hit::ModalButton(i) {
                l.hover.mix(l.accent, 0.25)
            } else if primary {
                l.accent.fade(0.5)
            } else {
                l.hover
            };
            p.fill_rounded_rect(*rect, l.radius, bg);
            draw_text(
                &self.text,
                p,
                label,
                &self.style(0.95, 600),
                l.fg,
                *rect,
                rect.y + rect.h / 2.0,
                Align::Center,
            );
        }
    }
}

/// "12 items" or "3 of 12 selected".
pub fn counts_text(b: &Browser) -> String {
    let total = b.len();
    let sel = b.selection.count();
    let noun = |n: usize| if n == 1 { "item" } else { "items" };
    if sel > 0 {
        format!("{sel} of {total} selected")
    } else if !b.filter.is_empty() {
        format!("{total} {} match", noun(total))
    } else {
        format!("{total} {}", noun(total))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

/// Draws one ellipsized line inside `area`'s x range, vertically centered on `cy`.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    text: &TextSystem,
    p: &mut dyn Painter,
    s: &str,
    style: &TextStyle,
    color: Color,
    area: Rect,
    cy: f32,
    align: Align,
) {
    if s.is_empty() || area.w <= 1.0 {
        return;
    }
    let shaped = text.shape(s, style, p.scale(), Some(area.w));
    let w = shaped.width().min(area.w);
    let x = match align {
        Align::Left => area.x,
        Align::Right => area.right() - w,
        Align::Center => area.x + (area.w - w) / 2.0,
    };
    p.draw_text(&shaped, Point::new(x, cy - shaped.height() / 2.0), color);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconShape {
    Folder,
    File,
    Place,
}

/// An icon from the theme, or a plain drawn stand-in while it loads or when the theme
/// has none.
fn draw_icon(p: &mut dyn Painter, l: &Look, img: Option<Image>, r: Rect, shape: IconShape) {
    if let Some(img) = img {
        p.draw_image(&img, r, None);
        return;
    }
    match shape {
        IconShape::Folder | IconShape::Place => {
            let tab = Rect::new(r.x, r.y + r.h * 0.12, r.w * 0.45, r.h * 0.2);
            p.fill_rounded_rect(tab, 2.0, l.accent.fade(0.8));
            let body = Rect::new(r.x, r.y + r.h * 0.26, r.w, r.h * 0.62);
            p.fill_rounded_rect(body, 3.0, l.accent);
        }
        IconShape::File => {
            let body = Rect::new(r.x + r.w * 0.14, r.y + r.h * 0.05, r.w * 0.72, r.h * 0.9);
            p.fill_rounded_rect(body, 3.0, l.fg_dim.fade(0.35));
            p.stroke_rounded_rect(body, 3.0, 1.0, l.fg_dim);
        }
    }
}

/// Distinct freedesktop icon names that the entries of a listing and the places need.
pub fn icon_names(entries: &[Entry], places: &[Place]) -> Vec<&'static str> {
    let mut seen: HashSet<&'static str> = HashSet::new();
    let mut out = Vec::new();
    for n in places
        .iter()
        .map(|p| p.icon)
        .chain(entries.iter().map(icon_name))
    {
        if seen.insert(n) {
            out.push(n);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Sort, sort_entries};
    use crate::places::PlaceKind;
    use aurora_ui::{PaintCaches, PixelBuffer};

    fn scene_with(n: usize) -> Scene {
        let mut s = Scene::new(
            Look::default(),
            TextSystem::new(),
            PathBuf::from("/x"),
            1 << 20,
        );
        let mut es: Vec<Entry> = (0..n)
            .map(|i| Entry::new(format!("file{i}"), Kind::File))
            .collect();
        sort_entries(&mut es, Sort::default());
        s.browser.set_entries(es);
        s.loading = false;
        s.size = Size::new(800.0, 500.0);
        s
    }

    #[test]
    fn counts_wording() {
        let mut s = scene_with(3);
        assert_eq!(counts_text(&s.browser), "3 items");
        s.browser.click(1, false, false);
        assert_eq!(counts_text(&s.browser), "1 of 3 selected");
        s.browser.selection.clear();
        s.browser.set_filter("file1");
        assert_eq!(counts_text(&s.browser), "1 item match");
        let one = scene_with(1);
        assert_eq!(counts_text(&one.browser), "1 item");
    }

    #[test]
    fn icon_names_are_distinct() {
        let es = vec![
            Entry::new("a.txt", Kind::File),
            Entry::new("b.txt", Kind::File),
            Entry::new("d", Kind::Dir),
        ];
        let places = vec![Place {
            label: "Home".into(),
            path: PathBuf::from("/h"),
            kind: PlaceKind::Home,
            icon: "user-home",
        }];
        assert_eq!(
            icon_names(&es, &places),
            ["user-home", "text-x-generic", "folder"]
        );
    }

    #[test]
    fn painting_a_huge_listing_is_cheap_and_selection_shows() {
        let text = TextSystem::new();
        if !text.has_fonts() {
            return;
        }
        let mut s = scene_with(100_000);
        s.text = text.clone();
        s.browser.click(2, false, false);
        let mut buf = PixelBuffer::new(800, 500);
        let mut caches = PaintCaches::new(text);
        let area = Rect::new(0.0, 0.0, 800.0, 500.0);
        let start = std::time::Instant::now();
        {
            let mut painter = buf.painter(1.0, &mut caches);
            s.paint(&mut painter, area);
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        // The selected row is tinted differently from its neighbour.
        let m = s.metrics();
        let sel = m.row_rect(2, 0.0);
        let other = m.row_rect(4, 0.0);
        let px = |r: Rect| buf.pixel((r.right() - 40.0) as u32, (r.y + r.h / 2.0) as u32);
        assert_ne!(px(sel), px(other));
    }

    #[test]
    fn paint_survives_every_state() {
        let text = TextSystem::new();
        let mut s = scene_with(5);
        s.text = text.clone();
        let mut buf = PixelBuffer::new(800, 500);
        let mut caches = PaintCaches::new(text);
        let area = Rect::new(0.0, 0.0, 800.0, 500.0);
        let mut paint = |s: &mut Scene| {
            let mut painter = buf.painter(1.0, &mut caches);
            s.paint(&mut painter, area);
        };
        paint(&mut s);
        s.editing = Editing::Rename {
            target: "file1".into(),
            edit: LineEdit::with_stem_selected("file1"),
        };
        paint(&mut s);
        s.editing = Editing::Path(LineEdit::new("/very/long/path"));
        paint(&mut s);
        s.editing = Editing::Filter(LineEdit::new("fi"));
        paint(&mut s);
        s.editing = Editing::New {
            dir: true,
            edit: LineEdit::new("New folder"),
        };
        paint(&mut s);
        s.editing = Editing::None;
        s.busy = Some(Busy {
            text: "Copying 1/2".into(),
            fraction: 0.4,
        });
        paint(&mut s);
        s.busy = None;
        s.toast = Some(Toast {
            text: "oops".into(),
            error: true,
        });
        paint(&mut s);
        s.modal = Some(Modal {
            title: "Replace?".into(),
            lines: vec!["a".into()],
            buttons: vec!["Skip".into(), "Replace".into()],
            apply_all: Some(false),
        });
        paint(&mut s);
        s.modal = None;
        s.browser
            .set_error("Cannot open /x: Permission denied".into());
        paint(&mut s);
        s.size = Size::new(60.0, 60.0);
        let mut tiny = PixelBuffer::new(60, 60);
        let mut painter = tiny.painter(1.0, &mut caches);
        s.paint(&mut painter, Rect::new(0.0, 0.0, 60.0, 60.0));
    }
}
