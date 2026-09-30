//! Pure conversions between Wayland units and toolkit units. Tested without a display.

use smithay_client_toolkit::seat::keyboard::{KeyEvent as SctkKey, Keysym, Modifiers};

use crate::damage::Damage;
use crate::geom::Rect;
use crate::input::{Key, KeyEvent, Mods};

/// Buffer size in device pixels for a logical size and a scale in 120ths
/// (the wp-fractional-scale unit), rounding half away from zero as the protocol says.
pub(crate) fn device_size(logical: (u32, u32), scale120: u32) -> (u32, u32) {
    let f = |v: u32| ((v as u64 * scale120 as u64 + 60) / 120).max(1) as u32;
    (f(logical.0), f(logical.1))
}

/// Logical damage to whole device pixels, one pixel of slack, clamped to the buffer.
pub(crate) fn device_rect(r: Rect, scale: f32, bounds: (u32, u32)) -> Rect {
    let x0 = ((r.x * scale).floor() - 1.0).max(0.0);
    let y0 = ((r.y * scale).floor() - 1.0).max(0.0);
    let x1 = ((r.right() * scale).ceil() + 1.0).min(bounds.0 as f32);
    let y1 = ((r.bottom() * scale).ceil() + 1.0).min(bounds.1 as f32);
    Rect::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
}

/// Device-pixel rect back to logical units for painting.
pub(crate) fn logical_rect(r: Rect, scale: f32) -> Rect {
    Rect::new(r.x / scale, r.y / scale, r.w / scale, r.h / scale)
}

/// Per-buffer damage bookkeeping for N-buffering.
///
/// A buffer that was not the one painted last is missing every change made since it was
/// last painted. Each slot therefore carries the damage it still owes; painting a slot
/// repaints `frame damage + owed damage`, clears its debt and adds the frame damage to
/// every other slot.
pub(crate) fn finish_frame(owed: &mut [Damage], used: usize, frame: &[Rect]) {
    for (i, d) in owed.iter_mut().enumerate() {
        if i == used {
            d.take();
        } else {
            frame.iter().for_each(|r| d.add(*r));
        }
    }
}

/// Everything slot `i` must repaint this frame.
pub(crate) fn repaint_region(owed: &[Damage], i: usize, frame: &[Rect]) -> Damage {
    let mut d = Damage::new();
    frame.iter().chain(owed[i].rects()).for_each(|r| d.add(*r));
    d
}

pub(crate) fn map_mods(m: Modifiers) -> Mods {
    Mods {
        ctrl: m.ctrl,
        alt: m.alt,
        shift: m.shift,
        logo: m.logo,
    }
}

pub(crate) fn map_key(ev: &SctkKey, mods: Mods) -> KeyEvent {
    let mut mods = mods;
    let key = match ev.keysym {
        Keysym::Return | Keysym::KP_Enter => Key::Enter,
        Keysym::Escape => Key::Escape,
        Keysym::BackSpace => Key::Backspace,
        Keysym::Delete | Keysym::KP_Delete => Key::Delete,
        Keysym::Tab => Key::Tab,
        Keysym::ISO_Left_Tab => {
            mods.shift = true;
            Key::Tab
        }
        Keysym::Left | Keysym::KP_Left => Key::Left,
        Keysym::Right | Keysym::KP_Right => Key::Right,
        Keysym::Up | Keysym::KP_Up => Key::Up,
        Keysym::Down | Keysym::KP_Down => Key::Down,
        Keysym::Home | Keysym::KP_Home => Key::Home,
        Keysym::End | Keysym::KP_End => Key::End,
        Keysym::Page_Up | Keysym::KP_Page_Up => Key::PageUp,
        Keysym::Page_Down | Keysym::KP_Page_Down => Key::PageDown,
        other => match other.key_char() {
            Some(c) => Key::Char(c),
            None => Key::Other(other.raw()),
        },
    };
    KeyEvent {
        key,
        text: ev.utf8.clone(),
        mods,
    }
}

