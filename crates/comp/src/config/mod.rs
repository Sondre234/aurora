//! User configuration: `~/.config/aurora/config.toml`.
//!
//! `raw` reads the file loosely, `Config::resolve` turns it into validated settings plus
//! warnings. The ladder: a missing file gives defaults, a TOML syntax error keeps the
//! previous config (defaults at first start), a bad value or list item is dropped alone,
//! an unknown key is only a warning. Nothing here can fail startup or a reload.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::anim::Curve;
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
    /// xdg-activation requests focus the window instead of only marking it urgent.
    pub focus_on_activate: bool,
    /// Fullscreen surfaces asking for async presentation (wp_tearing_control) may tear.
    pub allow_tearing: bool,
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
            focus_on_activate: false,
            allow_tearing: false,
        }
    }
}

/// What an animation of one kind does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimSpec {
    pub enabled: bool,
    pub duration_ms: u32,
    pub curve: Curve,
}

impl AnimSpec {
    pub fn duration(&self) -> Duration {
        Duration::from_millis(u64::from(self.duration_ms))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnimKind {
    WindowMove,
    WindowOpen,
    WindowClose,
    Workspace,
    Fade,
}

/// `[animations]`. Each kind starts from the section-wide duration and curve.
#[derive(Clone, Debug)]
pub struct Animations {
    pub enabled: bool,
    pub duration_ms: u32,
    pub curve: Curve,
    pub window_move: AnimSpec,
    pub window_open: AnimSpec,
    pub window_close: AnimSpec,
    pub workspace: AnimSpec,
    pub fade: AnimSpec,
}

impl Default for Animations {
    fn default() -> Self {
        let spec = AnimSpec {
            enabled: true,
            duration_ms: 200,
            curve: Curve::EASE_OUT,
        };
        Self {
            enabled: true,
            duration_ms: spec.duration_ms,
            curve: spec.curve,
            window_move: spec,
            window_open: spec,
            window_close: spec,
            workspace: spec,
            fade: spec,
        }
    }
}

impl Animations {
    /// The settings for `kind`, or `None` when animations are off, the kind is off or its
    /// duration is zero. Callers then snap instead of animating.
    pub fn spec(&self, kind: AnimKind) -> Option<AnimSpec> {
        if !self.enabled {
            return None;
        }
        let spec = match kind {
            AnimKind::WindowMove => self.window_move,
            AnimKind::WindowOpen => self.window_open,
            AnimKind::WindowClose => self.window_close,
            AnimKind::Workspace => self.workspace,
            AnimKind::Fade => self.fade,
        };
        (spec.enabled && spec.duration_ms > 0).then_some(spec)
    }
}

/// `[decoration]`: corners, shadows, blur and inactive dimming.
#[derive(Clone, Debug)]
pub struct Decoration {
    /// Corner radius in logical pixels, 0 for square corners.
    pub rounding: i32,
    pub shadow: bool,
    pub shadow_radius: i32,
    pub shadow_color: Color,
    pub blur: bool,
    pub blur_passes: u32,
    pub blur_radius: u32,
    /// Opacity of unfocused windows, 1.0 leaves them untouched.
    pub inactive_opacity: f32,
}

impl Default for Decoration {
    fn default() -> Self {
        Self {
            rounding: 10,
            shadow: true,
            shadow_radius: 20,
            shadow_color: Color([0.0, 0.0, 0.0, 115.0 / 255.0]),
            blur: true,
            blur_passes: 3,
            blur_radius: 6,
            inactive_opacity: 1.0,
        }
    }
}

/// `[xwayland]`: how X11 clients are scaled.
#[derive(Clone, Debug, PartialEq)]
pub struct XWayland {
    /// X pixels per logical pixel. 1 leaves X11 windows unscaled (the compositor resamples
    /// them on scaled outputs); the output's scale makes them render 1:1 on its pixels.
    pub scale: f64,
}

impl XWayland {
    pub const MIN_SCALE: f64 = 1.0;
    pub const MAX_SCALE: f64 = 4.0;

    /// The resolution to hand the X server so toolkits that read it grow with the scale, or
    /// `None` at scale 1 where the server keeps its own default.
    pub fn dpi(&self) -> Option<u32> {
        ((self.scale - 1.0).abs() > 1e-6).then(|| (96.0 * self.scale).round() as u32)
    }
}

impl Default for XWayland {
    fn default() -> Self {
        Self { scale: 1.0 }
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
        ((1..=16384).contains(&width) && (1..=16384).contains(&height)).then_some(Self {
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
    pub vrr: VrrMode,
}

/// `[[output]] vrr`: variable refresh rate (adaptive sync).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VrrMode {
    #[default]
    Off,
    On,
    /// Only while a fullscreen window is on the output.
    OnDemand,
}

impl VrrMode {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "off" => Self::Off,
            "on" => Self::On,
            "on-demand" => Self::OnDemand,
            _ => return None,
        })
    }
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

/// What the supervisor does when a service's process ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Never restarted.
    Never,
    /// Restarted after a non-zero exit or a signal, not after a clean exit.
    OnFailure,
    /// Restarted whatever the exit status.
    Always,
}

impl RestartPolicy {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "never" => Self::Never,
            "on-failure" => Self::OnFailure,
            "always" => Self::Always,
            _ => return None,
        })
    }
}

