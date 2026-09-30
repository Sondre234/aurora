//! Single-instance guard: an exclusive `flock` on a file in the runtime directory, held for
//! the life of the process. The kernel drops it when the process dies, so a crash never
//! leaves a stale guard behind.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum GuardError {
    /// Another aurora-lock holds the guard.
    AlreadyRunning,
    Io(io::Error),
}

/// Keeps the guard until dropped.
#[derive(Debug)]
pub struct Guard {
    _file: File,
}

/// `$XDG_RUNTIME_DIR/aurora/lock.lock` (falls back to `/tmp` only for a missing runtime
/// dir, which a real session always has).
pub fn guard_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("aurora").join("lock.lock")
}

pub fn acquire(path: &Path) -> Result<Guard, GuardError> {
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(GuardError::Io)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(GuardError::Io)?;
    // SAFETY: `flock` on a valid fd.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Guard { _file: file });
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        Err(GuardError::AlreadyRunning)
    } else {
        Err(GuardError::Io(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aurora-lock-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn second_instance_is_refused_until_the_first_ends() {
        let path = temp("guard").join("lock.lock");
        let first = acquire(&path).expect("first acquires");
        assert!(matches!(acquire(&path), Err(GuardError::AlreadyRunning)));
        drop(first);
        assert!(acquire(&path).is_ok());
        let _ = fs::remove_dir_all(path.parent().expect("has a parent"));
    }
}
