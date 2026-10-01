//! Keyboard input to bytes for the child: legacy xterm encoding, no kitty protocol.
//!
//! [`press_from_ui`] maps a toolkit key event to a [`Press`], [`encode_key`] turns a
//! press into the bytes a program expects given the [`Modes`] it enabled. Both are pure.
//! Paste handling ([`paste_bytes`]) lives here too since it is the other way text
//! reaches the child.

use aurora_ui::{Key, KeyEvent};

/// Terminal modes that change how keys are encoded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modes {
    /// DECCKM: arrows, Home and End send `ESC O x` instead of `ESC [ x`.
    pub app_cursor: bool,
    /// DECKPAM: the keypad sends `ESC O x` sequences.
    pub app_keypad: bool,
}

/// A key that is not just text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    Enter,
    Escape,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1 to F20.
    F(u8),
    /// A keypad key, by its X11 keysym (`KP_0` .. `KP_9`, `KP_Add`, ...).
    Keypad(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Named(Named),
    /// A printable key: the layout's un-shifted character and the text it produced.
    Char {
        base: char,
        text: Option<String>,
    },
    /// Anything else that produced text (dead-key compositions, ...).
    Text(String),
}

/// A key press with its modifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Press {
    pub kind: Kind,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

const KP_FIRST: u32 = 0xff80;
const KP_LAST: u32 = 0xffbd;
const F1: u32 = 0xffbe;
const F20: u32 = 0xffd1;
const INSERT: u32 = 0xff63;

/// Map a toolkit key event. `None` for keys that never produce input (modifiers, locks,
/// the menu key, anything unknown without text).
pub fn press_from_ui(ev: &KeyEvent) -> Option<Press> {
    let kind = match ev.key {
        Key::Enter => Kind::Named(Named::Enter),
        Key::Escape => Kind::Named(Named::Escape),
        Key::Backspace => Kind::Named(Named::Backspace),
        Key::Delete => Kind::Named(Named::Delete),
        Key::Tab => Kind::Named(Named::Tab),
        Key::Left => Kind::Named(Named::Left),
        Key::Right => Kind::Named(Named::Right),
        Key::Up => Kind::Named(Named::Up),
        Key::Down => Kind::Named(Named::Down),
        Key::Home => Kind::Named(Named::Home),
        Key::End => Kind::Named(Named::End),
        Key::PageUp => Kind::Named(Named::PageUp),
        Key::PageDown => Kind::Named(Named::PageDown),
        Key::Char(c) => Kind::Char {
            base: c,
            text: ev.text.clone().filter(|t| !t.is_empty()),
        },
        Key::Other(INSERT) => Kind::Named(Named::Insert),
        Key::Other(k @ F1..=F20) => Kind::Named(Named::F((k - F1 + 1) as u8)),
        Key::Other(k @ KP_FIRST..=KP_LAST) => Kind::Named(Named::Keypad(k)),
        Key::Other(_) => Kind::Text(ev.text.clone().filter(|t| !t.is_empty())?),
    };
    Some(Press {
        kind,
        ctrl: ev.mods.ctrl,
        alt: ev.mods.alt,
        shift: ev.mods.shift,
    })
}

/// xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4); 1 means "none".
fn mod_param(p: &Press) -> u8 {
    1 + p.shift as u8 + 2 * p.alt as u8 + 4 * p.ctrl as u8
}

fn csi(out: &mut Vec<u8>, first: Option<u8>, m: u8, fin: u8) {
    out.extend_from_slice(b"\x1b[");
    match (first, m) {
        (Some(n), 1) => out.extend_from_slice(n.to_string().as_bytes()),
        (Some(n), m) => out.extend_from_slice(format!("{n};{m}").as_bytes()),
        (None, 1) => {}
        (None, m) => out.extend_from_slice(format!("1;{m}").as_bytes()),
    }
    out.push(fin);
}

