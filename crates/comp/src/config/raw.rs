//! The file as written: a loose TOML tree, converted section by section so one bad value
//! costs that value (or list item) and nothing else.

use serde::Deserialize;
use toml::{Table, Value};

use super::{
    AnimSpec, Animations, Color, Decoration, General, Glob, ModKey, ModeSpec, OutputRule,
    RestartPolicy, ServiceSpec, WindowRule, WorkspaceRule, XWayland,
};
use crate::anim::Curve;

/// Sections stay untyped `Value`s so a wrongly typed section cannot fail the whole parse.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawConfig {
    pub general: Option<Value>,
    pub animations: Option<Value>,
    pub decoration: Option<Value>,
    pub xwayland: Option<Value>,
    pub keybinds: Option<Value>,
    pub mousebinds: Option<Value>,
    pub output: Option<Value>,
    pub workspace: Option<Value>,
    pub window_rule: Option<Value>,
    pub autostart: Option<Value>,
    pub services: Option<Value>,
    /// Unknown top-level keys, reported as warnings.
    #[serde(flatten)]
    pub extra: Table,
}

/// Parses the text; the error is `line:col: message`.
pub fn parse(text: &str) -> Result<RawConfig, String> {
    toml::from_str(text).map_err(|err| {
        let (line, col) = err.span().map_or((0, 0), |s| line_col(text, s.start));
        format!("{line}:{col}: {}", err.message())
    })
}

fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

pub fn check_keys(ctx: &str, table: &Table, known: &[&str], warnings: &mut Vec<String>) {
    for key in table.keys().filter(|k| !known.contains(&k.as_str())) {
        warnings.push(format!("{ctx}: unknown key {key:?}"));
    }
}

fn wrong_type(ctx: &str, key: &str, want: &str, got: &Value) -> String {
    format!("{ctx}.{key}: expected {want}, got {}", got.type_str())
}

pub fn get_bool(ctx: &str, table: &Table, key: &str) -> Result<Option<bool>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Boolean(b)) => Ok(Some(*b)),
        Some(other) => Err(wrong_type(ctx, key, "a boolean", other)),
    }
}

pub fn get_str<'a>(ctx: &str, table: &'a Table, key: &str) -> Result<Option<&'a str>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(wrong_type(ctx, key, "a string", other)),
    }
}

fn get_int(ctx: &str, table: &Table, key: &str) -> Result<Option<i64>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(n)) => Ok(Some(*n)),
        Some(other) => Err(wrong_type(ctx, key, "an integer", other)),
    }
}

fn get_float(ctx: &str, table: &Table, key: &str) -> Result<Option<f64>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Float(f)) => Ok(Some(*f)),
        Some(Value::Integer(n)) => Ok(Some(*n as f64)),
        Some(other) => Err(wrong_type(ctx, key, "a number", other)),
    }
}

fn get_pair(ctx: &str, table: &Table, key: &str) -> Result<Option<(i32, i32)>, String> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let pair = value.as_array().and_then(|a| match a.as_slice() {
        [Value::Integer(x), Value::Integer(y)] => {
            Some((i32::try_from(*x).ok()?, i32::try_from(*y).ok()?))
        }
        _ => None,
    });
    pair.map(Some)
        .ok_or_else(|| format!("{ctx}.{key}: expected [x, y] integers"))
}

/// A soft error: the message is kept and the value falls back to its default.
fn soft<T>(result: Result<Option<T>, String>, warnings: &mut Vec<String>) -> Option<T> {
    result.unwrap_or_else(|err| {
        warnings.push(err);
        None
    })
}

fn ranged(
    ctx: &str,
    key: &str,
    table: &Table,
    range: std::ops::RangeInclusive<i64>,
    warnings: &mut Vec<String>,
) -> Option<i64> {
    let n = soft(get_int(ctx, table, key), warnings)?;
    if range.contains(&n) {
        Some(n)
    } else {
        warnings.push(format!(
            "{ctx}.{key}: {n} is out of range ({}..={})",
            range.start(),
            range.end()
        ));
        None
    }
}

const GENERAL_KEYS: &[&str] = &[
    "gaps_in",
    "gaps_out",
    "border_width",
    "border_focused",
    "border_unfocused",
    "focus_follows_mouse",
    "move_follows",
    "workspaces",
    "mod_key",
    "allow_virtual_keyboard",
    "focus_on_activate",
];

