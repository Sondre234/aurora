//! The pseudo-terminal and the child process on it.
//!
//! No fork of our own: the master comes from `openpt`, the child is a
//! `std::process::Command` whose `pre_exec` makes the slave its controlling terminal.
//! The master is nonblocking; the app registers a dup of it (and the child's pidfd) as
//! calloop sources and never blocks on either.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

use rustix::fs::{Mode, OFlags};
use rustix::process::{PidfdFlags, Signal};
use rustix::pty::OpenptFlags;
use rustix::termios::{InputModes, OptionalActions, Winsize};

/// Unsent input beyond this is dropped (a child that never reads must not eat memory).
pub const MAX_OUTBOX: usize = 8 << 20;

/// Window size for the kernel: cells, plus the pixel size programs may query.
pub fn winsize(cols: usize, rows: usize, cell_w: u32, cell_h: u32) -> Winsize {
    let clamp = |v: u64| v.min(u16::MAX as u64) as u16;
    Winsize {
        ws_col: clamp(cols as u64),
        ws_row: clamp(rows as u64),
        ws_xpixel: clamp(cols as u64 * cell_w as u64),
        ws_ypixel: clamp(rows as u64 * cell_h as u64),
    }
}

/// The program to run: the explicit command, else `$SHELL`, else `/bin/sh`.
pub fn command_line(explicit: Option<&[String]>, shell: Option<OsString>) -> Vec<String> {
    match explicit {
        Some(c) if !c.is_empty() => c.to_vec(),
        _ => {
            let sh = shell.filter(|s| !s.is_empty()).map_or_else(
                || "/bin/sh".to_string(),
                |s| s.to_string_lossy().into_owned(),
            );
            vec![sh]
        }
    }
}

/// Environment the child gets.
pub const CHILD_ENV: [(&str, &str); 3] = [
    ("TERM", "xterm-256color"),
    ("COLORTERM", "truecolor"),
    ("TERM_PROGRAM", "aurora-term"),
];

/// `code=<n>` or `signal=<n>`, as the log contract spells it.
pub fn exit_text(status: &ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(c), _) => format!("code={c}"),
        (None, Some(s)) => format!("signal={s}"),
        _ => "code=-1".to_string(),
    }
}

/// Bytes waiting to be written to the child. A write that would block leaves the rest
/// queued for the next writable wakeup.
#[derive(Debug, Default)]
pub struct Outbox {
    buf: VecDeque<u8>,
    dropped: usize,
}

