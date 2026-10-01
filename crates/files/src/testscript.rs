//! QA input hook, compiled only with the `qa-hooks` cargo feature.
//!
//! `AURORA_FILES_TEST_SCRIPT=<file>` feeds key presses to the same handler real keys use.
//! One step per line (blank lines and `#` comments are skipped):
//!
//! ```text
//! Down                  a key; combos like ctrl+shift+n, alt+Left, Delete, F2, Enter
//! Down Down Enter       several keys on one line are several steps
//! type New name         types the rest of the line, one key per character
//! sleep 300             waits (milliseconds)
//! quit                  closes the window
//! ```
//!
//! A step runs only when the window is idle (no listing, operation or queue pending and
//! nothing happened for a moment), so a script can say "Enter, then Down" without sleeping
//! for the directory read in between. A conflict or confirm question counts as idle: the next
//! step is its answer.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use aurora_ui::runtime::calloop::LoopHandle;
use aurora_ui::runtime::calloop::timer::{TimeoutAction, Timer};
use aurora_ui::runtime::{Runtime, State};
use aurora_ui::{Key, KeyEvent, Mods};

use crate::app::Files;

const TICK: Duration = Duration::from_millis(25);
const SETTLE: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Key(KeyEvent),
    Type(String),
    Sleep(Duration),
    Quit,
}

#[derive(Default)]
pub struct Script {
    steps: VecDeque<Step>,
    wake_at: Option<Instant>,
}

impl Script {
    /// Reads `AURORA_FILES_TEST_SCRIPT`; no variable or a bad script is an empty script.
    pub fn from_env() -> Self {
        let Some(path) = std::env::var_os("AURORA_FILES_TEST_SCRIPT") else {
            return Self::default();
        };
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| parse(&t))
        {
            Ok(steps) => {
                tracing::info!("files: test script loaded steps={}", steps.len());
                Self {
                    steps,
                    wake_at: None,
                }
            }
            Err(e) => {
                tracing::error!("files: test script {}: {e}", path.to_string_lossy());
                Self::default()
            }
        }
    }
}

/// Parses a script; the error names the line.
pub fn parse(text: &str) -> Result<VecDeque<Step>, String> {
    let mut steps = VecDeque::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |e: &str| format!("line {}: {e}", n + 1);
        if let Some(rest) = line.strip_prefix("type ") {
            steps.push_back(Step::Type(rest.to_string()));
        } else if let Some(ms) = line.strip_prefix("sleep ") {
            let ms: u64 = ms.trim().parse().map_err(|_| at("bad sleep time"))?;
            steps.push_back(Step::Sleep(Duration::from_millis(ms)));
        } else if line == "quit" {
            steps.push_back(Step::Quit);
        } else {
            for token in line.split_whitespace() {
                let k = parse_key(token).ok_or_else(|| at(&format!("unknown key {token:?}")))?;
                steps.push_back(Step::Key(k));
            }
        }
    }
    Ok(steps)
}

/// `ctrl+shift+n`, `Enter`, `F2`, `a`, ... (case-insensitive names).
pub fn parse_key(token: &str) -> Option<KeyEvent> {
    let mut mods = Mods::default();
    let mut parts: Vec<&str> = token.split('+').collect();
    // A literal plus is written `shift+plus` or as the last, empty part (`ctrl++`).
    if token.ends_with('+') && parts.last() == Some(&"") {
        parts.pop();
        if let Some(last) = parts.last_mut()
            && last.is_empty()
        {
            *last = "plus";
        } else {
            parts.push("plus");
        }
    }
    let name = parts.pop()?;
    for m in parts {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "alt" => mods.alt = true,
            "shift" => mods.shift = true,
            "super" | "logo" => mods.logo = true,
            _ => return None,
        }
    }
    let lower = name.to_ascii_lowercase();
    let key = match lower.as_str() {
        "enter" | "return" => Key::Enter,
        "escape" | "esc" => Key::Escape,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "tab" => Key::Tab,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" => Key::PageUp,
        "pagedown" | "page_down" => Key::PageDown,
        "space" => Key::Char(' '),
        "slash" => Key::Char('/'),
        "plus" => Key::Char('+'),
        f if f.starts_with('f') && f.len() >= 2 && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u32 = f[1..].parse().ok()?;
            if !(1..=12).contains(&n) {
                return None;
            }
            Key::Other(0xFFBD + n)
        }
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            // A shifted letter arrives uppercase from the real keyboard.
            Key::Char(if mods.shift {
                c.to_ascii_uppercase()
            } else {
                c
            })
        }
    };
    let text = match key {
        Key::Char(c) if !mods.ctrl && !mods.alt && !mods.logo => Some(c.to_string()),
        _ => None,
    };
    Some(KeyEvent { key, text, mods })
}