/// Per-key fallback: a bad value keeps that key's default, the rest of the section applies.
pub fn general(section: Option<&Value>, warnings: &mut Vec<String>) -> General {
    let mut g = General::default();
    let Some(section) = section else { return g };
    let Some(table) = section.as_table() else {
        warnings.push(format!(
            "general: expected a table, got {}",
            section.type_str()
        ));
        return g;
    };
    let ctx = "general";
    check_keys(ctx, table, GENERAL_KEYS, warnings);

    if let Some(n) = ranged(ctx, "gaps_in", table, 0..=1000, warnings) {
        g.gaps_in = n as i32;
    }
    if let Some(n) = ranged(ctx, "gaps_out", table, 0..=1000, warnings) {
        g.gaps_out = n as i32;
    }
    if let Some(n) = ranged(ctx, "border_width", table, 0..=100, warnings) {
        g.border_width = n as i32;
    }
    if let Some(n) = ranged(ctx, "workspaces", table, 1..=32, warnings) {
        g.workspaces = n as u32;
    }
    for (key, slot) in [
        ("border_focused", &mut g.border_focused),
        ("border_unfocused", &mut g.border_unfocused),
    ] {
        if let Some(text) = soft(get_str(ctx, table, key), warnings) {
            match Color::parse(text) {
                Some(color) => *slot = color,
                None => warnings.push(format!(
                    "{ctx}.{key}: invalid color {text:?} (#rrggbb or #rrggbbaa)"
                )),
            }
        }
    }
    if let Some(b) = soft(get_bool(ctx, table, "focus_follows_mouse"), warnings) {
        g.focus_follows_mouse = b;
    }
    if let Some(b) = soft(get_bool(ctx, table, "move_follows"), warnings) {
        g.move_follows = b;
    }
    if let Some(b) = soft(get_bool(ctx, table, "allow_virtual_keyboard"), warnings) {
        g.allow_virtual_keyboard = b;
    }
    if let Some(b) = soft(get_bool(ctx, table, "focus_on_activate"), warnings) {
        g.focus_on_activate = b;
    }
    if let Some(text) = soft(get_str(ctx, table, "mod_key"), warnings) {
        match ModKey::parse(text) {
            Some(key) => g.mod_key = key,
            None => warnings.push(format!(
                "{ctx}.mod_key: unknown {text:?} (super, ctrl, shift, alt, altgr)"
            )),
        }
    }
    g
}

const ANIMATION_KINDS: &[&str] = &[
    "window_move",
    "window_open",
    "window_close",
    "workspace",
    "fade",
];

/// A section that must be a table; anything else is reported and gives `None`.
fn table_of<'a>(
    ctx: &str,
    section: Option<&'a Value>,
    warnings: &mut Vec<String>,
) -> Option<&'a Table> {
    let section = section?;
    let table = section.as_table();
    if table.is_none() {
        warnings.push(format!(
            "{ctx}: expected a table, got {}",
            section.type_str()
        ));
    }
    table
}

fn curve(ctx: &str, table: &Table, warnings: &mut Vec<String>) -> Option<Curve> {
    let text = soft(get_str(ctx, table, "curve"), warnings)?;
    let parsed = Curve::parse(text);
    if parsed.is_none() {
        warnings.push(format!(
            "{ctx}.curve: unknown {text:?} (linear, ease-out, ease-in-out, spring [ratio], bezier x1 y1 x2 y2)"
        ));
    }
    parsed
}

fn duration_ms(ctx: &str, table: &Table, warnings: &mut Vec<String>) -> Option<u32> {
    ranged(ctx, "duration_ms", table, 0..=10_000, warnings).map(|n| n as u32)
}

