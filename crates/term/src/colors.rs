//! Terminal colors derived from the theme palette. Pure.
//!
//! The 16 ANSI colors come from the palette (`ansi16`), 16..=255 are the fixed xterm
//! cube and gray ramp, and the named slots above 255 follow alacritty's numbering (the
//! same indices OSC 4/10/11 and color requests use).

use aurora_theme::{Palette, Rgba};

pub const FOREGROUND: u16 = 256;
pub const BACKGROUND: u16 = 257;
pub const CURSOR: u16 = 258;
/// Dim variants of ANSI 0..=7 occupy 259..=266.
pub const DIM_FIRST: u16 = 259;
pub const BRIGHT_FOREGROUND: u16 = 267;
pub const DIM_BACKGROUND: u16 = 268;

/// A color as stored in a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRef {
    /// 0..=255 palette, or one of the named slots above.
    Index(u16),
    Rgb(Rgba),
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Rgba([
        m(a.0[0], b.0[0]),
        m(a.0[1], b.0[1]),
        m(a.0[2], b.0[2]),
        m(a.0[3], b.0[3]),
    ])
}

fn opaque(c: Rgba) -> Rgba {
    Rgba([c.0[0], c.0[1], c.0[2], 255])
}

/// The 16 ANSI colors for a palette: black is a raised surface, bright black the dim
/// foreground, white a softened foreground, red the urgent color and blue the accent.
/// Green, yellow, magenta and cyan are fixed hues tuned to the default theme's tone; the
/// bright row mixes every color towards white.
pub fn ansi16(p: &Palette) -> [Rgba; 16] {
    let white = Rgba::rgb(255, 255, 255);
    let fg = opaque(p.fg);
    let bg = opaque(p.bg);
    let normal = [
        opaque(p.surface_alt),
        opaque(p.urgent),
        Rgba::rgb(0x9e, 0xce, 0x6a),
        Rgba::rgb(0xe0, 0xaf, 0x68),
        opaque(p.accent),
        Rgba::rgb(0xbb, 0x9a, 0xf7),
        Rgba::rgb(0x7d, 0xcf, 0xff),
        mix(fg, bg, 0.2),
    ];
    let mut out = [Rgba::rgb(0, 0, 0); 16];
    out[..8].copy_from_slice(&normal);
    for i in 1..7 {
        out[8 + i] = mix(normal[i], white, 0.3);
    }
    out[8] = opaque(p.fg_dim);
    out[15] = fg;
    out
}

/// Entry `i` (16..=255) of the xterm 256-color table: a 6x6x6 cube, then 24 grays.
pub fn cube_or_gray(i: u8) -> Rgba {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match i {
        16..=231 => {
            let n = i - 16;
            Rgba::rgb(
                LEVELS[(n / 36) as usize],
                LEVELS[(n / 6 % 6) as usize],
                LEVELS[(n % 6) as usize],
            )
        }
        232..=255 => {
            let v = 8 + 10 * (i - 232);
            Rgba::rgb(v, v, v)
        }
        _ => Rgba::rgb(0, 0, 0),
    }
}

/// Resolved colors for one theme.
#[derive(Debug, Clone, PartialEq)]
pub struct Scheme {
    pub ansi: [Rgba; 16],
    /// Default foreground.
    pub fg: Rgba,
    /// Default background with the theme's alpha (the compositor blurs behind it).
    pub bg: Rgba,
    /// The default background made opaque, for blending text over it.
    pub bg_solid: Rgba,
    pub cursor: Rgba,
    pub cursor_text: Rgba,
    pub selection: Rgba,
}

impl Scheme {
    pub fn from_palette(p: &Palette) -> Self {
        let bg_solid = opaque(p.bg);
        Self {
            ansi: ansi16(p),
            fg: p.fg,
            bg: p.bg,
            bg_solid,
            cursor: opaque(p.accent),
            cursor_text: opaque(p.accent_fg),
            selection: mix(bg_solid, opaque(p.accent), 0.35),
        }
    }

