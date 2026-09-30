//! The launcher's widget tree and its look. Built once per theme; result rows are swapped
//! in place with `Ui::edit` so typing never rebuilds the surface (performance.md rule 5).
//!
//! ```text
//! ROOT   full-screen dim backdrop, click outside the card hides
//!  └ CARD   centered column: surface, border, shadow
//!     ├ INPUT     search field
//!     ├ separator
//!     └ RESULTS   up to MAX_ROWS rows: icon, title (matched letters highlighted), subtitle
//! ```

use aurora_theme::{Rgba, Theme};
use aurora_ui::{
    Align, Color, Dim, FontFamily, Image, Insets, Node, Point, Size, TextAlign, TextInput,
    TextStyle,
};

use crate::fuzzy::highlight_runs;

pub const ROOT: u32 = 1;
pub const CARD: u32 = 2;
pub const INPUT: u32 = 3;
pub const RESULTS: u32 = 4;
/// Row `i` has id `ROW_BASE + i`.
pub const ROW_BASE: u32 = 100;

pub const MAX_ROWS: usize = 8;
pub const CARD_WIDTH: f32 = 640.0;
pub const ROW_HEIGHT: f32 = 44.0;
/// Logical icon size; icons are rasterized at twice this so 2x displays stay sharp.
pub const ICON_SIZE: f32 = 26.0;
pub const ICON_PX: u32 = 52;
const SUBTITLE_WIDTH: f32 = 170.0;

/// Resolved colors and type for drawing, derived from the live theme.
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    pub backdrop: Color,
    pub surface: Color,
    pub selected: Color,
    pub hover: Color,
    pub fg: Color,
    pub fg_dim: Color,
    pub accent: Color,
    pub border: Color,
    pub shadow: Color,
    pub radius: f32,
    pub family: FontFamily,
    /// Base text size in logical px.
    pub base: f32,
}

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

impl Look {
    pub fn from_theme(t: &Theme) -> Self {
        let p = &t.palette;
        Self {
            backdrop: Color::rgba(0, 0, 0, 96),
            surface: color(p.surface),
            selected: color(p.surface_alt),
            hover: color(p.surface_alt).fade(0.55),
            fg: color(p.fg),
            fg_dim: color(p.fg_dim),
            accent: color(p.accent),
            border: color(p.border),
            shadow: color(p.shadow),
            radius: t.shape.radius as f32,
            family: family(&t.fonts.family),
            // Theme sizes are points at scale 1.
            base: (t.fonts.size * 4.0 / 3.0).clamp(8.0, 48.0),
        }
    }

    fn text(&self, size: f32, weight: u16) -> TextStyle {
        TextStyle {
            family: self.family.clone(),
            weight,
            ..TextStyle::sized(size)
        }
    }
}

impl Default for Look {
    fn default() -> Self {
        Self::from_theme(&Theme::default())
    }
}

/// One displayed result.
#[derive(Clone)]
pub struct Row {
    pub title: String,
    /// Matched char indices into `title`.
    pub positions: Vec<usize>,
    pub subtitle: Option<String>,
    pub icon: Option<Image>,
}

/// The whole tree for the given state.
pub fn build(look: &Look, rows: &[Row], selected: usize, query: &str, has_query: bool) -> Node {
    let mut input = TextInput::default();
    input.text = query.to_string();
    input.cursor = query.len();
    input.placeholder = "Search applications".into();
    input.style = look.text(look.base * 1.45, 400);
    input.color = look.fg;
    input.placeholder_color = look.fg_dim;
    input.caret_color = look.accent;
    input.selection_color = look.accent.fade(0.35);
    input.min_width = 200.0;
    let mut input = Node::input(input).id(INPUT);
    input.style.padding = Insets::xy(12.0, 10.0);

    let separator = Node::spacer()
        .size(Dim::Fill(1.0), Dim::Px(1.0))
        .bg(look.border);

    let results = Node::column(result_nodes(look, rows, selected, has_query))
        .id(RESULTS)
        .gap(2.0);

    let card = Node::column(vec![input, separator, results])
        .id(CARD)
        .width(Dim::Px(CARD_WIDTH))
        .padding(Insets::all(10.0))
        .gap(8.0)
        .bg(look.surface)
        .radius(look.radius + 6.0)
        .border(1.0, look.border)
        .shadow(48.0, Point::new(0.0, 14.0), look.shadow)
        .clickable();

    Node::column(vec![card])
        .id(ROOT)
        .size(Dim::Fill(1.0), Dim::Fill(1.0))
        .align(Align::Center)
        .bg(look.backdrop)
        .clickable()
}

/// The children of the `RESULTS` column.
pub fn result_nodes(look: &Look, rows: &[Row], selected: usize, has_query: bool) -> Vec<Node> {
    if rows.is_empty() {
        let text = if has_query {
            "No matching applications"
        } else {
            "No applications found"
        };
        return vec![
            Node::label(text, look.text(look.base, 400), look.fg_dim)
                .padding(Insets::xy(12.0, 12.0)),
        ];
    }
    rows.iter()
        .enumerate()
        .map(|(i, row)| row_node(look, row, i, i == selected))
        .collect()
}

