//! `[input.keyboard]`, `[input.pointer]` and `[input.touchpad]`.

use toml::{Table, Value};

use super::raw::{check_keys, get_bool, get_float, get_str, ranged, soft, table_of};

/// XKB names and key repeat. Every XKB field left out stays `None`; when all of them are,
/// the keymap comes from the older sources (keymap.xkb, XKB_DEFAULT_*, xorg.conf.d).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyboard {
    pub rules: Option<String>,
    pub model: Option<String>,
    pub layout: Option<String>,
    pub variant: Option<String>,
    pub options: Option<String>,
    /// Keys per second, 0 turns repeat off.
    pub repeat_rate: i32,
    /// Milliseconds before a held key repeats.
    pub repeat_delay: i32,
}

impl Keyboard {
    /// True when the file names any XKB field, which then wins over the older sources.
    pub fn has_xkb(&self) -> bool {
        self.xkb().iter().any(|f| f.is_some())
    }

    /// Only the XKB part, for telling whether a reload has to swap the keymap.
    pub fn xkb(&self) -> [&Option<String>; 5] {
        [
            &self.rules,
            &self.model,
            &self.layout,
            &self.variant,
            &self.options,
        ]
    }
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            rules: None,
            model: None,
            layout: None,
            variant: None,
            options: None,
            repeat_rate: 40,
            repeat_delay: 250,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccelProfile {
    Flat,
    Adaptive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollMethod {
    None,
    TwoFinger,
    Edge,
    OnButtonDown,
}

/// libinput settings for one device class. A device that lacks a feature ignores it.
#[derive(Clone, Debug, PartialEq)]
pub struct Pointer {
    pub accel_profile: AccelProfile,
    /// -1..=1, 0 is libinput's default.
    pub accel_speed: f64,
    pub natural_scroll: bool,
    pub left_handed: bool,
    /// `None` keeps the device's own default.
    pub scroll_method: Option<ScrollMethod>,
    /// Touchpads only.
    pub tap: bool,
    /// Disable while typing, touchpads only.
    pub dwt: bool,
}

impl Pointer {
    fn mouse() -> Self {
        Self {
            accel_profile: AccelProfile::Adaptive,
            accel_speed: 0.0,
            natural_scroll: false,
            left_handed: false,
            scroll_method: None,
            tap: false,
            dwt: false,
        }
    }

    fn touchpad() -> Self {
        Self {
            tap: true,
            dwt: true,
            ..Self::mouse()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Input {
    pub keyboard: Keyboard,
    /// Mice, trackballs, trackpoints: every pointer that is not a touchpad.
    pub pointer: Pointer,
    pub touchpad: Pointer,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            keyboard: Keyboard::default(),
            pointer: Pointer::mouse(),
            touchpad: Pointer::touchpad(),
        }
    }
}

pub fn input(section: Option<&Value>, warnings: &mut Vec<String>) -> Input {
    let mut input = Input::default();
    let Some(table) = table_of("input", section, warnings) else {
        return input;
    };
    check_keys(
        "input",
        table,
        &["keyboard", "pointer", "touchpad"],
        warnings,
    );
    if let Some(t) = table_of("input.keyboard", table.get("keyboard"), warnings) {
        keyboard(t, &mut input.keyboard, warnings);
    }
    if let Some(t) = table_of("input.pointer", table.get("pointer"), warnings) {
        pointer("input.pointer", t, false, &mut input.pointer, warnings);
    }
    if let Some(t) = table_of("input.touchpad", table.get("touchpad"), warnings) {
        pointer("input.touchpad", t, true, &mut input.touchpad, warnings);
    }
    input
}

fn keyboard(t: &Table, k: &mut Keyboard, warnings: &mut Vec<String>) {
    let ctx = "input.keyboard";
    check_keys(
        ctx,
        t,
        &[
            "rules",
            "model",
            "layout",
            "variant",
            "options",
            "repeat_rate",
            "repeat_delay",
        ],
        warnings,
    );
    for (key, slot) in [
        ("rules", &mut k.rules),
        ("model", &mut k.model),
        ("layout", &mut k.layout),
        ("variant", &mut k.variant),
        ("options", &mut k.options),
    ] {
        *slot = soft(get_str(ctx, t, key), warnings).map(|s| s.trim().to_string());
    }
    if let Some(n) = ranged(ctx, "repeat_rate", t, 0..=1000, warnings) {
        k.repeat_rate = n as i32;
    }
    if let Some(n) = ranged(ctx, "repeat_delay", t, 1..=10_000, warnings) {
        k.repeat_delay = n as i32;
    }
}

fn pointer(ctx: &str, t: &Table, touchpad: bool, p: &mut Pointer, warnings: &mut Vec<String>) {
    let mut known = vec![
        "accel_profile",
        "accel_speed",
        "natural_scroll",
        "left_handed",
        "scroll_method",
    ];
    if touchpad {
        known.extend(["tap", "dwt"]);
    }
    check_keys(ctx, t, &known, warnings);
    if let Some(text) = soft(get_str(ctx, t, "accel_profile"), warnings) {
        match text {
            "flat" => p.accel_profile = AccelProfile::Flat,
            "adaptive" => p.accel_profile = AccelProfile::Adaptive,
            _ => warnings.push(format!(
                "{ctx}.accel_profile: unknown {text:?} (flat, adaptive)"
            )),
        }
    }
    if let Some(f) = soft(get_float(ctx, t, "accel_speed"), warnings) {
        if (-1.0..=1.0).contains(&f) {
            p.accel_speed = f;
        } else {
            warnings.push(format!("{ctx}.accel_speed: {f} is out of range (-1..=1)"));
        }
    }
    if let Some(text) = soft(get_str(ctx, t, "scroll_method"), warnings) {
        let method = match text {
            "none" => Some(ScrollMethod::None),
            "two-finger" => Some(ScrollMethod::TwoFinger),
            "edge" => Some(ScrollMethod::Edge),
            "on-button-down" => Some(ScrollMethod::OnButtonDown),
            _ => None,
        };
        match method {
            Some(_) => p.scroll_method = method,
            None => warnings.push(format!(
                "{ctx}.scroll_method: unknown {text:?} (none, two-finger, edge, on-button-down)"
            )),
        }
    }
    let mut flags = vec![
        ("natural_scroll", &mut p.natural_scroll),
        ("left_handed", &mut p.left_handed),
    ];
    if touchpad {
        flags.extend([("tap", &mut p.tap), ("dwt", &mut p.dwt)]);
    }
    for (key, slot) in flags {
        if let Some(b) = soft(get_bool(ctx, t, key), warnings) {
            *slot = b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Input, Vec<String>) {
        let raw = super::super::raw::parse(text).expect("test toml parses");
        let mut warnings = Vec::new();
        (input(raw.input.as_ref(), &mut warnings), warnings)
    }

    #[test]
    fn empty_is_the_old_hardcoded_behaviour() {
        let (i, w) = parse("");
        assert!(w.is_empty());
        assert!(!i.keyboard.has_xkb());
        assert_eq!((i.keyboard.repeat_rate, i.keyboard.repeat_delay), (40, 250));
        assert_eq!(i.pointer.accel_profile, AccelProfile::Adaptive);
        assert!(!i.pointer.tap && !i.pointer.natural_scroll);
        assert!(i.touchpad.tap && i.touchpad.dwt && !i.touchpad.natural_scroll);
    }

    #[test]
    fn full_sections_parse() {
        let (i, w) = parse(
            r#"
            [input.keyboard]
            layout = "us,no"
            variant = "altgr-intl,"
            options = "grp:alt_shift_toggle,caps:escape"
            repeat_rate = 30
            repeat_delay = 300
            [input.pointer]
            accel_profile = "flat"
            accel_speed = -0.5
            left_handed = true
            [input.touchpad]
            natural_scroll = true
            tap = false
            scroll_method = "edge"
            "#,
        );
        assert!(w.is_empty(), "{w:?}");
        let k = &i.keyboard;
        assert!(k.has_xkb());
        assert_eq!(k.layout.as_deref(), Some("us,no"));
        assert_eq!(k.variant.as_deref(), Some("altgr-intl,"));
        assert_eq!(k.model, None);
        assert_eq!((k.repeat_rate, k.repeat_delay), (30, 300));
        assert_eq!(i.pointer.accel_profile, AccelProfile::Flat);
        assert_eq!(i.pointer.accel_speed, -0.5);
        assert!(i.pointer.left_handed);
        assert!(i.touchpad.natural_scroll && !i.touchpad.tap && i.touchpad.dwt);
        assert_eq!(i.touchpad.scroll_method, Some(ScrollMethod::Edge));
        assert_eq!(i.pointer.scroll_method, None);
    }

    #[test]
    fn bad_values_fall_back_per_key() {
        let (i, w) = parse(
            r#"
            [input]
            mouse = {}
            [input.keyboard]
            layout = 5
            repeat_rate = 5000
            repeat_delay = 0
            [input.pointer]
            accel_profile = "fast"
            accel_speed = 3
            tap = true
            scroll_method = "wheel"
            natural_scroll = "yes"
            "#,
        );
        assert_eq!(i, Input::default());
        for needle in [
            "mouse",
            "layout",
            "repeat_rate",
            "repeat_delay",
            "accel_profile",
            "accel_speed",
            "input.pointer: unknown key \"tap\"",
            "scroll_method",
            "natural_scroll",
        ] {
            assert!(w.iter().any(|w| w.contains(needle)), "{needle}: {w:?}");
        }
    }

    #[test]
    fn wrong_section_types_are_reported() {
        let (i, w) = parse("[input]\nkeyboard = \"us\"\n");
        assert_eq!(i, Input::default());
        assert!(w.iter().any(|w| w.contains("input.keyboard")), "{w:?}");
    }
}