    /// Color of palette slot `i`, without dynamic overrides.
    pub fn lookup(&self, i: u16) -> Rgba {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=255 => cube_or_gray(i as u8),
            FOREGROUND => self.fg,
            BACKGROUND => self.bg,
            CURSOR => self.cursor,
            DIM_FIRST..=266 => self.dim(self.ansi[(i - DIM_FIRST) as usize]),
            BRIGHT_FOREGROUND => mix(opaque(self.fg), Rgba::rgb(255, 255, 255), 0.3),
            DIM_BACKGROUND => mix(self.bg, Rgba([0, 0, 0, self.bg.0[3]]), 0.33),
            _ => self.fg,
        }
    }

    /// Dimmed text color (SGR 2): two thirds brightness.
    pub fn dim(&self, c: Rgba) -> Rgba {
        mix(c, Rgba([0, 0, 0, c.0[3]]), 1.0 / 3.0)
    }

    /// Resolve a cell color; `over` supplies colors the program changed through OSC 4/10/11.
    pub fn resolve(&self, c: ColorRef, over: &dyn Fn(u16) -> Option<Rgba>) -> Rgba {
        match c {
            ColorRef::Rgb(rgb) => rgb,
            ColorRef::Index(i) => over(i).unwrap_or_else(|| self.lookup(i)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_row_is_derived_from_the_palette() {
        let p = Palette::default();
        let a = ansi16(&p);
        assert_eq!(a[1], opaque(p.urgent));
        assert_eq!(a[4], opaque(p.accent));
        assert_eq!(a[0], opaque(p.surface_alt));
        assert_eq!(a[8], opaque(p.fg_dim));
        assert_eq!(a[15], opaque(p.fg));
        assert!(a.iter().all(|c| c.0[3] == 255));
        // Bright variants are lighter than their normal color.
        for i in 1..7 {
            let lum = |c: Rgba| c.0[0] as u32 + c.0[1] as u32 + c.0[2] as u32;
            assert!(lum(a[8 + i]) > lum(a[i]), "color {i}");
        }
    }

    #[test]
    fn a_new_palette_changes_the_row() {
        let p = Palette {
            accent: Rgba::rgb(1, 2, 3),
            ..Palette::default()
        };
        assert_eq!(ansi16(&p)[4], Rgba::rgb(1, 2, 3));
    }

    #[test]
    fn cube_and_gray_match_xterm() {
        assert_eq!(cube_or_gray(16), Rgba::rgb(0, 0, 0));
        assert_eq!(cube_or_gray(21), Rgba::rgb(0, 0, 255));
        assert_eq!(cube_or_gray(196), Rgba::rgb(255, 0, 0));
        assert_eq!(cube_or_gray(231), Rgba::rgb(255, 255, 255));
        assert_eq!(cube_or_gray(67), Rgba::rgb(95, 135, 175));
        assert_eq!(cube_or_gray(232), Rgba::rgb(8, 8, 8));
        assert_eq!(cube_or_gray(255), Rgba::rgb(238, 238, 238));
    }

    #[test]
    fn resolution_order() {
        let s = Scheme::from_palette(&Palette::default());
        let none = |_: u16| None;
        assert_eq!(s.resolve(ColorRef::Index(1), &none), s.ansi[1]);
        assert_eq!(s.resolve(ColorRef::Index(FOREGROUND), &none), s.fg);
        assert_eq!(s.resolve(ColorRef::Index(BACKGROUND), &none), s.bg);
        assert_eq!(s.resolve(ColorRef::Index(200), &none), cube_or_gray(200));
        let rgb = Rgba::rgb(9, 8, 7);
        assert_eq!(s.resolve(ColorRef::Rgb(rgb), &none), rgb);
        // A program-set color wins over the table.
        let over = |i: u16| (i == 1).then_some(rgb);
        assert_eq!(s.resolve(ColorRef::Index(1), &over), rgb);
        assert_eq!(s.resolve(ColorRef::Index(2), &over), s.ansi[2]);
    }

    #[test]
    fn dim_slots_are_darker() {
        let s = Scheme::from_palette(&Palette::default());
        let d = s.lookup(DIM_FIRST + 2);
        assert!(d.0[1] < s.ansi[2].0[1]);
        assert_eq!(d, s.dim(s.ansi[2]));
    }

    #[test]
    fn default_background_keeps_the_theme_alpha() {
        let p = Palette::default();
        let s = Scheme::from_palette(&p);
        assert_eq!(s.bg.0[3], p.bg.0[3]);
        assert_eq!(s.bg_solid.0[3], 255);
        assert_eq!(&s.bg_solid.0[..3], &p.bg.0[..3]);
    }
}
