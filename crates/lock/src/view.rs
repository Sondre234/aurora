//! The lock screen widget tree and how prompt state maps onto it. One [`Ui`] per output,
//! all showing the same content.
//!
//! The password is never handed to the toolkit: the tree only ever holds a row of dots.
//! The surface is opaque (the compositor draws black behind lock surfaces, so there is
//! nothing to blend with).

use aurora_theme::{Rgba, Theme};
use aurora_ui::{
    Align, Color, FontFamily, Id, Insets, Justify, Node, TextAlign, TextStyle, TextSystem, Ui,
    widget::{Dim, Kind},
};

use crate::prompt::Status;

pub const CLOCK: u32 = 1;
pub const DATE: u32 = 2;
pub const DOTS: u32 = 3;
pub const STATUS: u32 = 4;
pub const USER: u32 = 5;

/// Everything the screen shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub clock: String,
    pub date: String,
    pub dots: usize,
    pub status: Status,
    pub caps_lock: bool,
}

pub fn color(c: Rgba) -> Color {
    let [r, g, b, a] = c.0;
    Color::rgba(r, g, b, a)
}

fn opaque(c: Rgba) -> Color {
    let [r, g, b, _] = c.0;
    Color::rgb(r, g, b)
}

/// The text shown in the status line.
pub fn status_text(status: Status, caps_lock: bool) -> String {
    match status {
        Status::Ready if caps_lock => "Caps Lock is on".into(),
        Status::Ready => String::new(),
        Status::Checking => "Checking...".into(),
        Status::Wrong if caps_lock => "Wrong password (Caps Lock is on)".into(),
        Status::Wrong => "Wrong password".into(),
        Status::Failed => "Authentication failed".into(),
        Status::LockedOut(s) => format!("Too many attempts, wait {s}s"),
        Status::Waiting => "Locking...".into(),
        Status::Unlocking => "Unlocking...".into(),
    }
}

/// `count` bullets, or the hint when nothing is typed.
pub fn dots_text(count: usize) -> String {
    if count == 0 {
        return "Password".into();
    }
    let mut s = String::with_capacity(count * 4);
    for i in 0..count {
        if i > 0 {
            s.push(' ');
        }
        s.push('\u{2022}');
    }
    s
}

fn status_color(theme: &Theme, status: Status, caps_lock: bool) -> Color {
    let p = &theme.palette;
    match status {
        Status::Wrong | Status::Failed | Status::LockedOut(_) => opaque(p.urgent),
        Status::Ready if caps_lock => opaque(p.urgent),
        _ => opaque(p.fg_dim),
    }
}

fn family(theme: &Theme) -> FontFamily {
    match theme.fonts.family.as_str() {
        "" | "sans-serif" => FontFamily::SansSerif,
        "serif" => FontFamily::Serif,
        "monospace" => FontFamily::Monospace,
        name => FontFamily::Named(name.to_string()),
    }
}

fn style(theme: &Theme, size: f32, weight: u16) -> TextStyle {
    TextStyle {
        family: family(theme),
        weight,
        ..TextStyle::sized(size)
    }
}

fn centered(mut node: Node) -> Node {
    node = node.text_align(TextAlign::Center);
    node
}

/// Builds the screen for `user`.
pub fn build(text: TextSystem, theme: &Theme, user: &str, model: &Model) -> Ui {
    let p = &theme.palette;
    let r = theme.shape.radius as f32;
    let panel = Node::row(vec![
        centered(Node::label(
            dots_text(model.dots),
            style(theme, 28.0, 400),
            opaque(p.fg),
        ))
        .id(DOTS)
        .width(Dim::Fill(1.0)),
    ])
    .width(Dim::Px(360.0))
    .height(Dim::Px(64.0))
    .padding(Insets::xy(16.0, 8.0))
    .justify(Justify::Center)
    .bg(opaque(p.surface))
    .radius(r)
    .border(theme.shape.border_width as f32, opaque(p.border_active));
    let root = Node::column(vec![
        Node::label(model.clock.clone(), style(theme, 112.0, 700), opaque(p.fg)).id(CLOCK),
        Node::label(
            model.date.clone(),
            style(theme, 24.0, 400),
            opaque(p.fg_dim),
        )
        .id(DATE),
        Node::spacer().height(Dim::Px(56.0)),
        Node::label(user.to_string(), style(theme, 18.0, 400), opaque(p.fg_dim)).id(USER),
        Node::spacer().height(Dim::Px(10.0)),
        panel,
        Node::spacer().height(Dim::Px(14.0)),
        Node::label(
            status_text(model.status, model.caps_lock),
            style(theme, 18.0, 400),
            status_color(theme, model.status, model.caps_lock),
        )
        .id(STATUS),
    ])
    .size(Dim::Fill(1.0), Dim::Fill(1.0))
    .align(Align::Center)
    .justify(Justify::Center)
    .bg(opaque(p.bg));
    Ui::new(text, root)
}

