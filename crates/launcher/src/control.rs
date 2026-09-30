//! The launcher's own control socket: `aurora-launcher toggle` (a second invocation)
//! tells the running daemon what to do.
//!
//! Why not a compositor IPC request: the compositor should not know launchers exist, and a
//! user bind is already "run this command" (`spawn aurora-launcher toggle`). The price is
//! one short-lived process per keypress, which only writes a line to a unix socket.
//!
//! Protocol: the client connects to `$XDG_RUNTIME_DIR/aurora/launcher.sock` (override with
//! `AURORA_LAUNCHER_SOCK`), sends one line (`toggle`, `show` or `hide`) and reads the
//! daemon's answer (`ok` or `err <reason>`). The directory is 0700 and the socket 0600.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Toggle,
    Show,
    Hide,
}

impl Command {
    pub fn name(self) -> &'static str {
        match self {
            Command::Toggle => "toggle",
            Command::Show => "show",
            Command::Hide => "hide",
        }
    }
}

pub fn parse_command(line: &str) -> Option<Command> {
    match line.trim() {
        "toggle" => Some(Command::Toggle),
        "show" => Some(Command::Show),
        "hide" => Some(Command::Hide),
        _ => None,
    }
}

/// `$AURORA_LAUNCHER_SOCK`, else `$XDG_RUNTIME_DIR/aurora/launcher.sock`.
pub fn socket_path() -> Option<PathBuf> {
    socket_path_from(
        std::env::var_os("AURORA_LAUNCHER_SOCK"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
}

pub fn socket_path_from(
    override_path: Option<std::ffi::OsString>,
    runtime_dir: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(p) = override_path.filter(|p| !p.is_empty()) {
        return Some(p.into());
    }
    runtime_dir
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("aurora").join("launcher.sock"))
}

/// Client side: sends `cmd` to the running daemon.
pub fn send(cmd: Command) -> io::Result<()> {
    let path = socket_path().ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "the launcher daemon is not running ({}: {e})",
                path.display()
            ),
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "{}", cmd.name())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    match answer.trim() {
        "ok" => Ok(()),
        other => Err(io::Error::other(format!("daemon answered {other:?}"))),
    }
}

/// Server side: binds the socket and serves commands on a thread, calling `on_command`
/// for each. Fails with `AddrInUse` when another daemon already answers on the socket;
/// a stale socket file from a dead daemon is replaced.
pub fn listen(on_command: impl Fn(Command) + Send + 'static) -> io::Result<PathBuf> {
    let path = socket_path().ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another launcher daemon is already running",
            ));
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new()
        .name("launcher-control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                let mut line = String::new();
                // Bounded read: a peer cannot make us buffer more than a short line.
                let read = BufReader::new((&stream).take(64)).read_line(&mut line);
                match (read, parse_command(&line)) {
                    (Ok(n), Some(cmd)) if n > 0 => {
                        on_command(cmd);
                        let _ = writeln!(stream, "ok");
                    }
                    (Ok(n), None) if n > 0 => {
                        let _ = writeln!(stream, "err unknown command");
                    }
                    _ => {}
                }
            }
        })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(parse_command("toggle\n"), Some(Command::Toggle));
        assert_eq!(parse_command(" show "), Some(Command::Show));
        assert_eq!(parse_command("hide"), Some(Command::Hide));
        assert_eq!(parse_command("quit"), None);
        assert_eq!(parse_command(""), None);
        for c in [Command::Toggle, Command::Show, Command::Hide] {
            assert_eq!(parse_command(c.name()), Some(c));
        }
    }

    #[test]
    fn socket_path_resolution() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        assert_eq!(
            socket_path_from(None, os("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/aurora/launcher.sock"))
        );
        assert_eq!(
            socket_path_from(os("/tmp/l.sock"), os("/run/user/1000")),
            Some(PathBuf::from("/tmp/l.sock"))
        );
        assert_eq!(socket_path_from(os(""), None), None);
    }
}