/// Control byte for Ctrl + `c`, if there is one.
fn ctrl_byte(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' | '~' => Some(0x1e),
        '_' | '/' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// The bytes to write to the child for `p`. Empty when the key sends nothing.
pub fn encode_key(p: &Press, modes: Modes) -> Vec<u8> {
    let mut out = Vec::new();
    let m = mod_param(p);
    match &p.kind {
        Kind::Char { base, text } => {
            if p.ctrl
                && let Some(b) = ctrl_byte(*base)
            {
                if p.alt {
                    out.push(0x1b);
                }
                out.push(b);
            } else if let Some(t) = text {
                if p.alt {
                    out.push(0x1b);
                }
                out.extend_from_slice(t.as_bytes());
            } else if p.alt && !p.ctrl {
                out.push(0x1b);
                let mut buf = [0u8; 4];
                out.extend_from_slice(base.encode_utf8(&mut buf).as_bytes());
            }
        }
        Kind::Text(t) => {
            if p.alt {
                out.push(0x1b);
            }
            out.extend_from_slice(t.as_bytes());
        }
        Kind::Named(n) => encode_named(&mut out, *n, p, m, modes),
    }
    out
}

fn encode_named(out: &mut Vec<u8>, n: Named, p: &Press, m: u8, modes: Modes) {
    // Alt on the simple keys is an ESC prefix.
    let alt_prefix = |out: &mut Vec<u8>| {
        if p.alt {
            out.push(0x1b);
        }
    };
    match n {
        Named::Enter => {
            alt_prefix(out);
            out.push(b'\r');
        }
        Named::Escape => {
            alt_prefix(out);
            out.push(0x1b);
        }
        Named::Backspace => {
            alt_prefix(out);
            out.push(if p.ctrl { 0x08 } else { 0x7f });
        }
        Named::Tab => {
            alt_prefix(out);
            if p.shift {
                out.extend_from_slice(b"\x1b[Z");
            } else {
                out.push(b'\t');
            }
        }
        Named::Up | Named::Down | Named::Left | Named::Right | Named::Home | Named::End => {
            let fin = match n {
                Named::Up => b'A',
                Named::Down => b'B',
                Named::Right => b'C',
                Named::Left => b'D',
                Named::Home => b'H',
                _ => b'F',
            };
            if m == 1 && modes.app_cursor {
                out.extend_from_slice(&[0x1b, b'O', fin]);
            } else {
                csi(out, None, m, fin);
            }
        }
        Named::Insert => csi(out, Some(2), m, b'~'),
        Named::Delete => csi(out, Some(3), m, b'~'),
        Named::PageUp => csi(out, Some(5), m, b'~'),
        Named::PageDown => csi(out, Some(6), m, b'~'),
        Named::F(k) => match k {
            1..=4 => {
                let fin = b'P' + (k - 1);
                if m == 1 {
                    out.extend_from_slice(&[0x1b, b'O', fin]);
                } else {
                    csi(out, None, m, fin);
                }
            }
            5..=20 => {
                let code = [
                    15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34,
                ];
                csi(out, Some(code[(k - 5) as usize]), m, b'~');
            }
            _ => {}
        },
        Named::Keypad(sym) => encode_keypad(out, sym, p, m, modes),
    }
}

/// Keypad keys: digits and operators as text unless application keypad mode is on.
fn encode_keypad(out: &mut Vec<u8>, sym: u32, p: &Press, m: u8, modes: Modes) {
    let (plain, app): (Option<u8>, Option<u8>) = match sym {
        0xff8d => (Some(b'\r'), Some(b'M')),
        0xffaa => (Some(b'*'), Some(b'j')),
        0xffab => (Some(b'+'), Some(b'k')),
        0xffac => (Some(b','), Some(b'l')),
        0xffad => (Some(b'-'), Some(b'm')),
        0xffae => (Some(b'.'), Some(b'n')),
        0xffaf => (Some(b'/'), Some(b'o')),
        0xffbd => (Some(b'='), Some(b'X')),
        0xffb0..=0xffb9 => {
            let d = (sym - 0xffb0) as u8;
            (Some(b'0' + d), Some(b'p' + d))
        }
        _ => (None, None),
    };
    if modes.app_keypad && m == 1 {
        if let Some(f) = app {
            out.extend_from_slice(&[0x1b, b'O', f]);
        }
    } else if let Some(b) = plain {
        if p.alt {
            out.push(0x1b);
        }
        out.push(b);
    }
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// Bytes to write for pasted `text`. With bracketed paste on, the text is wrapped in the
/// paste markers after removing every embedded end marker (so pasted text cannot end
/// the bracket early and run as typed commands). Without it, newlines become carriage
/// returns like typed Enter.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut clean = text.to_string();
        // Removing a marker can join its neighbours into a new one, so repeat.
        while clean.contains(PASTE_END) {
            clean = clean.replace(PASTE_END, "");
        }
        let mut out = Vec::with_capacity(clean.len() + 12);
        out.extend_from_slice(PASTE_START);
        out.extend_from_slice(clean.as_bytes());
        out.extend_from_slice(PASTE_END.as_bytes());
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_ui::Mods;

    fn named(n: Named) -> Press {
        Press {
            kind: Kind::Named(n),
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    fn ch(c: char) -> Press {
        Press {
            kind: Kind::Char {
                base: c,
                text: Some(c.to_string()),
            },
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    fn enc(p: &Press) -> Vec<u8> {
        encode_key(p, Modes::default())
    }

    #[test]
    fn text_and_alt_prefix() {
        assert_eq!(enc(&ch('a')), b"a");
        let mut p = ch('x');
        p.alt = true;
        assert_eq!(enc(&p), b"\x1bx");
        // Shifted text comes from the key's text, not its base character.
        let p = Press {
            kind: Kind::Char {
                base: 'a',
                text: Some("A".into()),
            },
            ..ch('a')
        };
        assert_eq!(enc(&p), b"A");
    }

    #[test]
    fn ctrl_letters_and_punctuation() {
        let mut p = ch('c');
        p.ctrl = true;
        assert_eq!(enc(&p), [3]);
        p.shift = true;
        assert_eq!(enc(&p), [3]);
        p.alt = true;
        assert_eq!(enc(&p), [0x1b, 3]);
        let mut sp = ch(' ');
        sp.ctrl = true;
        assert_eq!(enc(&sp), [0]);
        let mut br = ch('[');
        br.ctrl = true;
        assert_eq!(enc(&br), [0x1b]);
        let mut q = ch('?');
        q.ctrl = true;
        assert_eq!(enc(&q), [0x7f]);
        // No control byte for a plain digit that has none: the text goes through.
        let mut nine = ch('9');
        nine.ctrl = true;
        assert_eq!(enc(&nine), b"9");
    }

    #[test]
    fn simple_named_keys() {
        assert_eq!(enc(&named(Named::Enter)), b"\r");
        assert_eq!(enc(&named(Named::Escape)), b"\x1b");
        assert_eq!(enc(&named(Named::Backspace)), [0x7f]);
        assert_eq!(enc(&named(Named::Tab)), b"\t");
        let mut st = named(Named::Tab);
        st.shift = true;
        assert_eq!(enc(&st), b"\x1b[Z");
        let mut cb = named(Named::Backspace);
        cb.ctrl = true;
        assert_eq!(enc(&cb), [0x08]);
        let mut ae = named(Named::Enter);
        ae.alt = true;
        assert_eq!(enc(&ae), b"\x1b\r");
    }

    #[test]
    fn arrows_follow_application_cursor_mode() {
        let up = named(Named::Up);
        assert_eq!(enc(&up), b"\x1b[A");
        let app = Modes {
            app_cursor: true,
            ..Modes::default()
        };
        assert_eq!(encode_key(&up, app), b"\x1bOA");
        assert_eq!(encode_key(&named(Named::Home), app), b"\x1bOH");
        // Modifiers always use the CSI form.
        let mut ctrl_left = named(Named::Left);
        ctrl_left.ctrl = true;
        assert_eq!(encode_key(&ctrl_left, app), b"\x1b[1;5D");
        let mut sa = named(Named::Right);
        sa.shift = true;
        sa.alt = true;
        assert_eq!(enc(&sa), b"\x1b[1;4C");
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(&named(Named::Insert)), b"\x1b[2~");
        assert_eq!(enc(&named(Named::Delete)), b"\x1b[3~");
        assert_eq!(enc(&named(Named::PageUp)), b"\x1b[5~");
        assert_eq!(enc(&named(Named::PageDown)), b"\x1b[6~");
        let mut d = named(Named::Delete);
        d.ctrl = true;
        assert_eq!(enc(&d), b"\x1b[3;5~");
        assert_eq!(enc(&named(Named::End)), b"\x1b[F");
    }

    #[test]
    fn function_keys() {
        assert_eq!(enc(&named(Named::F(1))), b"\x1bOP");
        assert_eq!(enc(&named(Named::F(4))), b"\x1bOS");
        assert_eq!(enc(&named(Named::F(5))), b"\x1b[15~");
        assert_eq!(enc(&named(Named::F(10))), b"\x1b[21~");
        assert_eq!(enc(&named(Named::F(12))), b"\x1b[24~");
        assert_eq!(enc(&named(Named::F(20))), b"\x1b[34~");
        let mut s = named(Named::F(1));
        s.shift = true;
        assert_eq!(enc(&s), b"\x1b[1;2P");
        let mut c = named(Named::F(5));
        c.ctrl = true;
        assert_eq!(enc(&c), b"\x1b[15;5~");
        assert!(enc(&named(Named::F(21))).is_empty());
    }

    #[test]
    fn keypad_modes() {
        let kp5 = named(Named::Keypad(0xffb5));
        assert_eq!(enc(&kp5), b"5");
        let app = Modes {
            app_keypad: true,
            ..Modes::default()
        };
        assert_eq!(encode_key(&kp5, app), b"\x1bOu");
        assert_eq!(encode_key(&named(Named::Keypad(0xffab)), app), b"\x1bOk");
        assert_eq!(enc(&named(Named::Keypad(0xffab))), b"+");
    }

    #[test]
    fn toolkit_mapping() {
        let mut ev = KeyEvent::new(Key::Other(0xffbe + 4));
        assert_eq!(
            press_from_ui(&ev).map(|p| p.kind),
            Some(Kind::Named(Named::F(5)))
        );
        ev = KeyEvent::new(Key::Other(0xffe1)); // Shift_L alone
        assert_eq!(press_from_ui(&ev), None);
        ev = KeyEvent::new(Key::Other(INSERT));
        assert_eq!(
            press_from_ui(&ev).map(|p| p.kind),
            Some(Kind::Named(Named::Insert))
        );
        ev = KeyEvent::new(Key::Other(0xffb2)); // KP_2
        assert_eq!(
            press_from_ui(&ev).map(|p| p.kind),
            Some(Kind::Named(Named::Keypad(0xffb2)))
        );
        let typed = KeyEvent::typed('q').with_mods(Mods {
            ctrl: true,
            ..Mods::default()
        });
        let p = press_from_ui(&typed).expect("press");
        assert!(p.ctrl && !p.alt && !p.shift);
        assert_eq!(enc(&p), [0x11]);
        // A dead-key composition arrives as text on an unknown keysym.
        let mut dead = KeyEvent::new(Key::Other(0xfe51));
        dead.text = Some("é".into());
        assert_eq!(enc(&press_from_ui(&dead).expect("press")), "é".as_bytes());
    }

    #[test]
    fn bracketed_paste_wraps_and_strips_end_markers() {
        assert_eq!(paste_bytes("ls\nrm", true), b"\x1b[200~ls\nrm\x1b[201~");
        assert_eq!(
            paste_bytes("a\x1b[201~b", true),
            b"\x1b[200~ab\x1b[201~".to_vec()
        );
        // A marker assembled from the remains of another one is removed as well.
        assert_eq!(
            paste_bytes("\x1b[2\x1b[201~01~x", true),
            b"\x1b[200~x\x1b[201~".to_vec()
        );
    }

    #[test]
    fn plain_paste_turns_newlines_into_returns() {
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc");
        assert_eq!(paste_bytes("\x1b[200~", false), b"\x1b[200~");
    }
}
