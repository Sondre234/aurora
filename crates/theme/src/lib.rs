//! Aurora theme: palette, fonts, shape and motion.
//!
//! The compositor loads a theme from TOML (feature `toml`, on by default), then pushes a
//! [`ThemeSnapshot`] to services as `Event::Theme` so they repaint without a restart.
//! The types derive plain serde with no skipped or flattened fields, so they are safe for
//! postcard (not self-describing). Loading uses the same ladder as the compositor config:
//! a missing file gives defaults, a TOML syntax error is an `Err` (the caller keeps the
//! previous theme), a bad value is dropped alone with a warning, an unknown key is only a
//! warning. See [`Theme::from_toml_str`] for the file format.
//!
//! Evolution: new fields are appended to structs and get a default in [`Default`]; a wire
//! change that is not append-only needs an `aurora-ipc` `PROTO_VERSION` bump.

use serde::{Deserialize, Serialize};

#[cfg(feature = "toml")]
mod load;

/// An sRGB color with straight alpha, 8 bits per channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgba(pub [u8; 4]);

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self([r, g, b, 255])
    }

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self([r, g, b, a])
    }

    /// `#rrggbb` or `#rrggbbaa`.
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix('#')?;
        if !(hex.len() == 6 || hex.len() == 8) || !hex.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        let a = if hex.len() == 8 { byte(6)? } else { 255 };
        Some(Self([byte(0)?, byte(2)?, byte(4)?, a]))
    }

    /// `#rrggbb` when opaque, else `#rrggbbaa`. Round-trips through [`Rgba::parse`].
    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.0;
        if a == 255 {
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
        }
    }

    /// Straight-alpha floats in 0..=1, as the compositor's shaders take them.
    pub fn to_f32(self) -> [f32; 4] {
        self.0.map(|c| c as f32 / 255.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Palette {
    /// Window/bar backdrop.
    pub bg: Rgba,
    /// Raised surfaces: cards, popups, the launcher.
    pub surface: Rgba,
    /// Hovered or selected rows on a surface.
    pub surface_alt: Rgba,
    pub fg: Rgba,
    pub fg_dim: Rgba,
    pub accent: Rgba,
    /// Text on top of `accent`.
    pub accent_fg: Rgba,
    pub urgent: Rgba,
    pub border: Rgba,
    pub border_active: Rgba,
    pub shadow: Rgba,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fonts {
    pub family: String,
    /// Points at scale 1.
    pub size: f32,
    pub mono_family: String,
    pub mono_size: f32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shape {
    /// Corner radius in logical px.
    pub radius: u32,
    /// Gap between tiled windows and around the bar, logical px.
    pub gap: u32,
    pub border_width: u32,
    pub bar_height: u32,
}

/// Motion curves are kept as text in the compositor's `anim::Curve::parse` format:
/// `linear`, `ease-out`, `ease-in-out`, `spring [ratio]`, `bezier x1 y1 x2 y2`
/// (also `cubic-bezier(x1, y1, x2, y2)`). Services that animate parse them themselves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Motion {
    pub duration_ms: u32,
    pub curve: String,
    pub fade_curve: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    pub palette: Palette,
    pub fonts: Fonts,
    pub shape: Shape,
    pub motion: Motion,
}

/// What `Event::Theme` carries: the whole theme plus a revision that grows on every
/// change, so a service can drop a stale or duplicate push.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ThemeSnapshot {
    pub rev: u64,
    pub theme: Theme,
}

impl ThemeSnapshot {
    pub fn new(rev: u64, theme: Theme) -> Self {
        Self { rev, theme }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            bg: Rgba::new(0x14, 0x15, 0x1c, 0xd9),
            surface: Rgba::new(0x1e, 0x20, 0x2b, 0xe6),
            surface_alt: Rgba::rgb(0x2a, 0x2d, 0x3c),
            fg: Rgba::rgb(0xe6, 0xe8, 0xf2),
            fg_dim: Rgba::rgb(0x8a, 0x8f, 0xa8),
            accent: Rgba::rgb(0x7a, 0xa2, 0xf7),
            accent_fg: Rgba::rgb(0x10, 0x12, 0x1a),
            urgent: Rgba::rgb(0xf7, 0x76, 0x8e),
            border: Rgba::rgb(0x2f, 0x33, 0x45),
            border_active: Rgba::rgb(0x7a, 0xa2, 0xf7),
            shadow: Rgba::new(0, 0, 0, 0x99),
        }
    }
}

impl Default for Fonts {
    fn default() -> Self {
        Self {
            family: "sans-serif".into(),
            size: 11.0,
            mono_family: "monospace".into(),
            mono_size: 11.0,
        }
    }
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            radius: 10,
            gap: 8,
            border_width: 2,
            bar_height: 34,
        }
    }
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            duration_ms: 200,
            curve: "ease-out".into(),
            fade_curve: "linear".into(),
        }
    }
}

/// Mirror of the grammar accepted by the compositor's `anim::Curve::parse`; keep in sync.
pub fn curve_is_valid(text: &str) -> bool {
    let text = text.trim().to_ascii_lowercase();
    let inner = text
        .strip_prefix("cubic-bezier(")
        .and_then(|t| t.strip_suffix(')'))
        .map(|t| format!("bezier {t}"));
    let text = inner.as_deref().unwrap_or(&text);
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .collect();
    let num = |w: &str| w.parse::<f32>().ok().filter(|v| v.is_finite());
    match words.as_slice() {
        ["linear"] | ["ease-out"] | ["ease-in-out"] | ["spring"] => true,
        ["spring", r] => num(r).is_some(),
        ["bezier", a, b, c, d] => match (num(a), num(b), num(c), num(d)) {
            (Some(x1), Some(_), Some(x2), Some(_)) => {
                (0.0..=1.0).contains(&x1) && (0.0..=1.0).contains(&x2)
            }
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_parse_and_hex_roundtrip() {
        assert_eq!(Rgba::parse("#ff000080"), Some(Rgba([255, 0, 0, 128])));
        assert_eq!(Rgba::parse("#00ff00"), Some(Rgba([0, 255, 0, 255])));
        for bad in ["ff0000", "#ff00", "#gg0000", "#ff0000800", "#é12345", ""] {
            assert_eq!(Rgba::parse(bad), None, "{bad}");
        }
        for c in [Rgba::rgb(1, 2, 3), Rgba::new(9, 8, 7, 6)] {
            assert_eq!(Rgba::parse(&c.to_hex()), Some(c));
        }
        assert_eq!(Rgba::new(255, 0, 0, 255).to_f32(), [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn curve_grammar_matches_compositor() {
        for ok in [
            "linear",
            "Ease-Out",
            "ease-in-out",
            "spring",
            "spring 0.6",
            "bezier 0.2 0 0 1",
            "cubic-bezier(0.2, 0, 0, 1.4)",
        ] {
            assert!(curve_is_valid(ok), "{ok}");
        }
        for bad in [
            "",
            "wobble",
            "spring x",
            "bezier 1 2 3",
            "bezier 1.5 0 0 1",
            "spring nan",
        ] {
            assert!(!curve_is_valid(bad), "{bad}");
        }
    }

    #[test]
    fn default_curves_are_valid() {
        let m = Motion::default();
        assert!(curve_is_valid(&m.curve) && curve_is_valid(&m.fade_curve));
    }
}
