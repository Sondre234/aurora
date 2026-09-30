//! TOML loading with the compositor's config ladder.

use std::path::Path;

use toml::{Table, Value};

use crate::{Rgba, Theme, curve_is_valid};

impl Theme {
    /// Parses a theme file. Sections: `[palette]` (colors as `"#rrggbb[aa]"`), `[fonts]`
    /// (`family`, `size`, `mono_family`, `mono_size`), `[shape]` (`radius`, `gap`,
    /// `border_width`, `bar_height`), `[motion]` (`duration_ms`, `curve`, `fade_curve`).
    /// Every key is optional. `Err` only for a TOML syntax error; bad values keep the
    /// default and unknown keys are warnings.
    pub fn from_toml_str(text: &str) -> Result<(Theme, Vec<String>), String> {
        let table: Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
        let mut theme = Theme::default();
        let mut w = Vec::new();
        check_keys(
            "theme",
            &table,
            &["palette", "fonts", "shape", "motion"],
            &mut w,
        );

        if let Some(t) = section("palette", &table, &mut w) {
            let p = &mut theme.palette;
            let names = [
                "bg",
                "surface",
                "surface_alt",
                "fg",
                "fg_dim",
                "accent",
                "accent_fg",
                "urgent",
                "border",
                "border_active",
                "shadow",
            ];
            check_keys("palette", t, &names, &mut w);
            let slots: [(&str, &mut Rgba); 11] = [
                ("bg", &mut p.bg),
                ("surface", &mut p.surface),
                ("surface_alt", &mut p.surface_alt),
                ("fg", &mut p.fg),
                ("fg_dim", &mut p.fg_dim),
                ("accent", &mut p.accent),
                ("accent_fg", &mut p.accent_fg),
                ("urgent", &mut p.urgent),
                ("border", &mut p.border),
                ("border_active", &mut p.border_active),
                ("shadow", &mut p.shadow),
            ];
            for (key, slot) in slots {
                if let Some(text) = get_str("palette", t, key, &mut w) {
                    match Rgba::parse(&text) {
                        Some(c) => *slot = c,
                        None => w.push(format!(
                            "palette.{key}: invalid color {text:?} (want #rrggbb or #rrggbbaa)"
                        )),
                    }
                }
            }
        }

        if let Some(t) = section("fonts", &table, &mut w) {
            check_keys(
                "fonts",
                t,
                &["family", "size", "mono_family", "mono_size"],
                &mut w,
            );
            let f = &mut theme.fonts;
            for (key, slot) in [
                ("family", &mut f.family),
                ("mono_family", &mut f.mono_family),
            ] {
                if let Some(s) = get_str("fonts", t, key, &mut w) {
                    if s.trim().is_empty() {
                        w.push(format!("fonts.{key}: empty family"));
                    } else {
                        *slot = s;
                    }
                }
            }
            for (key, slot) in [("size", &mut f.size), ("mono_size", &mut f.mono_size)] {
                if let Some(v) = get_float("fonts", t, key, &mut w) {
                    if (1.0..=200.0).contains(&v) {
                        *slot = v;
                    } else {
                        w.push(format!("fonts.{key}: {v} out of range 1..=200"));
                    }
                }
            }
        }

        if let Some(t) = section("shape", &table, &mut w) {
            check_keys(
                "shape",
                t,
                &["radius", "gap", "border_width", "bar_height"],
                &mut w,
            );
            let s = &mut theme.shape;
            for (key, slot, max) in [
                ("radius", &mut s.radius, 200),
                ("gap", &mut s.gap, 200),
                ("border_width", &mut s.border_width, 50),
                ("bar_height", &mut s.bar_height, 500),
            ] {
                if let Some(v) = get_uint("shape", t, key, max, &mut w) {
                    *slot = v;
                }
            }
        }

        if let Some(t) = section("motion", &table, &mut w) {
            check_keys("motion", t, &["duration_ms", "curve", "fade_curve"], &mut w);
            let m = &mut theme.motion;
            if let Some(v) = get_uint("motion", t, "duration_ms", 10_000, &mut w) {
                m.duration_ms = v;
            }
            for (key, slot) in [("curve", &mut m.curve), ("fade_curve", &mut m.fade_curve)] {
                if let Some(s) = get_str("motion", t, key, &mut w) {
                    if curve_is_valid(&s) {
                        *slot = s;
                    } else {
                        w.push(format!(
                            "motion.{key}: unknown {s:?} (linear, ease-out, ease-in-out, spring [ratio], bezier x1 y1 x2 y2)"
                        ));
                    }
                }
            }
        }
        Ok((theme, w))
    }

    /// Loads `path`. A missing file gives the defaults; `Err` is an unreadable file or a
    /// syntax error, and the caller should keep its previous theme.
    pub fn load(path: &Path) -> Result<(Theme, Vec<String>), String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Theme::default(), vec![])),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

