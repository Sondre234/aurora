//! What one toast looks like: per-urgency styling from the theme and the widget tree.
//!
//! Widget ids: [`ROOT`] is the whole toast (click = default action or dismiss),
//! [`CLOSE`] the close button, `ACTION_BASE + i` the i-th action button.

use aurora_theme::{Rgba, Theme};
use aurora_ui::{Align, Color, Dim, FontFamily, Image, Insets, Node, Size, TextStyle};

use crate::hints::Urgency;
use crate::state::Notification;

pub const ROOT: u32 = 1;
pub const CLOSE: u32 = 2;
pub const ACTION_BASE: u32 = 10;
/// Action buttons shown per toast; the rest stay invocable by other clients only.
pub const MAX_ACTION_BUTTONS: usize = 3;

pub const ICON_SIZE: f32 = 40.0;
const PAD: f32 = 12.0;
const BAR_WIDTH: f32 = 4.0;

pub fn color(c: Rgba) -> Color {
    let [r, g, b, a] = c.0;
    Color::rgba(r, g, b, a)
}

/// Colors and metrics of a toast, derived from the theme and the urgency.
#[derive(Clone, Debug, PartialEq)]
pub struct ToastStyle {
    pub bg: Color,
    pub bg_hover: Color,
    pub fg: Color,
    pub fg_dim: Color,
    /// Left bar and border.
    pub accent: Color,
    pub border: Color,
    pub button_bg: Color,
    pub button_hover: Color,
    pub button_press: Color,
    pub radius: f32,
    pub border_width: f32,
    pub title_px: f32,
    pub body_px: f32,
    pub small_px: f32,
    pub family: FontFamily,
}

impl ToastStyle {
    pub fn new(theme: &Theme, urgency: Urgency) -> Self {
        let p = &theme.palette;
        let fg = color(p.fg);
        let surface = color(p.surface);
        let (accent, border, fg_dim) = match urgency {
            Urgency::Low => (color(p.fg_dim), color(p.border), color(p.fg_dim)),
            Urgency::Normal => (color(p.accent), color(p.border), color(p.fg_dim)),
            // Critical stays loud: urgent accent and border, brighter secondary text.
            Urgency::Critical => (color(p.urgent), color(p.urgent), color(p.fg).fade(0.8)),
        };
        let alt = color(p.surface_alt);
        let px = theme.fonts.size.clamp(6.0, 48.0) * 4.0 / 3.0;
        let family = match theme.fonts.family.as_str() {
            "" | "sans-serif" => FontFamily::SansSerif,
            "serif" => FontFamily::Serif,
            "monospace" => FontFamily::Monospace,
            name => FontFamily::Named(name.to_string()),
        };
        Self {
            bg: surface,
            bg_hover: surface.mix(fg, 0.06),
            fg,
            fg_dim,
            accent,
            border,
            button_bg: alt,
            button_hover: alt.mix(fg, 0.1),
            button_press: alt.mix(accent, 0.3),
            radius: theme.shape.radius.min(32) as f32,
            border_width: theme.shape.border_width.min(6) as f32,
            title_px: px,
            body_px: px * 0.93,
            small_px: px * 0.8,
            family,
        }
    }

    fn text(&self, px: f32) -> TextStyle {
        TextStyle {
            family: self.family.clone(),
            ..TextStyle::sized(px)
        }
    }
}