/// `[animations]`: `enabled`, `duration_ms`, `curve` and one inline table per kind that
/// overrides any of `enabled`, `duration_ms`, `curve` for that kind.
pub fn animations(section: Option<&Value>, warnings: &mut Vec<String>) -> Animations {
    let mut a = Animations::default();
    let Some(table) = table_of("animations", section, warnings) else {
        return a;
    };
    let ctx = "animations";
    let known: Vec<&str> = ["enabled", "duration_ms", "curve"]
        .into_iter()
        .chain(ANIMATION_KINDS.iter().copied())
        .collect();
    check_keys(ctx, table, &known, warnings);
    if let Some(b) = soft(get_bool(ctx, table, "enabled"), warnings) {
        a.enabled = b;
    }
    if let Some(ms) = duration_ms(ctx, table, warnings) {
        a.duration_ms = ms;
    }
    if let Some(c) = curve(ctx, table, warnings) {
        a.curve = c;
    }
    let base = AnimSpec {
        enabled: true,
        duration_ms: a.duration_ms,
        curve: a.curve,
    };
    let mut kind = |key: &str| -> AnimSpec {
        let mut spec = base;
        let Some(value) = table.get(key) else {
            return spec;
        };
        let kctx = format!("{ctx}.{key}");
        let Some(t) = table_of(&kctx, Some(value), warnings) else {
            return spec;
        };
        check_keys(&kctx, t, &["enabled", "duration_ms", "curve"], warnings);
        if let Some(b) = soft(get_bool(&kctx, t, "enabled"), warnings) {
            spec.enabled = b;
        }
        if let Some(ms) = duration_ms(&kctx, t, warnings) {
            spec.duration_ms = ms;
        }
        if let Some(c) = curve(&kctx, t, warnings) {
            spec.curve = c;
        }
        spec
    };
    a.window_move = kind("window_move");
    a.window_open = kind("window_open");
    a.window_close = kind("window_close");
    a.workspace = kind("workspace");
    a.fade = kind("fade");
    a
}

/// `[decoration]`: per-key fallback like `[general]`.
pub fn decoration(section: Option<&Value>, warnings: &mut Vec<String>) -> Decoration {
    let mut d = Decoration::default();
    let Some(table) = table_of("decoration", section, warnings) else {
        return d;
    };
    let ctx = "decoration";
    check_keys(
        ctx,
        table,
        &[
            "rounding",
            "shadow",
            "shadow_radius",
            "shadow_color",
            "blur",
            "blur_passes",
            "blur_radius",
            "inactive_opacity",
        ],
        warnings,
    );
    if let Some(n) = ranged(ctx, "rounding", table, 0..=100, warnings) {
        d.rounding = n as i32;
    }
    if let Some(b) = soft(get_bool(ctx, table, "shadow"), warnings) {
        d.shadow = b;
    }
    if let Some(n) = ranged(ctx, "shadow_radius", table, 0..=200, warnings) {
        d.shadow_radius = n as i32;
    }
    if let Some(text) = soft(get_str(ctx, table, "shadow_color"), warnings) {
        match Color::parse(text) {
            Some(color) => d.shadow_color = color,
            None => warnings.push(format!(
                "{ctx}.shadow_color: invalid color {text:?} (#rrggbb or #rrggbbaa)"
            )),
        }
    }
    if let Some(b) = soft(get_bool(ctx, table, "blur"), warnings) {
        d.blur = b;
    }
    if let Some(n) = ranged(ctx, "blur_passes", table, 1..=8, warnings) {
        d.blur_passes = n as u32;
    }
    if let Some(n) = ranged(ctx, "blur_radius", table, 1..=64, warnings) {
        d.blur_radius = n as u32;
    }
    if let Some(f) = soft(get_float(ctx, table, "inactive_opacity"), warnings) {
        if (0.0..=1.0).contains(&f) {
            d.inactive_opacity = f as f32;
        } else {
            warnings.push(format!(
                "{ctx}.inactive_opacity: {f} is out of range (0..=1)"
            ));
        }
    }
    d
}

/// `[xwayland]`: `scale`, per-key fallback like `[general]`.
pub fn xwayland(section: Option<&Value>, warnings: &mut Vec<String>) -> XWayland {
    let mut x = XWayland::default();
    let Some(table) = table_of("xwayland", section, warnings) else {
        return x;
    };
    let ctx = "xwayland";
    check_keys(ctx, table, &["scale"], warnings);
    if let Some(f) = soft(get_float(ctx, table, "scale"), warnings) {
        if (XWayland::MIN_SCALE..=XWayland::MAX_SCALE).contains(&f) {
            x.scale = f;
        } else {
            warnings.push(format!(
                "{ctx}.scale: {f} is out of range ({}..={})",
                XWayland::MIN_SCALE,
                XWayland::MAX_SCALE
            ));
        }
    }
    x
}

/// Items of a `[[list]]` section; `convert` errors drop only that item.
fn items<T>(
    name: &str,
    section: Option<&Value>,
    warnings: &mut Vec<String>,
    mut convert: impl FnMut(&str, &Table, &mut Vec<String>) -> Result<T, String>,
) -> Vec<T> {
    let Some(section) = section else {
        return Vec::new();
    };
    let Some(list) = section.as_array() else {
        warnings.push(format!(
            "{name}: expected an array of tables, got {}",
            section.type_str()
        ));
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, item) in list.iter().enumerate() {
        let ctx = format!("{name}[{}]", i + 1);
        let Some(table) = item.as_table() else {
            warnings.push(format!("{ctx}: expected a table, got {}", item.type_str()));
            continue;
        };
        match convert(&ctx, table, warnings) {
            Ok(v) => out.push(v),
            Err(err) => warnings.push(format!("{ctx}: {err}, item dropped")),
        }
    }
    out
}

