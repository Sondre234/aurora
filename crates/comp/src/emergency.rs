//! Hardcoded chords that always work: quit and VT switch. They ignore shortcut inhibitors,
//! layer surfaces and the config, and the config cannot bind them (see `is_reserved`).

use smithay::input::keyboard::{Keycode, Keysym, keysyms};

use crate::config::keybind::{Chord, Mods, Trigger};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emergency {
    Quit,
    VtSwitch(i32),
}

/// evdev KEY_BACKSPACE (14) plus the xkb offset of 8.
const KEYCODE_BACKSPACE: u32 = 14 + 8;

/// AltGr counts as Alt: with the altgr-intl layout the right Alt is not Mod1.
fn ctrl_alt(mods: Mods) -> bool {
    mods.contains(Mods::CTRL) && (mods.contains(Mods::ALT) || mods.contains(Mods::ALTGR))
}

fn is_f_key(sym: u32) -> bool {
    (keysyms::KEY_F1..=keysyms::KEY_F12).contains(&sym)
}

/// Matched on raw syms and keycode so no layout or level can hide the chord.
pub fn classify(
    mods: Mods,
    keycode: Keycode,
    raw_syms: &[Keysym],
    modified_sym: Keysym,
) -> Option<Emergency> {
    let ctrl_alt = ctrl_alt(mods);

    if ctrl_alt
        && (keycode == Keycode::new(KEYCODE_BACKSPACE)
            || raw_syms.iter().any(|s| {
                matches!(
                    s.raw(),
                    keysyms::KEY_BackSpace | keysyms::KEY_Terminate_Server
                )
            }))
    {
        return Some(Emergency::Quit);
    }

    let modified = modified_sym.raw();
    if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&modified) {
        return Some(Emergency::VtSwitch(
            (modified - keysyms::KEY_XF86Switch_VT_1 + 1) as i32,
        ));
    }
    if ctrl_alt {
        let f = raw_syms.iter().map(|s| s.raw()).find(|s| is_f_key(*s));
        if let Some(f) = f {
            return Some(Emergency::VtSwitch((f - keysyms::KEY_F1 + 1) as i32));
        }
    }
    None
}

/// Ctrl+F-key without Alt: worth a log line, since it is a VT chord with the wrong modifiers.
pub fn near_miss(mods: Mods, raw_syms: &[Keysym]) -> bool {
    mods.contains(Mods::CTRL) && !ctrl_alt(mods) && raw_syms.iter().any(|s| is_f_key(s.raw()))
}

/// True when `classify` could fire for a key with this chord, so the config must not bind it.
pub fn is_reserved(chord: &Chord) -> bool {
    let Trigger::Key(sym) = chord.trigger else {
        return false;
    };
    if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&sym) {
        return true;
    }
    ctrl_alt(chord.mods)
        && (matches!(sym, keysyms::KEY_BackSpace | keysyms::KEY_Terminate_Server) || is_f_key(sym))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(mods: Mods, code: u32, sym: u32, modified: u32) -> Option<Emergency> {
        classify(
            mods,
            Keycode::new(code),
            &[Keysym::new(sym)],
            Keysym::new(modified),
        )
    }

    #[test]
    fn emergency_chords() {
        let ctrl_alt = Mods::CTRL | Mods::ALT;
        let ctrl_altgr = Mods::CTRL | Mods::ALTGR;
        let bs = keysyms::KEY_BackSpace;
        let quit = Some(Emergency::Quit);
        assert_eq!(run(ctrl_alt, KEYCODE_BACKSPACE, bs, bs), quit);
        assert_eq!(run(ctrl_altgr, KEYCODE_BACKSPACE, bs, bs), quit);
        let f7 = keysyms::KEY_F7;
        assert_eq!(run(ctrl_altgr, 65, f7, f7), Some(Emergency::VtSwitch(7)));
        let vt3 = keysyms::KEY_XF86Switch_VT_3;
        assert_eq!(run(Mods::NONE, 65, vt3, vt3), Some(Emergency::VtSwitch(3)));

        let f1 = keysyms::KEY_F1;
        assert_eq!(run(Mods::CTRL, 59, f1, f1), None);
        assert_eq!(run(Mods::ALT, KEYCODE_BACKSPACE, bs, bs), None);
        let q = keysyms::KEY_q;
        assert_eq!(run(Mods::LOGO | ctrl_alt, 24, q, q), None);
    }

    #[test]
    fn reserved_chords() {
        let key = |mods, sym| Chord {
            mods,
            trigger: Trigger::Key(sym),
        };
        assert!(is_reserved(&key(
            Mods::CTRL | Mods::ALT,
            keysyms::KEY_BackSpace
        )));
        assert!(is_reserved(&key(
            Mods::CTRL | Mods::ALTGR | Mods::LOGO,
            keysyms::KEY_F1
        )));
        assert!(is_reserved(&key(Mods::NONE, keysyms::KEY_XF86Switch_VT_5)));
        assert!(!is_reserved(&key(Mods::CTRL, keysyms::KEY_F1)));
        assert!(!is_reserved(&key(Mods::LOGO, keysyms::KEY_BackSpace)));
    }
}