impl Outbox {
    pub fn push(&mut self, bytes: &[u8]) {
        let room = MAX_OUTBOX.saturating_sub(self.buf.len());
        let take = bytes.len().min(room);
        self.buf.extend(&bytes[..take]);
        self.dropped += bytes.len() - take;
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Bytes refused because the queue was full, so far.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// Write until the queue is empty or `write` reports `WouldBlock`. `Ok(true)` when
    /// the queue is empty afterwards.
    pub fn flush(&mut self, mut write: impl FnMut(&[u8]) -> io::Result<usize>) -> io::Result<bool> {
        while !self.buf.is_empty() {
            let (head, _) = self.buf.as_slices();
            match write(head) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.buf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }
}

/// A running child on a pty.
pub struct Pty {
    master: OwnedFd,
    child: Child,
}

fn errno(e: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

impl Pty {
    /// Start `argv` on a new pty of size `ws`, in `cwd` if given.
    pub fn spawn(argv: &[String], cwd: Option<&Path>, ws: Winsize) -> io::Result<Pty> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
        let master =
            rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
                .map_err(errno)?;
        rustix::pty::grantpt(&master).map_err(errno)?;
        rustix::pty::unlockpt(&master).map_err(errno)?;
        let name = rustix::pty::ptsname(&master, Vec::new()).map_err(errno)?;
        let slave = rustix::fs::open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(errno)?;
        rustix::termios::tcsetwinsize(&master, ws).map_err(errno)?;
        // Cooked-mode line editing needs to know the input is UTF-8 (backspace over a
        // multi-byte character).
        if let Ok(mut t) = rustix::termios::tcgetattr(&slave) {
            t.input_modes |= InputModes::IUTF8;
            let _ = rustix::termios::tcsetattr(&slave, OptionalActions::Now, &t);
        }

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        for (k, v) in CHILD_ENV {
            cmd.env(k, v);
        }
        cmd.env_remove("LINES").env_remove("COLUMNS");
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // Safety: setsid and the TIOCSCTTY ioctl are async-signal-safe and allocate
        // nothing; the closure runs between fork and exec with fd 0 being the slave.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setsid().map_err(errno)?;
                rustix::process::ioctl_tiocsctty(rustix::stdio::stdin()).map_err(errno)?;
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        let flags = rustix::fs::fcntl_getfl(&master).map_err(errno)?;
        rustix::fs::fcntl_setfl(&master, flags | OFlags::NONBLOCK).map_err(errno)?;
        Ok(Pty { master, child })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// A second descriptor for the same master, for registering with the event loop.
    pub fn dup_master(&self) -> io::Result<OwnedFd> {
        self.master.try_clone()
    }

    /// A pidfd that becomes readable when the child exits.
    pub fn pidfd(&self) -> io::Result<OwnedFd> {
        let pid = rustix::process::Pid::from_raw(self.child.id() as i32)
            .ok_or_else(|| io::Error::other("bad pid"))?;
        rustix::process::pidfd_open(pid, PidfdFlags::empty()).map_err(errno)
    }

    pub fn resize(&self, ws: Winsize) -> io::Result<()> {
        rustix::termios::tcsetwinsize(&self.master, ws).map_err(errno)
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        rustix::io::read(self.master.as_fd(), buf).map_err(errno)
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        rustix::io::write(self.master.as_fd(), buf).map_err(errno)
    }

    /// The exit status once the child is gone (reaps it).
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Hang up the shell like closing a terminal does. Closing the master also hangs up
    /// the foreground job; this covers a shell that ignores the tty hangup.
    pub fn hangup(&mut self) {
        if matches!(self.child.try_wait(), Ok(None))
            && let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32)
        {
            let _ = rustix::process::kill_process(pid, Signal::HUP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winsize_saturates_and_carries_pixels() {
        let w = winsize(100, 40, 9, 18);
        assert_eq!((w.ws_col, w.ws_row), (100, 40));
        assert_eq!((w.ws_xpixel, w.ws_ypixel), (900, 720));
        let big = winsize(70_000, 10, 20, 20);
        assert_eq!((big.ws_col, big.ws_xpixel), (u16::MAX, u16::MAX));
    }

    #[test]
    fn command_line_prefers_explicit_then_shell() {
        let cmd = vec!["htop".to_string(), "-d".to_string()];
        assert_eq!(command_line(Some(&cmd), Some("/bin/zsh".into())), cmd);
        assert_eq!(command_line(None, Some("/bin/zsh".into())), ["/bin/zsh"]);
        assert_eq!(command_line(None, Some("".into())), ["/bin/sh"]);
        assert_eq!(command_line(Some(&[]), None), ["/bin/sh"]);
    }

    #[test]
    fn exit_text_formats_code_and_signal() {
        assert_eq!(exit_text(&ExitStatus::from_raw(3 << 8)), "code=3");
        assert_eq!(exit_text(&ExitStatus::from_raw(9)), "signal=9");
        assert_eq!(exit_text(&ExitStatus::from_raw(0)), "code=0");
    }

    #[test]
    fn outbox_resumes_after_a_short_write() {
        let mut o = Outbox::default();
        o.push(b"hello world");
        let mut sink = Vec::new();
        let mut budget = 5usize;
        let done = o
            .flush(|b| {
                if budget == 0 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let n = b.len().min(budget);
                budget -= n;
                sink.extend_from_slice(&b[..n]);
                Ok(n)
            })
            .expect("flush");
        assert!(!done);
        assert_eq!((sink.as_slice(), o.len()), (&b"hello"[..], 6));
        let done = o
            .flush(|b| {
                sink.extend_from_slice(b);
                Ok(b.len())
            })
            .expect("flush");
        assert!(done && o.is_empty());
        assert_eq!(sink, b"hello world");
    }

    #[test]
    fn outbox_is_capped() {
        let mut o = Outbox::default();
        o.push(&vec![0u8; MAX_OUTBOX - 2]);
        o.push(b"abcd");
        assert_eq!((o.len(), o.dropped()), (MAX_OUTBOX, 2));
    }

    #[test]
    fn outbox_surfaces_errors() {
        let mut o = Outbox::default();
        o.push(b"x");
        let err = o.flush(|_| Err(io::ErrorKind::BrokenPipe.into()));
        assert_eq!(err.map_err(|e| e.kind()), Err(io::ErrorKind::BrokenPipe));
        assert!(
            o.flush(|_| Ok(0))
                .is_err_and(|e| e.kind() == io::ErrorKind::WriteZero)
        );
    }
}
