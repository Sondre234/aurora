use std::{
    ffi::OsStr,
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
    /// Starts a client that talks to this compositor.
    pub fn spawn(&self, cmd: &str) {
        spawn(cmd, &[("WAYLAND_DISPLAY", &self.socket_name)]);
    }
}