/// One `[services.<name>]` entry: a long-lived helper process (bar, launcher, notifier,
/// lock client) that the compositor starts and supervises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSpec {
    pub name: String,
    /// Run through `sh -c`, like autostart.
    pub command: String,
    pub enabled: bool,
    /// Started with the compositor. False means only an explicit request starts it (the
    /// `lock` action starts the service named `lock`).
    pub autostart: bool,
    pub restart: RestartPolicy,
    /// First restart delay; doubles per consecutive quick failure up to `max_backoff_ms`.
    pub backoff_ms: u32,
    pub max_backoff_ms: u32,
}

impl ServiceSpec {
    pub const MAX_BACKOFF_MS: u32 = 600_000;
}

#[derive(Debug, Default)]
pub struct Config {
    pub general: General,
    pub animations: Animations,
    pub decoration: Decoration,
    pub xwayland: XWayland,
    pub binds: BindTable,
    pub outputs: Vec<OutputRule>,
    pub workspace_rules: Vec<WorkspaceRule>,
    pub window_rules: Vec<WindowRule>,
    /// Commands run once per compositor process, after the socket exists.
    pub autostart: Vec<String>,
    /// `[services.<name>]`, supervised by `services.rs`, sorted by name.
    pub services: Vec<ServiceSpec>,
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
        let animations = raw::animations(raw.animations.as_ref(), &mut w);
        let decoration = raw::decoration(raw.decoration.as_ref(), &mut w);
        let xwayland = raw::xwayland(raw.xwayland.as_ref(), &mut w);
        let binds = BindTable::build(
            general.mod_key.mods(),
            general.workspaces,
            raw.keybinds.as_ref(),
            raw.mousebinds.as_ref(),
            &mut w,
        );
        let config = Self {
            animations,
            decoration,
            xwayland,
            binds,
            outputs: raw::outputs(raw.output.as_ref(), &mut w),
            workspace_rules: raw::workspace_rules(
                raw.workspace.as_ref(),
                general.workspaces,
                &mut w,
            ),
            window_rules: raw::window_rules(raw.window_rule.as_ref(), general.workspaces, &mut w),
            autostart: raw::autostart(raw.autostart.as_ref(), &mut w),
            services: raw::services(raw.services.as_ref(), &mut w),
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
                tracing::warn!(
                    "config: error {} using defaults",
                    err.lines().next().unwrap_or_default()
                );
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
        let _ = self.reload_config_checked();
    }

    /// Reloads the config and the theme, restarts nothing that runs fine, and tells IPC
    /// subscribers how it went. `Err` is the error that made it keep the previous config.
    pub fn reload_config_checked(&mut self) -> Result<(), String> {
        let (result, warnings) = match Config::load(&self.config_path) {
            Ok((config, warnings)) => {
                config.log_loaded(&self.config_path, &warnings);
                self.protocols
                    .virtual_keyboard
                    .set_allowed(config.general.allow_virtual_keyboard);
                self.config = Arc::new(config);
                self.apply_config();
                self.reapply_output_config();
                self.drm_apply_output_config();
                self.services_reload();
                (Ok(()), warnings)
            }
            Err(err) => {
                let first = err.lines().next().unwrap_or_default().to_string();
                tracing::warn!("config: error {first} keeping previous");
                (Err(first.clone()), vec![first])
            }
        };
        self.reload_theme();
        self.ipc_config_reloaded(result.is_ok(), warnings);
        result
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
    fn shipped_example_matches_the_defaults() {
        let (config, _) = resolve(EXAMPLE);
        let (a, d) = (Animations::default(), Decoration::default());
        assert_eq!(config.animations.enabled, a.enabled);
        assert_eq!(config.animations.duration_ms, a.duration_ms);
        assert_eq!(config.animations.curve, a.curve);
        assert_eq!(config.xwayland, XWayland::default());
        assert_eq!(config.decoration.rounding, d.rounding);
        assert_eq!(config.decoration.shadow_radius, d.shadow_radius);
        assert_eq!(config.decoration.shadow_color, d.shadow_color);
        assert_eq!(config.decoration.blur_passes, d.blur_passes);
        assert_eq!(config.decoration.blur_radius, d.blur_radius);
        assert_eq!(config.decoration.inactive_opacity, d.inactive_opacity);
    }

    #[test]
    fn animation_overrides_inherit_and_disable() {
        let (config, warnings) = resolve(
            r#"
            [animations]
            duration_ms = 100
            curve = "linear"
            window_move = { duration_ms = 300, curve = "spring 0.5" }
            workspace = { enabled = false }
            fade = { duration_ms = 0 }
            window_open = { curve = "wobble" }
            "#,
        );
        let a = &config.animations;
        let mv = a.spec(AnimKind::WindowMove).expect("move on");
        assert_eq!(mv.duration_ms, 300);
        assert_eq!(mv.curve, Curve::Spring { damping_ratio: 0.5 });
        let close = a.spec(AnimKind::WindowClose).expect("close on");
        assert_eq!((close.duration_ms, close.curve), (100, Curve::Linear));
        assert!(a.spec(AnimKind::Workspace).is_none());
        assert!(a.spec(AnimKind::Fade).is_none());
        // A bad curve keeps the inherited one.
        let open = a.spec(AnimKind::WindowOpen).expect("open on");
        assert_eq!(open.curve, Curve::Linear);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("window_open.curve"));
    }