/// Linux evdev button code to toolkit button.
pub(crate) fn map_button(code: u32) -> crate::input::Button {
    match code {
        0x110 => crate::input::Button::Left,
        0x111 => crate::input::Button::Right,
        0x112 => crate::input::Button::Middle,
        other => crate::input::Button::Other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sk(keysym: Keysym, utf8: Option<&str>) -> SctkKey {
        SctkKey {
            time: 0,
            raw_code: 0,
            keysym,
            utf8: utf8.map(str::to_string),
        }
    }

    #[test]
    fn device_size_rounds_like_the_protocol() {
        assert_eq!(device_size((100, 50), 120), (100, 50));
        assert_eq!(device_size((100, 50), 150), (125, 63)); // 62.5 rounds up
        assert_eq!(device_size((1, 1), 30), (1, 1)); // never zero
        assert_eq!(device_size((1920, 1080), 180), (2880, 1620));
    }

    #[test]
    fn device_rect_has_slack_and_clamps() {
        let r = device_rect(Rect::new(10.0, 10.0, 20.0, 5.0), 1.5, (100, 100));
        assert_eq!(r, Rect::new(14.0, 14.0, 32.0, 10.0));
        let r = device_rect(Rect::new(-5.0, 0.0, 500.0, 500.0), 1.0, (100, 80));
        assert_eq!(r, Rect::new(0.0, 0.0, 100.0, 80.0));
        assert_eq!(
            logical_rect(Rect::new(15.0, 30.0, 30.0, 30.0), 1.5),
            Rect::new(10.0, 20.0, 20.0, 20.0)
        );
    }

    #[test]
    fn triple_buffer_damage_catches_up() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(50.0, 0.0, 10.0, 10.0);
        let c = Rect::new(0.0, 50.0, 10.0, 10.0);
        let mut owed = vec![Damage::new(), Damage::new(), Damage::new()];
        // Frame 1 paints slot 0 with damage a.
        assert_eq!(repaint_region(&owed, 0, &[a]).rects(), &[a]);
        finish_frame(&mut owed, 0, &[a]);
        assert!(owed[0].is_empty() && owed[1].rects() == [a] && owed[2].rects() == [a]);
        // Frame 2 paints slot 1 with damage b: it must also repaint a.
        let region = repaint_region(&owed, 1, &[b]);
        assert_eq!(region.rects().len(), 2);
        finish_frame(&mut owed, 1, &[b]);
        // Slot 0 now owes b, slot 2 owes a and b, slot 1 owes nothing.
        assert_eq!(owed[0].rects(), &[b]);
        assert_eq!(owed[2].rects().len(), 2);
        assert!(owed[1].is_empty());
        // Frame 3 on slot 0 repaints c and b.
        let region = repaint_region(&owed, 0, &[c]);
        assert_eq!(region.rects().len(), 2);
    }

    #[test]
    fn key_mapping() {
        let m = Mods::default();
        assert_eq!(map_key(&sk(Keysym::Return, Some("\r")), m).key, Key::Enter);
        assert_eq!(map_key(&sk(Keysym::Up, None), m).key, Key::Up);
        assert_eq!(map_key(&sk(Keysym::Page_Down, None), m).key, Key::PageDown);
        let shift_tab = map_key(&sk(Keysym::ISO_Left_Tab, None), m);
        assert!(shift_tab.key == Key::Tab && shift_tab.mods.shift);
        let a = map_key(&sk(Keysym::a, Some("a")), m);
        assert_eq!((a.key, a.text.as_deref()), (Key::Char('a'), Some("a")));
        assert_eq!(
            map_key(&sk(Keysym::F5, None), m).key,
            Key::Other(Keysym::F5.raw())
        );
        assert_eq!(map_button(0x110), crate::input::Button::Left);
        assert_eq!(map_button(0x113), crate::input::Button::Other(0x113));
    }
}
