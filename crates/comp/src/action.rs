use std::{fmt, str::FromStr};

use crate::config::keybind::WheelDir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl FromStr for Dir {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "left" | "l" => Ok(Self::Left),
            "right" | "r" => Ok(Self::Right),
            "up" | "u" => Ok(Self::Up),
            "down" | "d" => Ok(Self::Down),
            other => Err(format!(
                "unknown direction {other:?} (left, right, up, down)"
            )),
        }
    }
}

impl fmt::Display for Dir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsTarget {
    Num(u32),
    Next,
    Prev,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Spawn(String),
    Close,
    Focus(Dir),
    Move(Dir),
    Swap(Dir),
    ResizeSplit(Dir, i32),
    Workspace(WsTarget),
    MoveToWorkspace(u32),
    FocusOutput(Dir),
    MoveToOutput(Dir),
    ToggleFloating,
    Fullscreen,
    Maximize,
    ToggleSplit,
    ReloadConfig,
    Quit,
    /// Placeholder in a user config that removes a default bind.
    None,
    /// Mouse binds: start an interactive move/resize of the window under the pointer.
    DragMove,
    DragResize,
    /// `--qa` only: log the state dump (same as SIGUSR2).
    DebugDump,
    /// `--qa` only: inject pointer input through the normal primitives.
    DebugPointer(DebugPointer),
}

/// Workspaces are capped at 32 (`general.workspaces`); the config resolver applies the
/// configured count on top of this.
pub const MAX_WORKSPACES: u32 = 32;

const MAX_RESIZE_PX: i32 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

impl PointerButton {
    /// evdev button code.
    pub fn code(self) -> u32 {
        match self {
            Self::Left => 272,
            Self::Right => 273,
            Self::Middle => 274,
        }
    }
}

/// Coordinates are global logical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugPointer {
    Move(i32, i32),
    MoveBy(i32, i32),
    Press(PointerButton),
    Release(PointerButton),
    Scroll(WheelDir),
}

fn parse_debug_pointer(rest: &str) -> Result<DebugPointer, String> {
    let mut args = rest.split_whitespace();
    let mut next = |what: &str| {
        args.next()
            .ok_or_else(|| format!("debug-pointer: missing {what}"))
    };
    let coord = |s: &str| {
        s.parse::<i32>()
            .map_err(|_| format!("invalid coordinate {s:?}"))
    };
    let button = |s: &str| match s {
        "left" => Ok(PointerButton::Left),
        "right" => Ok(PointerButton::Right),
        "middle" => Ok(PointerButton::Middle),
        other => Err(format!("unknown button {other:?} (left, right, middle)")),
    };
    let action = match next("subcommand")? {
        "move" => DebugPointer::Move(coord(next("x")?)?, coord(next("y")?)?),
        "move-by" => DebugPointer::MoveBy(coord(next("dx")?)?, coord(next("dy")?)?),
        "press" => DebugPointer::Press(button(next("button")?)?),
        "release" => DebugPointer::Release(button(next("button")?)?),
        "scroll" => DebugPointer::Scroll(match next("direction")? {
            "up" => WheelDir::Up,
            "down" => WheelDir::Down,
            other => return Err(format!("unknown scroll direction {other:?} (up, down)")),
        }),
        other => return Err(format!("unknown debug-pointer command {other:?}")),
    };
    if args.next().is_some() {
        return Err("debug-pointer: too many arguments".into());
    }
    Ok(action)
}

impl fmt::Display for DebugPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let button = |b: &PointerButton| match b {
            PointerButton::Left => "left",
            PointerButton::Right => "right",
            PointerButton::Middle => "middle",
        };
        match self {
            Self::Move(x, y) => write!(f, "move {x} {y}"),
            Self::MoveBy(x, y) => write!(f, "move-by {x} {y}"),
            Self::Press(b) => write!(f, "press {}", button(b)),
            Self::Release(b) => write!(f, "release {}", button(b)),
            Self::Scroll(WheelDir::Up) => f.write_str("scroll up"),
            Self::Scroll(WheelDir::Down) => f.write_str("scroll down"),
        }
    }
}

impl Action {
    /// Like `from_str`, but workspace numbers above `max_ws` are rejected too.
    pub fn parse(s: &str, max_ws: u32) -> Result<Self, String> {
        let action: Self = s.parse()?;
        match action {
            Self::Workspace(WsTarget::Num(n)) | Self::MoveToWorkspace(n) if n > max_ws => {
                Err(format!("workspace {n} is out of range (1..={max_ws})"))
            }
            _ => Ok(action),
        }
    }
}

fn workspace_num(s: &str) -> Result<u32, String> {
    let n: u32 = s.parse().map_err(|_| format!("invalid workspace {s:?}"))?;
    if (1..=MAX_WORKSPACES).contains(&n) {
        Ok(n)
    } else {
        Err(format!(
            "workspace {n} is out of range (1..={MAX_WORKSPACES})"
        ))
    }
}

impl FromStr for Action {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (name, rest) = match s.split_once(char::is_whitespace) {
            Some((name, rest)) => (name, rest.trim()),
            None => (s, ""),
        };
        let mut args = rest.split_whitespace();
        let mut arg = || {
            args.next()
                .ok_or_else(|| format!("{name}: missing argument"))
        };