/// Strips the `ctx.key: ` prefix a getter adds, since the caller prefixes the item.
fn strip(err: String, ctx: &str) -> String {
    err.strip_prefix(ctx)
        .map(|s| s.trim_start_matches('.').to_string())
        .unwrap_or(err)
}

pub fn outputs(section: Option<&Value>, warnings: &mut Vec<String>) -> Vec<OutputRule> {
    let mut rules: Vec<OutputRule> = items("output", section, warnings, |ctx, t, w| {
        check_keys(
            ctx,
            t,
            &["name", "enabled", "primary", "position", "mode", "scale"],
            w,
        );
        let e = |err| strip(err, ctx);
        let name = get_str(ctx, t, "name").map_err(e)?.ok_or("missing name")?;
        let mode = match get_str(ctx, t, "mode").map_err(e)? {
            Some(text) => Some(
                ModeSpec::parse(text)
                    .ok_or_else(|| format!("invalid mode {text:?} (WxH or WxH@Hz)"))?,
            ),
            None => None,
        };
        let scale = get_float(ctx, t, "scale").map_err(e)?;
        if let Some(s) = scale
            && !(0.25..=8.0).contains(&s)
        {
            return Err(format!("scale {s} is out of range (0.25..=8)"));
        }
        let position = get_pair(ctx, t, "position").map_err(e)?;
        if let Some((x, y)) = position
            && ![x, y].iter().all(|v| (-100_000..=100_000).contains(v))
        {
            return Err("position is out of range (-100000..=100000)".into());
        }
        Ok(OutputRule {
            name: name.to_string(),
            enabled: get_bool(ctx, t, "enabled").map_err(e)?.unwrap_or(true),
            primary: get_bool(ctx, t, "primary").map_err(e)?.unwrap_or(false),
            position,
            mode,
            scale,
        })
    });
    dedup(&mut rules, |r| r.name.clone(), "output", warnings);
    rules
}

pub fn workspace_rules(
    section: Option<&Value>,
    max_ws: u32,
    warnings: &mut Vec<String>,
) -> Vec<WorkspaceRule> {
    let mut rules: Vec<WorkspaceRule> = items("workspace", section, warnings, |ctx, t, w| {
        check_keys(ctx, t, &["id", "output", "default"], w);
        let e = |err| strip(err, ctx);
        let id = get_int(ctx, t, "id").map_err(e)?.ok_or("missing id")?;
        let id = u32::try_from(id)
            .ok()
            .filter(|id| (1..=max_ws).contains(id))
            .ok_or_else(|| format!("id {id} is out of range (1..={max_ws})"))?;
        Ok(WorkspaceRule {
            id,
            output: get_str(ctx, t, "output").map_err(e)?.map(str::to_string),
            default: get_bool(ctx, t, "default").map_err(e)?.unwrap_or(false),
        })
    });
    dedup(&mut rules, |r| r.id, "workspace", warnings);
    rules
}

pub fn window_rules(
    section: Option<&Value>,
    max_ws: u32,
    warnings: &mut Vec<String>,
) -> Vec<WindowRule> {
    items("window_rule", section, warnings, |ctx, t, w| {
        check_keys(
            ctx,
            t,
            &[
                "app_id",
                "title",
                "class",
                "floating",
                "workspace",
                "output",
                "size",
                "fullscreen",
            ],
            w,
        );
        let e = |err| strip(err, ctx);
        let glob = |key| -> Result<Option<Glob>, String> {
            Ok(get_str(ctx, t, key).map_err(e)?.map(Glob::new))
        };
        let (app_id, title, class) = (glob("app_id")?, glob("title")?, glob("class")?);
        if app_id.is_none() && title.is_none() && class.is_none() {
            return Err("needs at least one of app_id, title, class".into());
        }
        let workspace = match get_int(ctx, t, "workspace").map_err(e)? {
            Some(n) => Some(
                u32::try_from(n)
                    .ok()
                    .filter(|n| (1..=max_ws).contains(n))
                    .ok_or_else(|| format!("workspace {n} is out of range (1..={max_ws})"))?,
            ),
            None => None,
        };
        let size = get_pair(ctx, t, "size").map_err(e)?;
        if size.is_some_and(|(w, h)| w <= 0 || h <= 0) {
            return Err("size must be positive".into());
        }
        Ok(WindowRule {
            app_id,
            title,
            class,
            floating: get_bool(ctx, t, "floating").map_err(e)?,
            workspace,
            output: get_str(ctx, t, "output").map_err(e)?.map(str::to_string),
            size,
            fullscreen: get_bool(ctx, t, "fullscreen").map_err(e)?,
        })
    })
}

