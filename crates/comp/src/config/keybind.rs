//! Chords, triggers and the bind table. Pure: no Smithay state, only xkb name lookups.

use std::collections::{HashMap, HashSet};
use std::ops::BitOr;

use smithay::input::keyboard::{
    ModifiersState,
    xkb::{self, KEYSYM_CASE_INSENSITIVE, KEYSYM_NO_FLAGS},
};
use toml::{Table, Value};

use crate::action::Action;
use crate::emergency;

/// Modifier bitset. Lock modifiers (caps, num) are deliberately not representable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1);
    pub const CTRL: Self = Self(2);
    pub const ALT: Self = Self(4);
    pub const LOGO: Self = Self(8);
    /// ISO_Level3_Shift: its own bit, so `Super+AltGr+q` never matches `Super+q`.
    pub const ALTGR: Self = Self(16);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn from_state(state: &ModifiersState) -> Self {
        let mut mods = Self::NONE;
        for (set, bit) in [
            (state.shift, Self::SHIFT),
            (state.ctrl, Self::CTRL),
            (state.alt, Self::ALT),
            (state.logo, Self::LOGO),
            (state.iso_level3_shift, Self::ALTGR),
        ] {
            if set {
                mods = mods | bit;
            }
        }
        mods
    }
}

impl BitOr for Mods {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WheelDir {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// Raw level-0 latin keysym (lower case for letters).
    Key(u32),
    /// evdev button code (BTN_LEFT = 272).
    Button(u32),
    Wheel(WheelDir),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub mods: Mods,
    pub trigger: Trigger,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    pub action: Action,
    pub repeat: bool,
    /// Fires even while a client holds a keyboard-shortcuts inhibitor.
    pub bypass_inhibit: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Key,
    Mouse,
}

#[derive(Debug, Default)]
pub struct BindTable {
    binds: HashMap<Chord, Bind>,
}

impl BindTable {
    pub fn len(&self) -> usize {
        self.binds.len()
    }

    pub fn get(&self, chord: &Chord) -> Option<&Bind> {
        self.binds.get(chord)
    }