        let action = match name {
            "spawn" => {
                if rest.is_empty() {
                    return Err("spawn: missing command".into());
                }
                return Ok(Self::Spawn(rest.to_string()));
            }
            "close" => Self::Close,
            "focus" => Self::Focus(arg()?.parse()?),
            "move" => Self::Move(arg()?.parse()?),
            "swap" => Self::Swap(arg()?.parse()?),
            "resize-split" => {
                let dir = arg()?.parse()?;
                let px = arg()?;
                let px: i32 = px
                    .parse()
                    .map_err(|_| format!("invalid pixel count {px:?}"))?;
                if !(1..=MAX_RESIZE_PX).contains(&px) {
                    return Err(format!(
                        "pixel count {px} is out of range (1..={MAX_RESIZE_PX})"
                    ));
                }
                Self::ResizeSplit(dir, px)
            }
            "workspace" => Self::Workspace(match arg()? {
                "next" => WsTarget::Next,
                "prev" => WsTarget::Prev,
                n => WsTarget::Num(workspace_num(n)?),
            }),
            "move-to-workspace" => Self::MoveToWorkspace(workspace_num(arg()?)?),
            "focus-output" => Self::FocusOutput(arg()?.parse()?),
            "move-to-output" => Self::MoveToOutput(arg()?.parse()?),
            "toggle-floating" => Self::ToggleFloating,
            "fullscreen" => Self::Fullscreen,
            "maximize" => Self::Maximize,
            "toggle-split" => Self::ToggleSplit,
            "reload-config" => Self::ReloadConfig,
            "quit" => Self::Quit,
            "none" => Self::None,
            "drag-move" => Self::DragMove,
            "drag-resize" => Self::DragResize,
            "debug-dump" => Self::DebugDump,
            "debug-pointer" => return Ok(Self::DebugPointer(parse_debug_pointer(rest)?)),
            "" => return Err("empty action".into()),
            other => return Err(format!("unknown action {other:?}")),
        };
        if args.next().is_some() {
            return Err(format!("{name}: too many arguments"));
        }
        Ok(action)
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(cmd) => write!(f, "spawn {cmd}"),
            Self::Close => f.write_str("close"),
            Self::Focus(d) => write!(f, "focus {d}"),
            Self::Move(d) => write!(f, "move {d}"),
            Self::Swap(d) => write!(f, "swap {d}"),
            Self::ResizeSplit(d, px) => write!(f, "resize-split {d} {px}"),
            Self::Workspace(WsTarget::Num(n)) => write!(f, "workspace {n}"),
            Self::Workspace(WsTarget::Next) => f.write_str("workspace next"),
            Self::Workspace(WsTarget::Prev) => f.write_str("workspace prev"),
            Self::MoveToWorkspace(n) => write!(f, "move-to-workspace {n}"),
            Self::FocusOutput(d) => write!(f, "focus-output {d}"),
            Self::MoveToOutput(d) => write!(f, "move-to-output {d}"),
            Self::ToggleFloating => f.write_str("toggle-floating"),
            Self::Fullscreen => f.write_str("fullscreen"),
            Self::Maximize => f.write_str("maximize"),
            Self::ToggleSplit => f.write_str("toggle-split"),
            Self::ReloadConfig => f.write_str("reload-config"),
            Self::Quit => f.write_str("quit"),
            Self::None => f.write_str("none"),
            Self::DragMove => f.write_str("drag-move"),
            Self::DragResize => f.write_str("drag-resize"),
            Self::DebugDump => f.write_str("debug-dump"),
            Self::DebugPointer(p) => write!(f, "debug-pointer {p}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_round_trips() {
        let ok = [
            "spawn kitty -o x=y",
            "close",
            "focus left",
            "move down",
            "swap up",
            "resize-split right 40",
            "workspace 3",
            "workspace next",
            "workspace prev",
            "move-to-workspace 10",
            "focus-output right",
            "move-to-output left",
            "toggle-floating",
            "fullscreen",
            "maximize",
            "toggle-split",
            "reload-config",
            "quit",
            "none",
            "drag-move",
            "drag-resize",
            "debug-dump",
            "debug-pointer move 10 -20",
            "debug-pointer move-by -3 4",
            "debug-pointer press left",
            "debug-pointer release middle",
            "debug-pointer scroll up",
        ];
        for text in ok {
            let action: Action = text.parse().unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(action.to_string(), text);
        }
    }

    #[test]
    fn rejects_bad_actions() {
        let bad = [
            "",
            "spawn",
            "focus",
            "focus sideways",
            "workspace 0",
            "workspace 33",
            "workspace",
            "move-to-workspace next",
            "resize-split left",
            "resize-split left 0",
            "close now",
            "teleport",
        ];
        for text in bad {
            assert!(text.parse::<Action>().is_err(), "{text:?} should not parse");
        }
        assert!(Action::parse("workspace 11", 10).is_err());
        assert!(Action::parse("move-to-workspace 11", 10).is_err());
        assert!(Action::parse("workspace 10", 10).is_ok());
    }
}
