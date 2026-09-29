//! User configuration: `~/.config/aurora/config.toml`.
//!
//! `raw` reads the file loosely, `Config::resolve` turns it into validated settings plus
//! warnings. The ladder: a missing file gives defaults, a TOML syntax error keeps the
//! previous config (defaults at first start), a bad value or list item is dropped alone,
//! an unknown key is only a warning. Nothing here can fail startup or a reload.
#![allow(dead_code)] // most settings are consumed by later M2 steps

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::state::Aurora;
use keybind::{BindTable, Mods};

pub mod keybind;
mod raw;

/// Also the source of the built-in default binds.
pub const EXAMPLE: &str = include_str!("../../../../config/aurora.example.toml");

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub [f32; 4]);

impl Color {
    /// `#rrggbb` or `#rrggbbaa`.
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix('#')?;
        if !(hex.len() == 6 || hex.len() == 8) || !hex.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        let a = if hex.len() == 8 { byte(6)? } else { 255 };
        Some(Self([
            byte(0)? as f32 / 255.0,
            byte(2)? as f32 / 255.0,
            byte(4)? as f32 / 255.0,
            a as f32 / 255.0,
        ]))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModKey {
    Super,
    Ctrl,
    Shift,
    Alt,
    Altgr,
}

impl ModKey {
    fn parse(text: &str) -> Option<Self> {
        Some(match text.to_ascii_lowercase().as_str() {
            "super" | "logo" => Self::Super,
            "ctrl" | "control" => Self::Ctrl,
            "shift" => Self::Shift,
            "alt" => Self::Alt,
            "altgr" => Self::Altgr,
            _ => return None,
        })
    }

    pub fn mods(self) -> Mods {
        match self {
            Self::Super => Mods::LOGO,
            Self::Ctrl => Mods::CTRL,
            Self::Shift => Mods::SHIFT,
            Self::Alt => Mods::ALT,
            Self::Altgr => Mods::ALTGR,
        }
    }
}

#[derive(Clone, Debug)]
pub struct General {
    pub gaps_in: i32,
    pub gaps_out: i32,
    pub border_width: i32,
    pub border_focused: Color,
    pub border_unfocused: Color,
    pub focus_follows_mouse: bool,
    /// Moving a window to another workspace also switches the view there.
    pub move_follows: bool,
    pub workspaces: u32,
    pub mod_key: ModKey,
    pub allow_virtual_keyboard: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            gaps_in: 1,
            gaps_out: 5,
            border_width: 2,
            border_focused: Color([
                0x89 as f32 / 255.0,
                0xb4 as f32 / 255.0,
                0xfa as f32 / 255.0,
                1.0,
            ]),
            border_unfocused: Color([
                0x45 as f32 / 255.0,
                0x47 as f32 / 255.0,
                0x5a as f32 / 255.0,
                1.0,
            ]),
            focus_follows_mouse: true,
            move_follows: true,
            workspaces: 10,
            mod_key: ModKey::Super,
            allow_virtual_keyboard: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModeSpec {
    pub width: i32,
    pub height: i32,
    /// In millihertz, `None` picks the mode's preferred refresh.
    pub refresh_mhz: Option<u32>,
}

impl ModeSpec {
    /// `WxH` or `WxH@Hz` (Hz may be fractional).
    pub fn parse(text: &str) -> Option<Self> {
        let (size, hz) = match text.split_once('@') {
            Some((size, hz)) => (size, Some(hz)),
            None => (text, None),
        };
        let (w, h) = size.split_once('x')?;
        let (width, height): (i32, i32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
        let refresh_mhz = match hz {
            Some(hz) => {
                let hz: f64 = hz.trim().trim_end_matches("Hz").parse().ok()?;
                if !(1.0..=1000.0).contains(&hz) {
                    return None;
                }
                Some((hz * 1000.0).round() as u32)
            }
            None => None,
        };
        (width > 0 && height > 0).then_some(Self {
            width,
            height,
            refresh_mhz,
        })
    }
}

#[derive(Clone, Debug)]
pub struct OutputRule {
    pub name: String,
    pub enabled: bool,
    pub primary: bool,
    pub position: Option<(i32, i32)>,
    pub mode: Option<ModeSpec>,
    pub scale: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceRule {
    pub id: u32,
    pub output: Option<String>,
    pub default: bool,
}

/// Exact string, or a pattern where `*` matches any run of characters.
#[derive(Clone, Debug)]
pub struct Glob(String);

impl Glob {
    pub fn new(pattern: &str) -> Self {
        Self(pattern.to_string())
    }

