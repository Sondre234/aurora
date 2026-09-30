//! Session glue: where to look for apps, which terminal to use, direct spawning.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::entry::{Env, desktops_from, locales_from};
use crate::index::{Index, application_dirs, scan};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The `applications` directories of this session, highest priority first.
pub fn dirs() -> Vec<PathBuf> {
    application_dirs(
        var("XDG_DATA_HOME").as_deref(),
        var("HOME").as_deref(),
        var("XDG_DATA_DIRS").as_deref(),
    )
}

/// True when `probe` is an existing file (absolute) or found in `PATH`.
pub fn executable_exists(probe: &str) -> bool {
    if probe.contains('/') {
        return Path::new(probe).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(probe).is_file()))
}

/// Scans the real system. Blocking disk I/O: call from a worker thread or at startup.
pub fn scan_system() -> Index {
    let env = Env {
        desktops: desktops_from(var("XDG_CURRENT_DESKTOP").as_deref()),
        locales: locales_from(&[var("LC_ALL"), var("LC_MESSAGES"), var("LANG")]),
        exists: &executable_exists,
    };
    scan(&dirs(), &env)
}

/// Wraps `argv` for a `Terminal=true` app: `$TERMINAL` (split on whitespace) or `kitty`,
/// then `-e`.
pub fn terminal_argv(terminal_env: Option<&str>, argv: &[String]) -> Vec<String> {
    let mut out: Vec<String> = terminal_env
        .map(|t| t.split_whitespace().map(str::to_string).collect())
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| vec!["kitty".to_string()]);
    out.push("-e".into());
    out.extend(argv.iter().cloned());
    out
}

/// Launches `argv` ourselves, detached in its own session, for when the compositor's IPC
/// is unavailable. Reaps the child on a thread so no zombie is left.
pub fn spawn_direct(argv: &[String]) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Safety: setsid is async-signal-safe and nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(mut child) => {
            let _ = std::thread::Builder::new()
                .name("reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
        Err(err) => tracing::warn!("launcher: cannot start {program:?}: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn terminal_wrapping() {
        assert_eq!(
            terminal_argv(None, &s(&["htop"])),
            s(&["kitty", "-e", "htop"])
        );
        assert_eq!(
            terminal_argv(Some("alacritty --class x"), &s(&["vim", "a"])),
            s(&["alacritty", "--class", "x", "-e", "vim", "a"])
        );
        assert_eq!(
            terminal_argv(Some("  "), &s(&["t"])),
            s(&["kitty", "-e", "t"])
        );
    }

    #[test]
    fn executable_probe_handles_paths_and_names() {
        assert!(!executable_exists("/definitely/not/here"));
        assert!(!executable_exists("definitely-not-a-real-program-xyz"));
    }
}