/// The widget tree of one toast, `width` logical px wide (auto height).
pub fn build(n: &Notification, st: &ToastStyle, image: Option<Image>, width: f32) -> Node {
    let mut header = vec![
        Node::label(
            if n.app_name.is_empty() {
                "Notification"
            } else {
                &n.app_name
            },
            st.text(st.small_px),
            st.fg_dim,
        )
        .width(Dim::Fill(1.0)),
    ];
    header.push(
        Node::button("\u{00d7}", st.text(st.title_px), st.fg_dim)
            .id(CLOSE)
            .padding(Insets::xy(6.0, 0.0))
            .hover_bg(st.button_hover)
            .press_bg(st.button_press),
    );
    let mut column = vec![Node::row(header).gap(4.0)];
    if !n.summary.is_empty() {
        column.push(
            Node::label(
                &n.summary,
                TextStyle {
                    weight: 700,
                    ..st.text(st.title_px).wrapped(2)
                },
                st.fg,
            )
            .width(Dim::Fill(1.0)),
        );
    }
    if !n.body.is_empty() {
        column.push(
            Node::label(
                &n.body,
                st.text(st.body_px).wrapped(5),
                st.fg_dim.mix(st.fg, 0.5),
            )
            .width(Dim::Fill(1.0)),
        );
    }
    let buttons: Vec<Node> = n
        .actions
        .iter()
        .take(MAX_ACTION_BUTTONS)
        .enumerate()
        .map(|(i, a)| {
            Node::button(&a.label, st.text(st.small_px), st.fg)
                .id(ACTION_BASE + i as u32)
                .bg(st.button_bg)
                .hover_bg(st.button_hover)
                .press_bg(st.button_press)
        })
        .collect();
    if !buttons.is_empty() {
        column.push(Node::row(buttons).gap(6.0));
    }
    let column = Node::column(column)
        .gap(4.0)
        .width(Dim::Fill(1.0))
        .padding(Insets::all(PAD - 2.0));

    let mut content = Vec::new();
    if let Some(img) = image {
        content.push(
            Node::row(vec![Node::icon(Some(img), Size::new(ICON_SIZE, ICON_SIZE))])
                .padding(Insets {
                    left: PAD,
                    top: PAD,
                    right: 0.0,
                    bottom: PAD,
                })
                .align(Align::Start),
        );
    }
    content.push(column);

    let bar = Node::spacer().width(Dim::Px(BAR_WIDTH)).bg(st.accent);
    let mut root = Node::row(vec![
        bar,
        Node::row(content).width(Dim::Fill(1.0)).align(Align::Start),
    ])
    .id(ROOT)
    .width(Dim::Px(width))
    .align(Align::Stretch)
    .bg(st.bg)
    .hover_bg(st.bg_hover)
    .radius(st.radius)
    .clip()
    .clickable();
    if st.border_width > 0.0 {
        root = root.border(st.border_width, st.border);
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hints::Hints;
    use crate::state::{Center, Config, NotifyArgs};
    use aurora_ui::{Id, TextSystem, Ui};

    fn notification(urgency: Urgency, actions: &[&str]) -> Notification {
        let mut c = Center::new(Config::default());
        c.notify(
            0,
            NotifyArgs {
                id: 1,
                app_name: "app".into(),
                app_icon: String::new(),
                summary: "Summary".into(),
                body: "Body".into(),
                actions: actions.iter().map(|s| s.to_string()).collect(),
                hints: Hints {
                    urgency,
                    ..Hints::default()
                },
                expire_timeout: 0,
            },
        );
        c.get(1).unwrap().clone()
    }

    #[test]
    fn urgency_changes_the_accent() {
        let theme = Theme::default();
        let low = ToastStyle::new(&theme, Urgency::Low);
        let normal = ToastStyle::new(&theme, Urgency::Normal);
        let crit = ToastStyle::new(&theme, Urgency::Critical);
        assert_eq!(normal.accent, color(theme.palette.accent));
        assert_eq!(crit.accent, color(theme.palette.urgent));
        assert_eq!(crit.border, color(theme.palette.urgent));
        assert_ne!(low.accent, normal.accent);
        assert_eq!(low.bg, normal.bg);
    }

    #[test]
    fn theme_fonts_and_shape_apply() {
        let mut theme = Theme::default();
        theme.fonts.family = "Inter".into();
        theme.fonts.size = 12.0;
        theme.shape.radius = 500;
        let st = ToastStyle::new(&theme, Urgency::Normal);
        assert_eq!(st.family, FontFamily::Named("Inter".into()));
        assert_eq!(st.title_px, 16.0);
        assert_eq!(st.radius, 32.0);
    }

    #[test]
    fn tree_has_the_interactive_ids() {
        let st = ToastStyle::new(&Theme::default(), Urgency::Normal);
        let n = notification(
            Urgency::Normal,
            &["default", "Open", "a", "A", "b", "B", "c", "C", "d", "D"],
        );
        let ui = Ui::new(TextSystem::new(), build(&n, &st, None, 360.0));
        for id in [ROOT, CLOSE, ACTION_BASE, ACTION_BASE + 1, ACTION_BASE + 2] {
            assert!(ui.node(Id(id)).is_some(), "missing widget {id}");
        }
        assert!(ui.node(Id(ACTION_BASE + 3)).is_none(), "button cap");
    }

    #[test]
    fn plain_toast_has_no_action_row() {
        let st = ToastStyle::new(&Theme::default(), Urgency::Low);
        let n = notification(Urgency::Low, &[]);
        let ui = Ui::new(TextSystem::new(), build(&n, &st, None, 360.0));
        assert!(ui.node(Id(ACTION_BASE)).is_none());
    }
}