    /// Built-in defaults (the shipped example config) with the user's tables merged over
    /// them: same chord replaces, `none` removes. Problems become warnings, never errors.
    pub fn build(
        mod_key: Mods,
        max_ws: u32,
        keys: Option<&Value>,
        mouse: Option<&Value>,
        warnings: &mut Vec<String>,
    ) -> Self {
        let mut binds = HashMap::new();

        match toml::from_str::<Table>(super::EXAMPLE) {
            Ok(defaults) => {
                // A default the user's settings make invalid (workspace 9 with 4 workspaces)
                // is silently dropped; the example test pins that the defaults are clean.
                let mut ignored = Vec::new();
                for (section, kind) in [("keybinds", Kind::Key), ("mousebinds", Kind::Mouse)] {
                    if let Some(table) = defaults.get(section).and_then(Value::as_table) {
                        apply(
                            &mut binds,
                            section,
                            table,
                            kind,
                            mod_key,
                            max_ws,
                            &mut ignored,
                        );
                    }
                }
            }
            Err(err) => tracing::error!(%err, "built-in default binds do not parse"),
        }

        for (section, value, kind) in [
            ("keybinds", keys, Kind::Key),
            ("mousebinds", mouse, Kind::Mouse),
        ] {
            let Some(value) = value else { continue };
            match value.as_table() {
                Some(table) => apply(&mut binds, section, table, kind, mod_key, max_ws, warnings),
                None => warnings.push(format!(
                    "{section}: expected a table, got {}",
                    value.type_str()
                )),
            }
        }
        Self { binds }
    }
}

fn apply(
    binds: &mut HashMap<Chord, Bind>,
    section: &str,
    table: &Table,
    kind: Kind,
    mod_key: Mods,
    max_ws: u32,
    warnings: &mut Vec<String>,
) {
    let mut seen = HashSet::new();
    for (name, value) in table {
        let ctx = format!("{section}.\"{name}\"");
        let chord = match parse_chord(name, kind, mod_key) {
            Ok((chord, note)) => {
                warnings.extend(note.map(|n| format!("{ctx}: {n}")));
                chord
            }
            Err(err) => {
                warnings.push(format!("{ctx}: {err}"));
                continue;
            }
        };
        let bind = match parse_bind(value, max_ws, &ctx, warnings) {
            Ok(bind) => bind,
            Err(err) => {
                warnings.push(format!("{ctx}: {err}"));
                continue;
            }
        };
        if emergency::is_reserved(&chord) {
            warnings.push(format!("{ctx}: reserved emergency chord, ignored"));
            continue;
        }
        if !seen.insert(chord) {
            warnings.push(format!("{ctx}: duplicate of an earlier bind, ignored"));
            continue;
        }
        if bind.action == Action::None {
            binds.remove(&chord);
        } else {
            binds.insert(chord, bind);
        }
    }
}

fn parse_bind(
    value: &Value,
    max_ws: u32,
    ctx: &str,
    warnings: &mut Vec<String>,
) -> Result<Bind, String> {
    match value {
        Value::String(text) => Ok(Bind {
            action: Action::parse(text, max_ws)?,
            repeat: false,
            bypass_inhibit: false,
        }),
        Value::Table(table) => {
            super::raw::check_keys(
                ctx,
                table,
                &["action", "repeat", "bypass_inhibit"],
                warnings,
            );
            let action = super::raw::get_str(ctx, table, "action")?.ok_or("missing action")?;
            Ok(Bind {
                action: Action::parse(action, max_ws)?,
                repeat: super::raw::get_bool(ctx, table, "repeat")?.unwrap_or(false),
                bypass_inhibit: super::raw::get_bool(ctx, table, "bypass_inhibit")?
                    .unwrap_or(false),
            })
        }
        other => Err(format!(
            "expected a string or table, got {}",
            other.type_str()
        )),
    }
}

fn mod_token(token: &str, mod_key: Mods) -> Option<Mods> {
    Some(match token.to_ascii_lowercase().as_str() {
        "mod" => mod_key,
        "super" | "logo" | "win" | "mod4" => Mods::LOGO,
        "ctrl" | "control" => Mods::CTRL,
        "shift" => Mods::SHIFT,
        "alt" | "mod1" => Mods::ALT,
        "altgr" | "mod5" | "iso_level3_shift" => Mods::ALTGR,
        _ => return None,
    })
}

/// Returns the chord plus an optional non-fatal note.
fn parse_chord(text: &str, kind: Kind, mod_key: Mods) -> Result<(Chord, Option<String>), String> {
    let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let key = parts.pop().filter(|k| !k.is_empty()).ok_or("empty key")?;
    let mut mods = Mods::NONE;
    for part in parts {
        mods =
            mods | mod_token(part, mod_key).ok_or_else(|| format!("unknown modifier {part:?}"))?;
    }
    let (trigger, note) = match kind {
        Kind::Key => {
            let (sym, note) = parse_keysym(key).ok_or_else(|| format!("unknown key {key:?}"))?;
            (Trigger::Key(sym), note)
        }
        Kind::Mouse => (
            parse_mouse(key).ok_or_else(|| format!("unknown mouse button {key:?}"))?,
            None,
        ),
    };
    Ok((Chord { mods, trigger }, note))
}

/// Level-0 keysym for a key name; letters fold to lower case so `Super+Q` is `Super+q`.
fn parse_keysym(name: &str) -> Option<(u32, Option<String>)> {
    // xkbcommon takes a C string.
    if name.contains('\0') {
        return None;
    }
    let lookup = |n: &str, flags| {
        let sym = xkb::keysym_from_name(n, flags).raw();
        (sym != 0).then_some(sym)
    };
    let lower = name.to_ascii_lowercase();
    let raw = if name.len() == 1 {
        lookup(&lower, KEYSYM_NO_FLAGS).or_else(|| lookup(name, KEYSYM_NO_FLAGS))
    } else {
        lookup(name, KEYSYM_NO_FLAGS)
            .or_else(|| lookup(&lower, KEYSYM_NO_FLAGS))
            .or_else(|| lookup(name, KEYSYM_CASE_INSENSITIVE))
    }?;
    let raw = if (0x41..=0x5a).contains(&raw) {
        raw + 0x20
    } else {
        raw
    };

    let shifted = u8::try_from(raw)
        .is_ok_and(|b| b.is_ascii_punctuation() && "!@#$%^&*()_+{}|:\"<>?~".contains(b as char));
    let note = shifted.then(|| format!("{name:?} is a shifted symbol, write Shift+<key> instead"));
    Some((raw, note))
}

fn parse_mouse(name: &str) -> Option<Trigger> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "left" | "btn_left" => Trigger::Button(0x110),
        "right" | "btn_right" => Trigger::Button(0x111),
        "middle" | "btn_middle" => Trigger::Button(0x112),
        "side" | "btn_side" => Trigger::Button(0x113),
        "extra" | "btn_extra" => Trigger::Button(0x114),
        "wheel_up" => Trigger::Wheel(WheelDir::Up),
        "wheel_down" => Trigger::Wheel(WheelDir::Down),
        other => Trigger::Button(other.strip_prefix("mouse:")?.parse().ok()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::input::keyboard::keysyms;

    fn chord(text: &str) -> Result<Chord, String> {
        parse_chord(text, Kind::Key, Mods::LOGO).map(|(c, _)| c)
    }

    fn key(mods: Mods, sym: u32) -> Chord {
        Chord {
            mods,
            trigger: Trigger::Key(sym),
        }
    }

    #[test]
    fn chord_table() {
        assert_eq!(
            chord("Super+Shift+1"),
            Ok(key(Mods::LOGO | Mods::SHIFT, keysyms::KEY_1))
        );
        assert_eq!(
            chord("Mod+AltGr+w"),
            Ok(key(Mods::LOGO | Mods::ALTGR, keysyms::KEY_w))
        );
        assert_eq!(chord("Super+Q"), chord("Super+q"));
        assert_eq!(
            chord("Super+comma"),
            Ok(key(Mods::LOGO, keysyms::KEY_comma))
        );
        assert_eq!(chord("Super+Left"), Ok(key(Mods::LOGO, keysyms::KEY_Left)));
        assert_ne!(chord("Super+AltGr+q"), chord("Super+q"));
        assert!(chord("Hyper+q").is_err());
        assert!(chord("Super+").is_err());
        assert!(chord("Super+nosuchkey").is_err());
        let (_, note) = parse_chord("Super+exclam", Kind::Key, Mods::LOGO).unwrap();
        assert!(note.is_some());
    }

    #[test]
    fn mouse_names() {
        let m = |s| parse_chord(s, Kind::Mouse, Mods::LOGO).map(|(c, _)| c.trigger);
        assert_eq!(m("Super+left"), Ok(Trigger::Button(272)));
        assert_eq!(m("Super+BTN_RIGHT"), Ok(Trigger::Button(273)));
        assert_eq!(m("Super+mouse:274"), Ok(Trigger::Button(274)));
        assert_eq!(m("Super+wheel_up"), Ok(Trigger::Wheel(WheelDir::Up)));
        assert!(m("Super+banana").is_err());
    }

    fn table(text: &str) -> Value {
        Value::Table(toml::from_str(text).unwrap())
    }

    #[test]
    fn user_binds_merge_over_defaults() {
        let mut w = Vec::new();
        let base = BindTable::build(Mods::LOGO, 10, None, None, &mut w);
        assert!(w.is_empty(), "{w:?}");
        let quit = key(Mods::LOGO, keysyms::KEY_m);
        assert_eq!(base.get(&quit).map(|b| &b.action), Some(&Action::Quit));

        let user = table(
            r#"
            "Super+m" = "none"
            "Super+F5" = { action = "spawn foo", repeat = true }
            "Ctrl+Alt+BackSpace" = "spawn evil"
            "Ctrl+Alt+F1" = "spawn evil"
            "Super+9" = "workspace 11"
            "#,
        );
        let table = BindTable::build(Mods::LOGO, 10, Some(&user), None, &mut w);
        assert_eq!(table.get(&quit), None);
        let f5 = table.get(&key(Mods::LOGO, keysyms::KEY_F5)).unwrap();
        assert!(f5.repeat && !f5.bypass_inhibit);
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(
            w.iter()
                .any(|m| m.contains("Ctrl+Alt+BackSpace") && m.contains("reserved"))
        );
        assert!(
            w.iter()
                .any(|m| m.contains("Ctrl+Alt+F1") && m.contains("reserved"))
        );
    }
}
