use std::{
    ffi::{OsStr, OsString},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
};

use crate::state::Aurora;

/// A `sh -c` command line, in its own session. `env` is set on the child only; the
/// compositor's own environment is never modified.
fn shell_command(cmd: &str, env: &[(&str, &OsStr)]) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd);
    prepare(&mut command, env);
    command
}

fn prepare(command: &mut Command, env: &[(&str, &OsStr)]) {
    command.stdin(Stdio::null());
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
}

/// One waiting thread per child keeps zombies from piling up without touching the loop.
fn reap(mut child: Child) {
    let reaper = std::thread::Builder::new()
        .name("reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
    if let Err(err) = reaper {
        tracing::warn!("spawn: cannot start a reaper thread: {err}");
    }
}

fn launch(mut command: Command, what: &str) {
    match command.spawn() {
        Ok(child) => reap(child),
        Err(err) => tracing::warn!("spawn: failed to start {what:?}: {err}"),
    }
}

/// Runs `cmd` through `sh -c` in its own session.
pub fn spawn(cmd: &str, env: &[(&str, &OsStr)]) {
    tracing::info!("spawn: {cmd}");
    launch(shell_command(cmd, env), cmd);
}

/// Starts `cmd` (through `sh -c`, own session) for the service supervisor, which reaps the
/// child itself so it can learn how it ended.
pub fn spawn_service(cmd: &str, env: &[(&str, &OsStr)]) -> std::io::Result<Child> {
    shell_command(cmd, env).spawn()
}

impl Aurora {
    /// What every child sees: how to reach this compositor and its X server. Set on the Command only; the compositor's own environment stays as it was.
    pub fn base_env(&self) -> Vec<(&'static str, OsString)> {
        let mut env: Vec<(&'static str, OsString)> = vec![
            ("WAYLAND_DISPLAY", self.socket_name.clone()),
            ("XDG_SESSION_TYPE", "wayland".into()),
            ("XDG_CURRENT_DESKTOP", "Aurora".into()),
        ];
        if let Some(display) = self.xwayland.display {
            env.push(("DISPLAY", format!(":{display}").into()));
        }
        env
    }

    /// `base_env` plus a token for the app to present when it maps, so a launched app
    /// takes focus.
    pub fn spawn_env(&mut self) -> Vec<(&'static str, OsString)> {
        let mut env = self.base_env();
        let activation = &mut self.protocols.activation;
        activation.retain_tokens(|_, d| d.timestamp.elapsed().as_secs() < 10);
        let (token, _) = activation.create_external_token(None);
        env.push(("XDG_ACTIVATION_TOKEN", token.as_str().into()));
        env.push(("DESKTOP_STARTUP_ID", token.as_str().into()));
        env
    }

    /// Starts a client that talks to this compositor.
    pub fn spawn(&mut self, cmd: &str) {
        let env = self.spawn_env();
        let env: Vec<_> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        spawn(cmd, &env);
    }
}
