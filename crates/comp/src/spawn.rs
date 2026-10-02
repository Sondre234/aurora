use std::{
    ffi::{OsStr, OsString},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
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

/// `argv[0]` looked up in `PATH` with `argv[1..]` as its arguments, no shell involved.
fn argv_command(argv: &[String], env: &[(&str, &OsStr)]) -> Option<Command> {
    let (program, args) = argv.split_first()?;
    let mut command = Command::new(program);
    command.args(args);
    prepare(&mut command, env);
    Some(command)
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

/// Runs a program without a shell (`argv[0]` from `PATH`), in its own session.
pub fn spawn_argv(argv: &[String], env: &[(&str, &OsStr)]) {
    let Some(command) = argv_command(argv, env) else {
        return;
    };
    let line = argv.join(" ");
    tracing::info!("spawn: {line}");
    launch(command, &line);
}

/// Runs a program without a shell like `spawn_argv`, but its waiting thread logs how it
/// ended (`<what>: ok` or `<what>: failed ...`) and kills it if it still runs after `limit`,
/// so a hung helper never lingers. Never blocks the caller.
pub fn spawn_logged(argv: &[String], env: &[(&str, &OsStr)], what: &'static str, limit: Duration) {
    let Some(mut command) = argv_command(argv, env) else {
        return;
    };
    command.stdout(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return tracing::warn!("{what}: failed to start {:?}: {err}", argv[0]),
    };
    let waiter = std::thread::Builder::new()
        .name("reaper".into())
        .spawn(move || {
            let start = Instant::now();
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) if start.elapsed() < limit => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(format!("timed out after {}s", limit.as_secs()));
                    }
                    Err(err) => break Err(err.to_string()),
                }
            };
            match status {
                Ok(status) if status.success() => tracing::info!("{what}: ok"),
                Ok(status) => tracing::warn!("{what}: failed {status}"),
                Err(err) => tracing::warn!("{what}: failed {err}"),
            }
        });
    if let Err(err) = waiter {
        tracing::warn!("spawn: cannot start a reaper thread: {err}");
    }
}

/// Starts `cmd` (through `sh -c`, own session) for the service supervisor, which reaps the
/// child itself so it can learn how it ended.
pub fn spawn_service(cmd: &str, env: &[(&str, &OsStr)]) -> std::io::Result<Child> {
    shell_command(cmd, env).spawn()
}

impl Aurora {
    /// What every child sees: how to reach this compositor, its X server and its IPC
    /// socket. Set on the Command only; the compositor's own environment stays as it was.
    pub fn base_env(&self) -> Vec<(&'static str, OsString)> {
        let mut env: Vec<(&'static str, OsString)> = vec![
            ("WAYLAND_DISPLAY", self.socket_name.clone()),
            ("XDG_SESSION_TYPE", "wayland".into()),
            ("XDG_CURRENT_DESKTOP", "Aurora".into()),
        ];
        if let Some(display) = self.xwayland.display {
            env.push(("DISPLAY", format!(":{display}").into()));
        }
        if let Some(path) = self.ipc_socket_path() {
            env.push(("AURORA_IPC_SOCK", path.into_os_string()));
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

    /// Starts a program (no shell) that talks to this compositor.
    pub fn spawn_argv(&mut self, argv: &[String]) {
        let env = self.spawn_env();
        let env: Vec<_> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        spawn_argv(argv, &env);
    }
}
