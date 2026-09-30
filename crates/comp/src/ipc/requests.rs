//! What the compositor does with an IPC request. `gate` and `validate` are pure (and tested);
//! `Aurora::ipc_request` applies them and then calls the same window-manager entry points
//! the key binds use, so a request can never do anything a bind could not.
use aurora_ipc::{ErrorCode, OverviewAction, Request, Response};
use aurora_layout::WinId;
use aurora_theme::{Theme, curve_is_valid};

use crate::{action::WsTarget, state::Aurora, wm::Phase};

/// A typed failure: the code and a message for the client.
pub type Fail = (ErrorCode, String);

pub type Reply = Result<Response, Fail>;

fn fail<T>(code: ErrorCode, msg: impl Into<String>) -> Result<T, Fail> {
    Err((code, msg.into()))
}

/// Largest spawn request accepted: arguments and their total size.
const MAX_ARGS: usize = 256;
const MAX_ARGV_BYTES: usize = 128 * 1024;
const MAX_NAME: usize = 256;

/// While the session is locked a client may only look, ask for a lock, answer the lock
/// handshake, reload config and set the theme. Nothing that moves, focuses, closes or
/// launches.
pub fn gate(request: &Request, locked: bool) -> Result<(), Fail> {
    if !locked {
        return Ok(());
    }
    match request {
        Request::GetSnapshot
        | Request::ListWindows
        | Request::ListOutputs
        | Request::GetTheme
        | Request::ReloadConfig
        | Request::SetTheme(_)
        | Request::Lock
        | Request::Unlock => Ok(()),
        Request::SwitchWorkspace { .. }
        | Request::FocusWindow { .. }
        | Request::CloseWindow { .. }
        | Request::Spawn { .. }
        | Request::Overview(_) => fail(ErrorCode::Denied, "the session is locked"),
    }
}

/// Checks a request's arguments without touching any state. `workspaces` is the configured
/// workspace count.
pub fn validate(request: &Request, workspaces: u32) -> Result<(), Fail> {
    match request {
        Request::SwitchWorkspace { output, index } => {
            if !(1..=workspaces).contains(index) {
                return fail(
                    ErrorCode::BadRequest,
                    format!("workspace {index} is out of range (1..={workspaces})"),
                );
            }
            if output.as_ref().is_some_and(|o| o.len() > MAX_NAME) {
                return fail(ErrorCode::BadRequest, "output name is too long");
            }
            Ok(())
        }
        Request::Spawn { argv } => validate_argv(argv),
        Request::SetTheme(theme) => validate_theme(theme),
        _ => Ok(()),
    }
}

fn validate_argv(argv: &[String]) -> Result<(), Fail> {
    let Some(program) = argv.first() else {
        return fail(ErrorCode::BadRequest, "spawn: empty argv");
    };
    if program.trim().is_empty() {
        return fail(ErrorCode::BadRequest, "spawn: empty program name");
    }
    if argv.len() > MAX_ARGS {
        return fail(
            ErrorCode::BadRequest,
            format!("spawn: more than {MAX_ARGS} arguments"),
        );
    }
    if argv.iter().map(String::len).sum::<usize>() > MAX_ARGV_BYTES {
        return fail(ErrorCode::BadRequest, "spawn: arguments are too large");
    }
    if argv.iter().any(|a| a.contains('\0')) {
        return fail(ErrorCode::BadRequest, "spawn: argument contains a NUL byte");
    }
    Ok(())
}

/// The same ranges `Theme::from_toml_str` enforces on files.
pub fn validate_theme(theme: &Theme) -> Result<(), Fail> {
    let bad = |what: String| fail(ErrorCode::BadRequest, format!("theme: {what}"));
    let f = &theme.fonts;
    for (name, family) in [("family", &f.family), ("mono_family", &f.mono_family)] {
        if family.trim().is_empty() || family.len() > 256 {
            return bad(format!("fonts.{name} must be 1..=256 bytes"));
        }
    }
    for (name, size) in [("size", f.size), ("mono_size", f.mono_size)] {
        if !(1.0..=200.0).contains(&size) {
            return bad(format!("fonts.{name} {size} is out of range 1..=200"));
        }
    }
    let s = &theme.shape;
    for (name, v, max) in [
        ("radius", s.radius, 200),
        ("gap", s.gap, 200),
        ("border_width", s.border_width, 50),
        ("bar_height", s.bar_height, 500),
    ] {
        if v > max {
            return bad(format!("shape.{name} {v} is out of range 0..={max}"));
        }
    }
    let m = &theme.motion;
    if m.duration_ms > 10_000 {
        return bad(format!(
            "motion.duration_ms {} is out of range 0..=10000",
            m.duration_ms
        ));
    }
    for (name, curve) in [("curve", &m.curve), ("fade_curve", &m.fade_curve)] {
        if !curve_is_valid(curve) {
            return bad(format!("motion.{name} {curve:?} is not a known curve"));
        }
    }
    Ok(())
}

