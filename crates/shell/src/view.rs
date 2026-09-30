//! Bar widget tree from the view-model and the theme. Pure: no Wayland.

use aurora_theme::{Rgba, Theme};
use aurora_ui::{Align, Color, Dim, FontFamily, Id, Insets, Node, TextStyle};

use crate::model::OutputView;

/// Workspace pill `n` has widget id `WS_BASE + n`.
pub const WS_BASE: u32 = 1000;

pub fn ws_id(index: u32) -> Id {
    Id(WS_BASE + index)
}

/// The workspace a clicked widget id stands for.
pub fn ws_from_id(id: Id) -> Option<u32> {
    id.0.checked_sub(WS_BASE).filter(|i| *i > 0)
}

pub fn color(c: Rgba) -> Color {
    let [r, g, b, a] = c.0;
    Color::rgba(r, g, b, a)
}

pub fn family(name: &str) -> FontFamily {
    match name.trim().to_ascii_lowercase().as_str() {
        "" | "sans-serif" | "sans" => FontFamily::SansSerif,
        "serif" => FontFamily::Serif,
        "monospace" | "mono" => FontFamily::Monospace,
        _ => FontFamily::Named(name.trim().to_string()),
    }
}

/// Theme font sizes are points at scale 1; the bar converts to logical px (96 dpi).
pub fn text_style(theme: &Theme) -> TextStyle {
    TextStyle {
        family: family(&theme.fonts.family),
        ..TextStyle::sized(theme.fonts.size * 4.0 / 3.0)
    }
}

/// The bar's logical height.
pub fn bar_height(theme: &Theme) -> u32 {
    theme.shape.bar_height.clamp(16, 256)
}

