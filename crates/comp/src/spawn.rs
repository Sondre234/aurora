use std::{
    ffi::{OsStr, OsString},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
};

use crate::state::Aurora;

/// Runs `cmd` through `sh -c` in its own session. `env` is set on the child only; the
/// compositor's own environment is never modified.
pub fn spawn(cmd: &str, env: &[(&str, &OsStr)]) {
    tracing::info!("spawn: {cmd}");
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd).stdin(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    // Safety: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            // The signalfd source blocks signals on this thread and Rust ignores SIGPIPE;
            // both survive exec, so children must not inherit them.
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::setsid();
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return tracing::warn!("spawn: failed to start {cmd:?}: {err}"),
    };
    // One waiting thread per child keeps zombies from piling up without touching the loop.
    let reaper = std::thread::Builder::new()
        .name("reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
    if let Err(err) = reaper {
        tracing::warn!("spawn: cannot start a reaper thread: {err}");
    }
}

impl Aurora {
    /// What every child sees: how to reach this compositor and its X server. Set on the
    /// Command only; the compositor's own environment stays as it was.
    pub fn spawn_env(&mut self) -> Vec<(&'static str, OsString)> {
        let mut env: Vec<(&'static str, OsString)> = vec![
            ("WAYLAND_DISPLAY", self.socket_name.clone()),
            ("XDG_SESSION_TYPE", "wayland".into()),
            ("XDG_CURRENT_DESKTOP", "Aurora".into()),
        ];
        // A token for the app to present when it maps, so a launched app takes focus.
        let activation = &mut self.protocols.activation;
        activation.retain_tokens(|_, d| d.timestamp.elapsed().as_secs() < 10);
        let (token, _) = activation.create_external_token(None);
        env.push(("XDG_ACTIVATION_TOKEN", token.as_str().into()));
        env.push(("DESKTOP_STARTUP_ID", token.as_str().into()));
        if let Some(display) = self.xwayland.display {
            env.push(("DISPLAY", format!(":{display}").into()));
        }
        env
    }

    /// Starts a client that talks to this compositor.
    pub fn spawn(&mut self, cmd: &str) {
        let env = self.spawn_env();
        let env: Vec<_> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        spawn(cmd, &env);
    }
}