/// Whether the IPC connection of process `client_pid` is the session-lock client
/// (`owner_pid`). Both must be known.
pub fn is_lock_client(client_pid: i32, owner_pid: Option<i32>) -> bool {
    client_pid > 0 && owner_pid == Some(client_pid)
}

impl Aurora {
    /// Runs one request from the client with process id `client_pid`.
    pub fn ipc_request(&mut self, client_pid: i32, request: Request) -> Reply {
        gate(&request, self.is_locked())?;
        validate(&request, self.config.general.workspaces)?;
        match request {
            Request::GetSnapshot => Ok(Response::Snapshot(self.ipc_current_snapshot())),
            Request::ListWindows => Ok(Response::Windows(self.ipc_fresh_snapshot().windows)),
            Request::ListOutputs => Ok(Response::Outputs(self.ipc_fresh_snapshot().outputs)),
            Request::GetTheme => Ok(Response::Theme(self.theme.clone())),
            Request::SwitchWorkspace { output, index } => {
                if let Some(name) = output {
                    let Some(target) = self.wm.outputs.iter().find(|o| o.name() == name) else {
                        return fail(ErrorCode::NotFound, format!("no output named {name:?}"));
                    };
                    self.wm.active_output = Some(target.clone());
                }
                if self.wm.active_output.is_none() {
                    return fail(ErrorCode::NotFound, "there is no output");
                }
                self.switch_workspace(WsTarget::Num(index));
                Ok(Response::Ok)
            }
            Request::FocusWindow { id } => {
                let id = WinId(id);
                let Some(ws) = self
                    .wm
                    .windows
                    .get(&id)
                    .filter(|w| w.phase == Phase::Mapped && w.ws != 0)
                    .map(|w| w.ws)
                else {
                    return fail(ErrorCode::NotFound, format!("no window {}", id.0));
                };
                if !self.wm.ws_output.contains_key(&ws) {
                    self.switch_workspace(WsTarget::Num(ws));
                }
                self.focus_window(Some(id), true);
                Ok(Response::Ok)
            }
            Request::CloseWindow { id } => {
                let Some(id) = id.map(WinId).or(self.wm.focused) else {
                    return fail(ErrorCode::NotFound, "no window is focused");
                };
                if !self.close_window(id) {
                    return fail(ErrorCode::NotFound, format!("no window {}", id.0));
                }
                Ok(Response::Ok)
            }
            Request::Spawn { argv } => {
                self.spawn_argv(&argv);
                Ok(Response::Ok)
            }
            Request::ReloadConfig => match self.reload_config_checked() {
                Ok(()) => Ok(Response::Ok),
                Err(err) => fail(ErrorCode::Internal, err),
            },
            Request::Overview(action) => {
                match action {
                    OverviewAction::Toggle => self.toggle_overview(),
                    OverviewAction::Open => self.open_overview(),
                    OverviewAction::Close => self.close_overview(false),
                }
                Ok(Response::Ok)
            }
            Request::SetTheme(theme) => {
                self.set_live_theme(theme);
                Ok(Response::Ok)
            }
            Request::Lock => {
                if !self.is_locked() && !self.lock_service_configured() {
                    return fail(
                        ErrorCode::Unsupported,
                        "no [services.lock] is configured to take the session lock",
                    );
                }
                self.lock_action();
                Ok(Response::Ok)
            }
            Request::Unlock => {
                if !is_lock_client(client_pid, self.lock_owner_pid()) {
                    return fail(
                        ErrorCode::Denied,
                        "only the session-lock client may send unlock",
                    );
                }
                // The session unlocks when the client destroys its lock object, never
                // because of this message; it only tells us authentication succeeded.
                tracing::info!("lock: unlock acknowledged over ipc");
                Ok(Response::Ok)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code<T: std::fmt::Debug>(r: Result<T, Fail>) -> ErrorCode {
        r.unwrap_err().0
    }

    #[test]
    fn a_locked_session_refuses_everything_that_acts() {
        let acting = [
            Request::SwitchWorkspace {
                output: None,
                index: 1,
            },
            Request::FocusWindow { id: 1 },
            Request::CloseWindow { id: None },
            Request::Spawn {
                argv: vec!["x".into()],
            },
            Request::Overview(OverviewAction::Open),
        ];
        for r in &acting {
            assert_eq!(code(gate(r, true)), ErrorCode::Denied, "{r:?}");
            assert!(gate(r, false).is_ok(), "{r:?}");
        }
        let passive = [
            Request::GetSnapshot,
            Request::ListWindows,
            Request::ListOutputs,
            Request::GetTheme,
            Request::ReloadConfig,
            Request::SetTheme(Theme::default()),
            Request::Lock,
            Request::Unlock,
        ];
        for r in &passive {
            assert!(gate(r, true).is_ok(), "{r:?}");
        }
    }

    #[test]
    fn workspace_index_is_checked_against_the_configured_count() {
        let switch = |index| Request::SwitchWorkspace {
            output: None,
            index,
        };
        assert!(validate(&switch(1), 10).is_ok());
        assert!(validate(&switch(10), 10).is_ok());
        assert_eq!(code(validate(&switch(0), 10)), ErrorCode::BadRequest);
        assert_eq!(code(validate(&switch(11), 10)), ErrorCode::BadRequest);
        assert_eq!(code(validate(&switch(u32::MAX), 10)), ErrorCode::BadRequest);
        let long = Request::SwitchWorkspace {
            output: Some("x".repeat(MAX_NAME + 1)),
            index: 1,
        };
        assert_eq!(code(validate(&long, 10)), ErrorCode::BadRequest);
    }

    #[test]
    fn spawn_argv_limits() {
        let spawn = |argv: Vec<&str>| Request::Spawn {
            argv: argv.into_iter().map(String::from).collect(),
        };
        assert!(validate(&spawn(vec!["kitty", "-e", "htop"]), 10).is_ok());
        assert_eq!(code(validate(&spawn(vec![]), 10)), ErrorCode::BadRequest);
        assert_eq!(code(validate(&spawn(vec![" "]), 10)), ErrorCode::BadRequest);
        assert_eq!(
            code(validate(&spawn(vec!["a", "b\0c"]), 10)),
            ErrorCode::BadRequest
        );
        let many = Request::Spawn {
            argv: vec!["a".into(); MAX_ARGS + 1],
        };
        assert_eq!(code(validate(&many, 10)), ErrorCode::BadRequest);
        let big = Request::Spawn {
            argv: vec!["a".repeat(MAX_ARGV_BYTES), "b".into()],
        };
        assert_eq!(code(validate(&big, 10)), ErrorCode::BadRequest);
    }

    #[test]
    fn theme_validation_follows_the_file_ranges() {
        assert!(validate_theme(&Theme::default()).is_ok());
        let mut t = Theme::default();
        t.fonts.size = 0.5;
        assert!(validate_theme(&t).is_err());
        let mut t = Theme::default();
        t.fonts.size = f32::NAN;
        assert!(validate_theme(&t).is_err());
        let mut t = Theme::default();
        t.fonts.family = "  ".into();
        assert!(validate_theme(&t).is_err());
        let mut t = Theme::default();
        t.shape.radius = 201;
        assert!(validate_theme(&t).is_err());
        let mut t = Theme::default();
        t.motion.duration_ms = 10_001;
        assert!(validate_theme(&t).is_err());
        let mut t = Theme::default();
        t.motion.curve = "wobble".into();
        assert!(validate_theme(&t).is_err());
        let mut t = Theme::default();
        t.motion.fade_curve = "bezier 0.2 0 0.4 1".into();
        assert!(validate_theme(&t).is_ok());
    }

    #[test]
    fn only_the_lock_owner_may_unlock() {
        assert!(is_lock_client(42, Some(42)));
        assert!(!is_lock_client(42, Some(43)));
        assert!(!is_lock_client(42, None));
        assert!(!is_lock_client(0, Some(0)));
        assert!(!is_lock_client(-1, Some(-1)));
    }
}