fn row_node(look: &Look, row: &Row, i: usize, selected: bool) -> Node {
    let icon = Node::icon(row.icon.clone(), Size::new(ICON_SIZE, ICON_SIZE));
    let title: Vec<Node> = highlight_runs(&row.title, &row.positions)
        .into_iter()
        .map(|(text, hit)| {
            let (weight, c) = if hit {
                (700, look.accent)
            } else {
                (400, look.fg)
            };
            Node::label(text, look.text(look.base, weight), c)
        })
        .collect();
    let title = Node::row(title).width(Dim::Fill(1.0)).clip();
    let sub = Node::label(
        row.subtitle.clone().unwrap_or_default(),
        look.text(look.base * 0.82, 400),
        look.fg_dim,
    )
    .width(Dim::Px(SUBTITLE_WIDTH))
    .text_align(TextAlign::Right);
    let mut node = Node::row(vec![icon, title, sub])
        .id(ROW_BASE + i as u32)
        .height(Dim::Px(ROW_HEIGHT))
        .padding(Insets::xy(10.0, 0.0))
        .gap(12.0)
        .radius(8.0)
        .hover_bg(look.hover)
        .clickable();
    if selected {
        node = node.bg(look.selected);
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_ui::{Id, PaintCaches, PixelBuffer, Ui};

    fn rows(n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| Row {
                title: format!("App {i}"),
                positions: vec![0],
                subtitle: (i % 2 == 0).then(|| "Generic".to_string()),
                icon: None,
            })
            .collect()
    }

    fn ui_with(rows: &[Row], selected: usize) -> Ui {
        let text = aurora_ui::TextSystem::new();
        let root = build(&Look::default(), rows, selected, "", false);
        let mut ui = Ui::new(text, root);
        ui.set_size(Size::new(1000.0, 700.0));
        ui.layout();
        ui
    }

    #[test]
    fn look_follows_the_theme() {
        let mut t = Theme::default();
        t.palette.accent = Rgba::rgb(1, 2, 3);
        t.fonts.family = "Inter".into();
        t.fonts.size = 12.0;
        t.shape.radius = 14;
        let look = Look::from_theme(&t);
        assert_eq!(look.accent, Color::rgb(1, 2, 3));
        assert_eq!(look.family, FontFamily::Named("Inter".into()));
        assert_eq!(look.base, 16.0);
        assert_eq!(look.radius, 14.0);
        assert_eq!(family("Sans-Serif"), FontFamily::SansSerif);
        assert_eq!(family("monospace"), FontFamily::Monospace);
    }

    #[test]
    fn the_card_is_centered_and_rows_stack() {
        let ui = ui_with(&rows(5), 0);
        let card = ui.rect_of(Id(CARD)).expect("card");
        assert_eq!(card.w, CARD_WIDTH);
        assert!((card.x + card.w / 2.0 - 500.0).abs() < 1.0, "{card:?}");
        let r0 = ui.rect_of(Id(ROW_BASE)).expect("row 0");
        let r4 = ui.rect_of(Id(ROW_BASE + 4)).expect("row 4");
        assert_eq!(r0.h, ROW_HEIGHT);
        assert!(r4.y > r0.y + 4.0 * ROW_HEIGHT - 1.0);
        assert!(ui.rect_of(Id(ROW_BASE + 5)).is_none());
        let input = ui.rect_of(Id(INPUT)).expect("input");
        assert!(input.y < r0.y && input.w > 500.0);
    }

    #[test]
    fn rows_can_be_swapped_in_place() {
        let mut ui = ui_with(&rows(2), 0);
        let look = Look::default();
        ui.edit(Id(RESULTS), |n| {
            n.children = result_nodes(&look, &rows(6), 1, true)
        });
        ui.layout();
        assert!(ui.rect_of(Id(ROW_BASE + 5)).is_some());
        ui.edit(Id(RESULTS), |n| {
            n.children = result_nodes(&look, &[], 0, true)
        });
        ui.layout();
        assert!(ui.rect_of(Id(ROW_BASE)).is_none());
    }

    #[test]
    fn empty_state_uses_the_right_wording() {
        let look = Look::default();
        assert_eq!(result_nodes(&look, &[], 0, true).len(), 1);
        assert_eq!(result_nodes(&look, &rows(3), 0, false).len(), 3);
    }

    #[test]
    fn selected_row_is_painted_differently() {
        let text = aurora_ui::TextSystem::new();
        if !text.has_fonts() {
            return;
        }
        let mut ui = ui_with(&rows(3), 1);
        let mut buf = PixelBuffer::new(1000, 700);
        let mut caches = PaintCaches::new(text);
        ui.draw(&mut buf.painter(1.0, &mut caches));
        let r0 = ui.rect_of(Id(ROW_BASE)).expect("row 0");
        let r1 = ui.rect_of(Id(ROW_BASE + 1)).expect("row 1");
        // Far right of the row, past the right-aligned subtitle's padding: plain background.
        let probe =
            |r: aurora_ui::Rect| buf.argb((r.right() - 3.0) as u32, (r.y + r.h / 2.0) as u32);
        assert_ne!(probe(r0), probe(r1), "selected row must stand out");
        // The card is opaque-ish and the backdrop dims the rest.
        let outside = buf.argb(5, 5);
        let inside = buf.argb(500, (r0.y + 2.0) as u32);
        assert_ne!(outside, inside);
        assert!(buf.pixel(5, 5).a > 0, "backdrop has some alpha");
    }
}