pub fn autostart(section: Option<&Value>, warnings: &mut Vec<String>) -> Vec<String> {
    let Some(section) = section else {
        return Vec::new();
    };
    let Some(list) = section.as_array() else {
        warnings.push(format!(
            "autostart: expected an array of strings, got {}",
            section.type_str()
        ));
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, item) in list.iter().enumerate() {
        match item.as_str().filter(|s| !s.trim().is_empty()) {
            Some(cmd) => out.push(cmd.to_string()),
            None => warnings.push(format!(
                "autostart[{}]: expected a command string, item dropped",
                i + 1
            )),
        }
    }
    out
}

/// `[services.<name>]` tables: `command` (required), `enabled`, `autostart`, `restart`
/// (`never`, `on-failure`, `always`), `backoff_ms`, `max_backoff_ms`. A bad entry is dropped
/// alone; the result is sorted by name (the table's own order).
pub fn services(section: Option<&Value>, warnings: &mut Vec<String>) -> Vec<ServiceSpec> {
    let Some(table) = table_of("services", section, warnings) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, value) in table {
        let ctx = format!("services.{name}");
        let Some(t) = table_of(&ctx, Some(value), warnings) else {
            continue;
        };
        match service(name, &ctx, t, warnings) {
            Ok(spec) => out.push(spec),
            Err(err) => warnings.push(format!("{ctx}: {err}, service dropped")),
        }
    }
    out
}

fn service(
    name: &str,
    ctx: &str,
    t: &Table,
    warnings: &mut Vec<String>,
) -> Result<ServiceSpec, String> {
    check_keys(
        ctx,
        t,
        &[
            "command",
            "enabled",
            "autostart",
            "restart",
            "backoff_ms",
            "max_backoff_ms",
        ],
        warnings,
    );
    let e = |err| strip(err, ctx);
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("name must be 1..=64 of letters, digits, '-', '_', '.'".into());
    }
    let command = get_str(ctx, t, "command")
        .map_err(e)?
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or("missing command")?;
    let restart = match get_str(ctx, t, "restart").map_err(e)? {
        Some(text) => RestartPolicy::parse(text)
            .ok_or_else(|| format!("unknown restart {text:?} (never, on-failure, always)"))?,
        None => RestartPolicy::OnFailure,
    };
    let backoff = |key: &str, default: u32, warnings: &mut Vec<String>| {
        ranged(
            ctx,
            key,
            t,
            10..=i64::from(ServiceSpec::MAX_BACKOFF_MS),
            warnings,
        )
        .map_or(default, |n| n as u32)
    };
    let backoff_ms = backoff("backoff_ms", 500, warnings);
    let max_backoff_ms = backoff("max_backoff_ms", 30_000, warnings);
    if max_backoff_ms < backoff_ms {
        return Err(format!(
            "max_backoff_ms {max_backoff_ms} is below backoff_ms {backoff_ms}"
        ));
    }
    Ok(ServiceSpec {
        name: name.to_string(),
        command: command.to_string(),
        enabled: get_bool(ctx, t, "enabled").map_err(e)?.unwrap_or(true),
        autostart: get_bool(ctx, t, "autostart").map_err(e)?.unwrap_or(true),
        restart,
        backoff_ms,
        max_backoff_ms,
    })
}

/// Keeps the first item per key, so a later duplicate cannot silently override.
fn dedup<T, K: PartialEq + std::fmt::Display>(
    list: &mut Vec<T>,
    key: impl Fn(&T) -> K,
    name: &str,
    warnings: &mut Vec<String>,
) {
    let mut seen: Vec<K> = Vec::new();
    list.retain(|item| {
        let k = key(item);
        if seen.contains(&k) {
            warnings.push(format!(
                "{name}: duplicate entry for {k}, later one ignored"
            ));
            false
        } else {
            seen.push(k);
            true
        }
    });
}