/// Pushes `model` into an existing screen. Only widgets whose content changed repaint.
pub fn apply(ui: &mut Ui, theme: &Theme, model: &Model) {
    let set = |ui: &mut Ui, id: u32, text: &str| {
        if ui.text_of(Id(id)) != Some(text) {
            ui.set_text(Id(id), text);
        }
    };
    set(ui, CLOCK, &model.clock);
    set(ui, DATE, &model.date);
    set(ui, DOTS, &dots_text(model.dots));
    set(ui, STATUS, &status_text(model.status, model.caps_lock));
    let c = status_color(theme, model.status, model.caps_lock);
    let current = ui.node(Id(STATUS)).and_then(|n| match &n.kind {
        Kind::Label(l) => Some(l.color),
        _ => None,
    });
    if current != Some(c) {
        ui.edit(Id(STATUS), |n| {
            if let Kind::Label(l) = &mut n.kind {
                l.color = c;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_ui::{PaintCaches, PixelBuffer, Size};

    fn model(dots: usize, status: Status) -> Model {
        Model {
            clock: "14:05".into(),
            date: "Wednesday, 30 September".into(),
            dots,
            status,
            caps_lock: false,
        }
    }

    #[test]
    fn dots_text_shapes() {
        assert_eq!(dots_text(0), "Password");
        assert_eq!(dots_text(1), "\u{2022}");
        assert_eq!(dots_text(3), "\u{2022} \u{2022} \u{2022}");
    }

    #[test]
    fn status_lines() {
        assert_eq!(status_text(Status::Ready, false), "");
        assert_eq!(status_text(Status::Ready, true), "Caps Lock is on");
        assert_eq!(status_text(Status::Wrong, false), "Wrong password");
        assert_eq!(
            status_text(Status::LockedOut(7), false),
            "Too many attempts, wait 7s"
        );
    }

    #[test]
    fn widget_tree_never_holds_the_password() {
        // The only text a prompt can put in the tree is dots; check the widgets agree.
        let theme = Theme::default();
        let mut ui = build(
            TextSystem::new(),
            &theme,
            "sondre",
            &model(4, Status::Ready),
        );
        ui.set_size(Size::new(800.0, 600.0));
        ui.layout();
        assert_eq!(
            ui.text_of(Id(DOTS)),
            Some("\u{2022} \u{2022} \u{2022} \u{2022}")
        );
        apply(&mut ui, &theme, &model(0, Status::Wrong));
        assert_eq!(ui.text_of(Id(DOTS)), Some("Password"));
        assert_eq!(ui.text_of(Id(STATUS)), Some("Wrong password"));
    }

    #[test]
    fn paints_an_opaque_surface_with_a_panel() {
        let text = TextSystem::new();
        if !text.has_fonts() {
            eprintln!("skipped: no fonts installed");
            return;
        }
        let theme = Theme::default();
        let (w, h) = (800u32, 600u32);
        let mut ui = build(text.clone(), &theme, "sondre", &model(3, Status::Ready));
        ui.set_size(Size::new(w as f32, h as f32));
        ui.layout();
        let mut caches = PaintCaches::new(text);
        let mut buf = PixelBuffer::new(w, h);
        ui.draw(&mut buf.painter(1.0, &mut caches));

        // Every corner is the opaque background.
        let [r, g, b, _] = theme.palette.bg.0;
        for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
            assert_eq!(buf.pixel(x, y), Color::rgb(r, g, b));
        }
        // The password panel paints the surface color at its corner-free edge.
        let panel = ui.rect_of(Id(DOTS)).expect("dots laid out").outset(0.0);
        let [sr, sg, sb, _] = theme.palette.surface.0;
        let px = buf.pixel((panel.x - 8.0) as u32, (panel.y + panel.h / 2.0) as u32);
        assert_eq!(px, Color::rgb(sr, sg, sb));
        // The dots themselves drew something lighter than the panel.
        let lit = (panel.x as u32..panel.right() as u32).any(|x| {
            let c = buf.pixel(x, (panel.y + panel.h / 2.0) as u32);
            c.r > sr.saturating_add(60)
        });
        assert!(lit, "dots are visible");
    }

    #[test]
    fn apply_repaints_only_what_changed() {
        let theme = Theme::default();
        let mut ui = build(
            TextSystem::new(),
            &theme,
            "sondre",
            &model(0, Status::Ready),
        );
        ui.set_size(Size::new(800.0, 600.0));
        ui.layout();
        let mut caches = PaintCaches::new(ui.text().clone());
        let mut buf = PixelBuffer::new(800, 600);
        ui.draw(&mut buf.painter(1.0, &mut caches));
        assert!(!ui.needs_redraw());
        apply(&mut ui, &theme, &model(0, Status::Ready));
        assert!(!ui.needs_redraw(), "same model, no repaint");
        apply(&mut ui, &theme, &model(1, Status::Ready));
        assert!(ui.needs_redraw());
    }
}