pub fn build(view: &OutputView, clock: &str, theme: &Theme) -> Node {
    let p = &theme.palette;
    let style = text_style(theme);
    let gap = theme.shape.gap.clamp(2, 32) as f32;
    let pill_h = (bar_height(theme) as f32 - 8.0).max(12.0);
    let radius = (theme.shape.radius as f32).min(pill_h / 2.0);

    let pills = view
        .workspaces
        .iter()
        .map(|w| {
            let (bg, fg, hover) = if w.active {
                (Some(color(p.accent)), color(p.accent_fg), color(p.accent))
            } else if w.urgent {
                (Some(color(p.urgent)), color(p.accent_fg), color(p.urgent))
            } else {
                (None, color(p.fg), color(p.surface_alt))
            };
            let mut pill = Node::row(vec![Node::label(w.index.to_string(), style.clone(), fg)])
                .id(ws_id(w.index).0)
                .padding(Insets::xy(10.0, 0.0))
                .height(Dim::Px(pill_h))
                .radius(radius)
                .hover_bg(hover)
                .clickable();
            if let Some(bg) = bg {
                pill = pill.bg(bg);
            }
            pill
        })
        .collect();
    let left = Node::row(pills).gap(4.0);

    let mut mid = Vec::new();
    if !view.app_id.is_empty() {
        mid.push(Node::label(
            view.app_id.clone(),
            style.clone(),
            color(p.fg_dim),
        ));
    }
    mid.push(Node::label(view.title.clone(), style.clone(), color(p.fg)).width(Dim::Fill(1.0)));
    let mid = Node::row(mid).gap(8.0).width(Dim::Fill(1.0));

    let right = Node::label(clock, style, color(p.fg));

    Node::row(vec![left, mid, right])
        .align(Align::Center)
        .gap(gap * 2.0)
        .padding(Insets::xy(gap, 0.0))
        .height(Dim::Fill(1.0))
        .bg(color(p.bg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WsView;
    use aurora_ui::{
        Button, Input, PaintCaches, PixelBuffer, Point, Size, TextSystem, Ui, UiEvent,
    };

    fn sample() -> OutputView {
        OutputView {
            workspaces: vec![
                WsView {
                    index: 1,
                    active: true,
                    urgent: false,
                    windows: 1,
                },
                WsView {
                    index: 3,
                    active: false,
                    urgent: true,
                    windows: 2,
                },
            ],
            title: "a fairly long window title that will not fit".into(),
            app_id: "kitty".into(),
        }
    }

    fn ui_with(view: &OutputView, theme: &Theme, w: f32, scale: f32) -> Option<Ui> {
        let text = TextSystem::new();
        if !text.has_fonts() {
            return None;
        }
        let mut ui = Ui::new(text, build(view, "Tue 30 Sep  14:05", theme));
        ui.set_scale(scale);
        ui.set_size(Size::new(w, bar_height(theme) as f32));
        ui.layout();
        Some(ui)
    }

    fn close(a: Color, b: Color) -> bool {
        let d = |x: u8, y: u8| x.abs_diff(y) <= 3;
        d(a.r, b.r) && d(a.g, b.g) && d(a.b, b.b) && d(a.a, b.a)
    }

    #[test]
    fn ids_roundtrip() {
        assert_eq!(ws_from_id(ws_id(7)), Some(7));
        assert_eq!(ws_from_id(Id(3)), None);
        assert_eq!(ws_from_id(Id(WS_BASE)), None);
        assert_eq!(family("Inter"), FontFamily::Named("Inter".into()));
        assert_eq!(family("Monospace"), FontFamily::Monospace);
    }

    #[test]
    fn renders_translucent_bar_with_active_pill() {
        let theme = Theme::default();
        let Some(ui) = ui_with(&sample(), &theme, 600.0, 1.0) else {
            return;
        };
        let mut buf = PixelBuffer::new(600, bar_height(&theme));
        let mut caches = PaintCaches::new(ui.text().clone());
        ui.paint(
            &mut buf.painter(1.0, &mut caches),
            &[aurora_ui::Rect::new(
                0.0,
                0.0,
                600.0,
                bar_height(&theme) as f32,
            )],
        );
        // Translucent backdrop between the widgets, the compositor blurs behind it.
        assert!(close(buf.pixel(300, 1), color(theme.palette.bg)));
        assert!(buf.argb(300, 1)[0] < 255);
        // Active pill: accent, sampled in its left padding where there is no glyph.
        let r = ui.rect_of(ws_id(1)).expect("pill 1");
        let c = buf.pixel((r.x + 3.0) as u32, (r.y + r.h / 2.0) as u32);
        assert!(close(c, color(theme.palette.accent)), "{c:?}");
        let u = ui.rect_of(ws_id(3)).expect("pill 3");
        let c = buf.pixel((u.x + 3.0) as u32, (u.y + u.h / 2.0) as u32);
        assert!(close(c, color(theme.palette.urgent)), "{c:?}");
        // Text pixels exist somewhere on the right (the clock).
        let any_text = (450..590).any(|x| (8..26).any(|y| buf.pixel(x, y).r > 150));
        assert!(any_text);
    }

    #[test]
    fn retheme_changes_pixels_and_scale_two_fills_the_buffer() {
        let mut theme = Theme::default();
        theme.palette.bg = Rgba::new(255, 0, 0, 128);
        theme.shape.bar_height = 40;
        let Some(mut ui) = ui_with(&sample(), &theme, 300.0, 2.0) else {
            return;
        };
        let mut buf = PixelBuffer::new(600, 80);
        let mut caches = PaintCaches::new(ui.text().clone());
        ui.draw(&mut buf.painter(2.0, &mut caches));
        assert!(close(buf.pixel(300, 2), Color::rgba(255, 0, 0, 128)));
        assert!(close(buf.pixel(599, 79), Color::rgba(255, 0, 0, 128)));
    }

    #[test]
    fn clicking_a_pill_reports_its_workspace() {
        let theme = Theme::default();
        let Some(mut ui) = ui_with(&sample(), &theme, 600.0, 1.0) else {
            return;
        };
        let r = ui.rect_of(ws_id(3)).expect("pill 3");
        let p = Point::new(r.x + r.w / 2.0, r.y + r.h / 2.0);
        ui.handle(Input::PointerMove(p));
        ui.handle(Input::PointerDown(p, Button::Left));
        let ev = ui.handle(Input::PointerUp(p, Button::Left));
        assert_eq!(ev, vec![UiEvent::Clicked(ws_id(3))]);
        assert_eq!(ws_from_id(ws_id(3)), Some(3));
    }

    #[test]
    fn long_title_is_clipped_inside_the_bar() {
        let theme = Theme::default();
        let Some(mut ui) = ui_with(&sample(), &theme, 400.0, 1.0) else {
            return;
        };
        let mut buf = PixelBuffer::new(400, bar_height(&theme));
        let mut caches = PaintCaches::new(ui.text().clone());
        ui.draw(&mut buf.painter(1.0, &mut caches));
        // The clock still has its room at the right edge: its last glyphs are drawn.
        assert!((360..396).any(|x| (8..26).any(|y| buf.pixel(x, y).r > 150)));
    }
}
