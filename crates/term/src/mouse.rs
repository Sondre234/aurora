//! Mouse reporting to the child (X10/normal and SGR 1006 encodings). Pure.

/// Which events the program asked to receive (DECSET 1000, 1002, 1003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reporting {
    #[default]
    Off,
    /// Presses, releases and the wheel.
    Click,
    /// Plus motion while a button is held.
    Drag,
    /// Plus all motion.
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    fn code(self) -> u32 {
        match self {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Press(Button),
    Release(Button),
    /// Pointer motion, with the button held if any.
    Motion(Option<Button>),
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// Whether `kind` is reported under `mode`.
pub fn reported(mode: Reporting, kind: Kind) -> bool {
    match (mode, kind) {
        (Reporting::Off, _) => false,
        (_, Kind::Motion(None)) => mode == Reporting::Motion,
        (_, Kind::Motion(Some(_))) => matches!(mode, Reporting::Drag | Reporting::Motion),
        _ => true,
    }
}

/// The report for an event at the zero-based cell (`col`, `row`), or `None` when the mode
/// does not report it or the legacy encoding cannot express the position.
pub fn encode(
    mode: Reporting,
    sgr: bool,
    kind: Kind,
    col: usize,
    row: usize,
    mods: Mods,
) -> Option<Vec<u8>> {
    if !reported(mode, kind) {
        return None;
    }
    let mut b = match kind {
        Kind::Press(btn) => btn.code(),
        // Legacy releases do not say which button; SGR does.
        Kind::Release(btn) => {
            if sgr {
                btn.code()
            } else {
                3
            }
        }
        Kind::Motion(Some(btn)) => btn.code() + 32,
        Kind::Motion(None) => 3 + 32,
        Kind::WheelUp => 64,
        Kind::WheelDown => 65,
    };
    b += 4 * mods.shift as u32 + 8 * mods.alt as u32 + 16 * mods.ctrl as u32;
    if sgr {
        let fin = if matches!(kind, Kind::Release(_)) {
            'm'
        } else {
            'M'
        };
        Some(format!("\x1b[<{};{};{}{}", b, col + 1, row + 1, fin).into_bytes())
    } else {
        // One byte per value, offset by 32: positions past 222 do not fit.
        let (x, y) = (col + 1 + 32, row + 1 + 32);
        if x > 255 || y > 255 {
            return None;
        }
        Some(vec![0x1b, b'[', b'M', (b + 32) as u8, x as u8, y as u8])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        shift: false,
        alt: false,
        ctrl: false,
    };

    #[test]
    fn reporting_levels() {
        let motion = Kind::Motion(None);
        let drag = Kind::Motion(Some(Button::Left));
        assert!(!reported(Reporting::Off, Kind::Press(Button::Left)));
        assert!(reported(Reporting::Click, Kind::Press(Button::Left)));
        assert!(reported(Reporting::Click, Kind::WheelUp));
        assert!(!reported(Reporting::Click, drag));
        assert!(reported(Reporting::Drag, drag));
        assert!(!reported(Reporting::Drag, motion));
        assert!(reported(Reporting::Motion, motion));
    }

    #[test]
    fn sgr_encoding() {
        let press = encode(
            Reporting::Click,
            true,
            Kind::Press(Button::Left),
            0,
            0,
            NONE,
        );
        assert_eq!(press.as_deref(), Some(&b"\x1b[<0;1;1M"[..]));
        let release = encode(
            Reporting::Click,
            true,
            Kind::Release(Button::Right),
            9,
            4,
            NONE,
        );
        assert_eq!(release.as_deref(), Some(&b"\x1b[<2;10;5m"[..]));
        let ctrl_wheel = Mods { ctrl: true, ..NONE };
        let wheel = encode(Reporting::Click, true, Kind::WheelDown, 1, 1, ctrl_wheel);
        assert_eq!(wheel.as_deref(), Some(&b"\x1b[<81;2;2M"[..]));
        let drag = encode(
            Reporting::Drag,
            true,
            Kind::Motion(Some(Button::Middle)),
            300,
            2,
            NONE,
        );
        assert_eq!(drag.as_deref(), Some(&b"\x1b[<33;301;3M"[..]));
    }

    #[test]
    fn legacy_encoding_and_limits() {
        let press = encode(
            Reporting::Click,
            false,
            Kind::Press(Button::Left),
            0,
            0,
            NONE,
        );
        assert_eq!(press.as_deref(), Some(&[0x1b, b'[', b'M', 32, 33, 33][..]));
        let release = encode(
            Reporting::Click,
            false,
            Kind::Release(Button::Middle),
            1,
            2,
            NONE,
        );
        assert_eq!(
            release.as_deref(),
            Some(&[0x1b, b'[', b'M', 35, 34, 35][..])
        );
        let far = encode(
            Reporting::Click,
            false,
            Kind::Press(Button::Left),
            250,
            0,
            NONE,
        );
        assert_eq!(far, None);
        assert_eq!(
            encode(Reporting::Off, true, Kind::Press(Button::Left), 0, 0, NONE),
            None
        );
    }
}