    #[test]
    fn disabled_animations_give_no_specs() {
        let (config, _) = resolve("[animations]\nenabled = false\n");
        assert!(config.animations.spec(AnimKind::WindowMove).is_none());
    }

    #[test]
    fn decoration_bad_values_fall_back_per_key() {
        let (config, warnings) = resolve(
            r#"
            [decoration]
            rounding = 14
            blur_passes = 99
            shadow_color = "red"
            inactive_opacity = 1.5
            blur = false
            sparkle = true
            "#,
        );
        let d = &config.decoration;
        assert_eq!(d.rounding, 14);
        assert_eq!(d.blur_passes, Decoration::default().blur_passes);
        assert_eq!(d.shadow_color, Decoration::default().shadow_color);
        assert_eq!(d.inactive_opacity, 1.0);
        assert!(!d.blur);
        for needle in ["blur_passes", "shadow_color", "inactive_opacity", "sparkle"] {
            assert!(
                warnings.iter().any(|w| w.contains(needle)),
                "{needle}: {warnings:?}"
            );
        }
    }

    #[test]
    fn xwayland_scale_parses_and_rejects_bad_values() {
        assert_eq!(XWayland::default().dpi(), None, "the default changes nothing");
        let (config, warnings) = resolve("[xwayland]\nscale = 1.25\n");
        assert_eq!(config.xwayland.scale, 1.25);
        assert_eq!(config.xwayland.dpi(), Some(120));
        assert!(warnings.is_empty(), "{warnings:?}");
        let (config, _) = resolve("[xwayland]\nscale = 2\n");
        assert_eq!(config.xwayland.scale, 2.0);
        for bad in ["0", "9.5", "\"big\"", "-1"] {
            let (config, warnings) = resolve(&format!("[xwayland]\nscale = {bad}\n"));
            assert_eq!(config.xwayland, XWayland::default(), "{bad}");
            assert!(warnings.iter().any(|w| w.contains("scale")), "{bad}");
        }
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
    fn services_parse_with_defaults_and_drop_bad_entries() {
        let (config, warnings) = resolve(
            r#"
            [services.shell]
            command = "aurora-shell"

            [services.lock]
            command = "aurora-lock"
            autostart = false
            restart = "always"
            backoff_ms = 100
            max_backoff_ms = 5000

            [services.off]
            command = "x"
            enabled = false

            [services.nocmd]
            restart = "never"

            [services."bad name"]
            command = "x"

            [services.badrestart]
            command = "x"
            restart = "sometimes"

            [services.inverted]
            command = "x"
            backoff_ms = 9000
            max_backoff_ms = 100
            "#,
        );
        let names: Vec<_> = config.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["lock", "off", "shell"]);
        let shell = &config.services[2];
        assert!(shell.enabled && shell.autostart);
        assert_eq!(shell.restart, RestartPolicy::OnFailure);
        assert_eq!((shell.backoff_ms, shell.max_backoff_ms), (500, 30_000));
        let lock = &config.services[0];
        assert!(!lock.autostart);
        assert_eq!(lock.restart, RestartPolicy::Always);
        assert_eq!((lock.backoff_ms, lock.max_backoff_ms), (100, 5000));
        assert!(!config.services[1].enabled);
        for needle in ["nocmd", "bad name", "badrestart", "inverted"] {
            assert!(
                warnings.iter().any(|w| w.contains(needle)),
                "{needle}: {warnings:?}"
            );
        }
    }

    #[test]
    fn services_section_of_the_wrong_type_is_reported() {
        let (config, warnings) = resolve("services = 3\n");
        assert!(config.services.is_empty());
        assert!(
            warnings.iter().any(|w| w.contains("services")),
            "{warnings:?}"
        );
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

    #[test]
    fn output_vrr_parses_and_rejects_bad_values() {
        let (config, warnings) = resolve(
            r#"
            [[output]]
            name = "A-1"
            vrr = "on-demand"
            [[output]]
            name = "B-1"
            vrr = "on"
            [[output]]
            name = "C-1"
            [[output]]
            name = "D-1"
            vrr = "sometimes"
            [[output]]
            name = "E-1"
            vrr = true
            "#,
        );
        let vrr: Vec<_> = config.outputs.iter().map(|o| (o.name.as_str(), o.vrr)).collect();
        assert_eq!(
            vrr,
            [
                ("A-1", VrrMode::OnDemand),
                ("B-1", VrrMode::On),
                ("C-1", VrrMode::Off)
            ]
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().all(|w| w.contains("vrr")), "{warnings:?}");
    }
}