fn check_keys(ctx: &str, table: &Table, known: &[&str], w: &mut Vec<String>) {
    for key in table.keys().filter(|k| !known.contains(&k.as_str())) {
        w.push(format!("{ctx}: unknown key {key:?}"));
    }
}

fn section<'a>(name: &str, table: &'a Table, w: &mut Vec<String>) -> Option<&'a Table> {
    match table.get(name)? {
        Value::Table(t) => Some(t),
        other => {
            w.push(format!("{name}: expected table, got {}", other.type_str()));
            None
        }
    }
}

fn get_str(ctx: &str, t: &Table, key: &str, w: &mut Vec<String>) -> Option<String> {
    match t.get(key)? {
        Value::String(s) => Some(s.clone()),
        other => {
            w.push(format!(
                "{ctx}.{key}: expected string, got {}",
                other.type_str()
            ));
            None
        }
    }
}

fn get_float(ctx: &str, t: &Table, key: &str, w: &mut Vec<String>) -> Option<f32> {
    match t.get(key)? {
        Value::Float(f) if f.is_finite() => Some(*f as f32),
        Value::Integer(i) => Some(*i as f32),
        other => {
            w.push(format!(
                "{ctx}.{key}: expected number, got {}",
                other.type_str()
            ));
            None
        }
    }
}

fn get_uint(ctx: &str, t: &Table, key: &str, max: u32, w: &mut Vec<String>) -> Option<u32> {
    match t.get(key)? {
        Value::Integer(i) if (0..=max as i64).contains(i) => Some(*i as u32),
        Value::Integer(i) => {
            w.push(format!("{ctx}.{key}: {i} out of range 0..={max}"));
            None
        }
        other => {
            w.push(format!(
                "{ctx}.{key}: expected integer, got {}",
                other.type_str()
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_defaults() {
        let (t, w) = Theme::from_toml_str("").unwrap();
        assert_eq!(t, Theme::default());
        assert!(w.is_empty());
    }

    #[test]
    fn full_file_applies() {
        let (t, w) = Theme::from_toml_str(
            r##"
[palette]
bg = "#000000"
accent = "#ff8800cc"
[fonts]
family = "Inter"
size = 12
mono_size = 9.5
[shape]
radius = 0
bar_height = 40
[motion]
duration_ms = 120
curve = "cubic-bezier(0.2, 0, 0, 1)"
fade_curve = "spring 0.7"
"##,
        )
        .unwrap();
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(t.palette.bg, Rgba::rgb(0, 0, 0));
        assert_eq!(t.palette.accent, Rgba::new(255, 136, 0, 204));
        assert_eq!(t.palette.fg, Theme::default().palette.fg);
        assert_eq!(t.fonts.family, "Inter");
        assert_eq!(t.fonts.size, 12.0);
        assert_eq!(t.fonts.mono_size, 9.5);
        assert_eq!(
            (t.shape.radius, t.shape.bar_height, t.shape.gap),
            (0, 40, 8)
        );
        assert_eq!(t.motion.duration_ms, 120);
        assert_eq!(t.motion.fade_curve, "spring 0.7");
    }

    #[test]
    fn bad_values_drop_alone_with_warnings() {
        let (t, w) = Theme::from_toml_str(
            r##"
stray = 1
[palette]
bg = "red"
fg = 5
nope = "#000000"
accent = "#00ff00"
[fonts]
size = 0
family = ""
[shape]
radius = -3
gap = "wide"
[motion]
curve = "wobble"
duration_ms = 99999
[unknown_section]
"##,
        )
        .unwrap();
        let d = Theme::default();
        assert_eq!(t.palette.bg, d.palette.bg);
        assert_eq!(t.palette.fg, d.palette.fg);
        assert_eq!(t.palette.accent, Rgba::rgb(0, 255, 0));
        assert_eq!(t.fonts, d.fonts);
        assert_eq!(t.shape, d.shape);
        assert_eq!(t.motion, d.motion);
        for needle in [
            "unknown key \"stray\"",
            "unknown key \"unknown_section\"",
            "palette.bg",
            "palette.fg: expected string",
            "unknown key \"nope\"",
            "fonts.size",
            "fonts.family",
            "shape.radius",
            "shape.gap",
            "motion.curve",
            "motion.duration_ms",
        ] {
            assert!(w.iter().any(|m| m.contains(needle)), "{needle}: {w:?}");
        }
        assert_eq!(w.len(), 11, "{w:?}");
    }

    #[test]
    fn wrong_section_type_warns() {
        let (t, w) = Theme::from_toml_str("palette = 3\n").unwrap();
        assert_eq!(t, Theme::default());
        assert_eq!(w.len(), 1, "{w:?}");
    }

    #[test]
    fn syntax_error_is_err_and_missing_file_is_defaults() {
        assert!(Theme::from_toml_str("[palette\nbg=").is_err());
        let path = std::env::temp_dir().join("aurora-theme-test-definitely-missing.toml");
        let (t, w) = Theme::load(&path).unwrap();
        assert_eq!(t, Theme::default());
        assert!(w.is_empty());
    }
}