    pub fn is_match(&self, text: &str) -> bool {
        let mut parts = self.0.split('*');
        let Some(first) = parts.next() else {
            return false;
        };
        let Some(mut rest) = text.strip_prefix(first) else {
            return false;
        };
        let mut parts = parts.peekable();
        if parts.peek().is_none() {
            return rest.is_empty();
        }
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                return rest.ends_with(part);
            }
            match rest.find(part) {
                Some(i) => rest = &rest[i + part.len()..],
                None => return false,
            }
        }
        true
    }
}

#[derive(Clone, Debug)]
pub struct WindowRule {
    pub app_id: Option<Glob>,
    pub title: Option<Glob>,
    /// X11 WM_CLASS.
    pub class: Option<Glob>,
    pub floating: Option<bool>,
    pub workspace: Option<u32>,
    pub output: Option<String>,
    pub size: Option<(i32, i32)>,
    pub fullscreen: Option<bool>,
}

#[derive(Debug, Default)]
pub struct Config {
    pub general: General,
    pub binds: BindTable,
    pub outputs: Vec<OutputRule>,
    pub workspace_rules: Vec<WorkspaceRule>,
    pub window_rules: Vec<WindowRule>,
    /// Commands run once per compositor process, after the socket exists.
    pub autostart: Vec<String>,
    /// False when the file did not exist and everything is defaults.
    pub from_file: bool,
}

impl Config {
    pub fn resolve(raw: raw::RawConfig) -> (Self, Vec<String>) {
        let mut w = Vec::new();
        for key in raw.extra.keys() {
            w.push(format!("unknown section {key:?}"));
        }
        let general = raw::general(raw.general.as_ref(), &mut w);
        let binds = BindTable::build(
            general.mod_key.mods(),
            general.workspaces,
            raw.keybinds.as_ref(),
            raw.mousebinds.as_ref(),
            &mut w,
        );
        let config = Self {
            binds,
            outputs: raw::outputs(raw.output.as_ref(), &mut w),
            workspace_rules: raw::workspace_rules(
                raw.workspace.as_ref(),
                general.workspaces,
                &mut w,
            ),
            window_rules: raw::window_rules(raw.window_rule.as_ref(), general.workspaces, &mut w),
            autostart: raw::autostart(raw.autostart.as_ref(), &mut w),
            general,
            from_file: true,
        };
        (config, w)
    }

    /// `Err` only for an unreadable file or a TOML syntax error, with `line:col` when known.
    pub fn load(path: &Path) -> Result<(Self, Vec<String>), String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let (mut config, warnings) = Self::resolve(raw::RawConfig::default());
                config.from_file = false;
                return Ok((config, warnings));
            }
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };
        let raw = raw::parse(&text).map_err(|err| format!("{}:{err}", path.display()))?;
        Ok(Self::resolve(raw))
    }

    fn log_loaded(&self, path: &Path, warnings: &[String]) {
        let missing = if self.from_file {
            ""
        } else {
            " (no file, defaults)"
        };
        tracing::info!(
            "config: loaded path={} binds={} warnings={}{missing}",
            path.display(),
            self.binds.len(),
            warnings.len()
        );
        for warning in warnings {
            tracing::warn!("config: warning {warning}");
        }
    }

    /// Startup load: an error falls back to defaults.
    pub fn load_initial(path: &Path) -> Arc<Self> {
        match Self::load(path) {
            Ok((config, warnings)) => {
                config.log_loaded(path, &warnings);
                Arc::new(config)
            }
            Err(err) => {
                tracing::warn!("config: error {err} using defaults");
                let (config, _) = Self::resolve(raw::RawConfig::default());
                Arc::new(Self {
                    from_file: false,
                    ..config
                })
            }
        }
    }
}

/// `$XDG_CONFIG_HOME/aurora`, falling back to `~/.config/aurora`.
pub fn config_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("aurora"))
}