/// Starts the periodic tick that feeds the script.
pub fn install(handle: &LoopHandle<'static, State<Files>>) {
    let _ = handle.insert_source(
        Timer::from_duration(TICK),
        |_, _, state: &mut State<Files>| {
            state.app.script_tick(&mut state.rt);
            TimeoutAction::ToDuration(TICK)
        },
    );
}

impl Files {
    pub fn script_tick(&mut self, rt: &mut Runtime<Self>) {
        if self.script.steps.is_empty() {
            return;
        }
        if !self.ready_logged {
            return;
        }
        if let Some(t) = self.script.wake_at {
            if Instant::now() < t {
                return;
            }
            self.script.wake_at = None;
        }
        let asking = self.scene.borrow().modal.is_some();
        let busy = self.awaiting.is_some()
            || self.sorting
            || (self.running.is_some() && !asking)
            || (!self.queue.is_empty() && !asking);
        if busy || self.last_activity.elapsed() < SETTLE {
            return;
        }
        let Some(step) = self.script.steps.pop_front() else {
            return;
        };
        match step {
            Step::Key(k) => self.on_key(rt, &k),
            Step::Type(text) => {
                for c in text.chars() {
                    self.on_key(rt, &KeyEvent::typed(c));
                }
            }
            Step::Sleep(d) => self.script.wake_at = Some(Instant::now() + d),
            Step::Quit => rt.quit(),
        }
        self.last_activity = Instant::now();
        if self.script.steps.is_empty() {
            tracing::info!("files: test script done");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> KeyEvent {
        parse_key(s).unwrap_or_else(|| panic!("{s}"))
    }

    #[test]
    fn keys_and_combos() {
        assert_eq!(k("Enter").key, Key::Enter);
        assert_eq!(k("esc").key, Key::Escape);
        assert_eq!(k("F2").key, Key::Other(0xFFBF));
        assert_eq!(k("f5").key, Key::Other(0xFFC2));
        assert_eq!(k("a").text.as_deref(), Some("a"));
        let c = k("ctrl+shift+n");
        assert_eq!(
            (c.key, c.mods.ctrl, c.mods.shift, c.text),
            (Key::Char('N'), true, true, None)
        );
        assert!(k("alt+Left").mods.alt);
        assert_eq!(k("shift+Delete").key, Key::Delete);
        assert_eq!(k("slash").key, Key::Char('/'));
        assert_eq!(k("ctrl++").key, Key::Char('+'));
        assert!(parse_key("hyper+a").is_none());
        assert!(parse_key("nonsense").is_none());
        assert!(parse_key("F13").is_none());
    }

    #[test]
    fn scripts_parse() {
        let steps =
            parse("# go\nDown Down Enter\n\ntype New name\nsleep 50\nctrl+a\nquit\n").unwrap();
        assert_eq!(steps.len(), 7);
        assert_eq!(steps[3], Step::Type("New name".into()));
        assert_eq!(steps[4], Step::Sleep(Duration::from_millis(50)));
        assert_eq!(steps[6], Step::Quit);
        let err = parse("Down\nbogus\n").unwrap_err();
        assert!(err.contains("line 2"), "{err}");
        assert!(parse("sleep soon").is_err());
    }
}