/// `--config`, else `AURORA_CONFIG`, else `<config dir>/config.toml`.
pub fn resolve_path(cli: Option<PathBuf>) -> PathBuf {
    cli.or_else(|| {
        std::env::var_os("AURORA_CONFIG")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
    .or_else(|| config_dir().map(|d| d.join("config.toml")))
    .unwrap_or_else(|| PathBuf::from("aurora.toml"))
}

impl Aurora {
    /// Never fails: an unreadable or unparsable file keeps the previous config.
    pub fn reload_config(&mut self) {
        match Config::load(&self.config_path) {
            Ok((config, warnings)) => {
                config.log_loaded(&self.config_path, &warnings);
                self.protocols
                    .virtual_keyboard
                    .set_allowed(config.general.allow_virtual_keyboard);
                self.config = Arc::new(config);
            }
            Err(err) => tracing::warn!("config: error {err} keeping previous"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(text: &str) -> (Config, Vec<String>) {
        Config::resolve(raw::parse(text).expect("test toml parses"))
    }

    #[test]
    fn shipped_example_parses_cleanly() {
        let (config, warnings) = resolve(EXAMPLE);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(config.binds.len() > 40);
        let names: Vec<_> = config.outputs.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["HDMI-A-1", "DP-3", "DP-1"]);
        assert_eq!(
            config.outputs[1].mode.map(|m| m.refresh_mhz),
            Some(Some(200_000))
        );
    }

    #[test]
    fn empty_config_is_defaults_with_default_binds() {
        let (config, warnings) = resolve("");
        assert!(warnings.is_empty());
        assert_eq!(config.general.workspaces, 10);
        assert!(config.binds.len() > 40);
    }

    #[test]
    fn bad_items_are_dropped_and_unknown_keys_warned() {
        let (config, warnings) = resolve(
            r#"
            surprise = 1
            [general]
            gaps_in = "wide"
            gaps_out = 12
            colour = "red"
            [[output]]
            name = "A-1"
            scale = 2
            [[output]]
            name = "B-1"
            mode = "banana"
            [[output]]
            scale = 1
            [[output]]
            name = "A-1"
            [[window_rule]]
            floating = true
            [[window_rule]]
            app_id = "pavucontrol"
            floating = true
            "#,
        );
        assert_eq!(config.general.gaps_in, 1);
        assert_eq!(config.general.gaps_out, 12);
        assert_eq!(config.outputs.len(), 1);
        assert_eq!(config.outputs[0].scale, Some(2.0));
        assert_eq!(config.window_rules.len(), 1);
        for needle in [
            "surprise",
            "gaps_in",
            "colour",
            "mode",
            "missing name",
            "duplicate",
            "at least one",
        ] {
            assert!(
                warnings.iter().any(|w| w.contains(needle)),
                "{needle}: {warnings:?}"
            );
        }
    }

    #[test]
    fn syntax_error_reports_line_and_column() {
        let err = raw::parse("[general]\ngaps_in = = 3\n").unwrap_err();
        assert!(err.starts_with("2:"), "{err}");
    }

    #[test]
    fn glob_matching() {
        let g = |p| Glob::new(p);
        assert!(g("firefox").is_match("firefox"));
        assert!(!g("firefox").is_match("firefox2"));
        assert!(g("*").is_match(""));
        assert!(g("*dialog").is_match("file-dialog"));
        assert!(g("steam_app_*").is_match("steam_app_252950"));
        assert!(g("a*b*c").is_match("axxbyyc"));
        assert!(!g("a*b*c").is_match("axxcyyb"));
        assert!(!g("a*b").is_match("a"));
    }

    #[test]
    fn mode_and_color_parsing() {
        assert_eq!(
            ModeSpec::parse("1920x1080@59.94").map(|m| m.refresh_mhz),
            Some(Some(59_940))
        );
        assert_eq!(
            ModeSpec::parse("3840x1080").map(|m| m.refresh_mhz),
            Some(None)
        );
        assert_eq!(ModeSpec::parse("0x1080"), None);
        assert_eq!(
            Color::parse("#ff000080").map(|c| c.0[3] > 0.49 && c.0[3] < 0.51),
            Some(true)
        );
        assert_eq!(Color::parse("ff0000"), None);
    }
}
