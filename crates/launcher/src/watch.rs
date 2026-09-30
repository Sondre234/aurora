//! Debounced watching of the `applications` directories.
//!
//! inotify through `notify`; if it cannot be set up the thread falls back to polling a
//! cheap mtime signature every few seconds. A burst of events (a package install writes
//! dozens of files) collapses into one rescan after a quiet period, and our own reads
//! (access events) never trigger a rescan. Directories that do not exist yet are retried
//! every [`RETRY_MISSING`].

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};

pub const DEBOUNCE: Duration = Duration::from_millis(400);
const POLL_EVERY: Duration = Duration::from_secs(5);
const RETRY_MISSING: Duration = Duration::from_secs(30);

/// True for event kinds that can change what the index would contain.
pub fn is_relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

/// True when a changed path could affect the index: a `.desktop` file or a directory
/// (no extension), not editor swap files and the like.
pub fn is_relevant_path(path: &Path) -> bool {
    match path.extension() {
        None => true,
        Some(e) => e == "desktop",
    }
}

/// Starts the watcher thread. `on_change` runs on it once per debounced burst (do the
/// rescan there and forward the result into the event loop).
pub fn start(dirs: Vec<PathBuf>, debounce: Duration, on_change: impl Fn() + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("launcher-watch".into())
        .spawn(move || run(dirs, debounce, &on_change));
    if let Err(err) = spawned {
        tracing::warn!("launcher: cannot start the directory watcher: {err}");
    }
}

fn run(dirs: Vec<PathBuf>, debounce: Duration, on_change: &dyn Fn()) {
    let (tx, rx) = mpsc::channel::<()>();
    let handler = move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && is_relevant(&ev.kind)
            && (ev.paths.is_empty() || ev.paths.iter().any(|p| is_relevant_path(p)))
        {
            let _ = tx.send(());
        }
    };
    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(w) => w,
        Err(err) => {
            tracing::warn!("launcher: inotify unavailable ({err}), polling instead");
            poll(&dirs, on_change);
            return;
        }
    };
    let mut watched: Vec<&PathBuf> = Vec::new();
    let mut first_pass = true;
    loop {
        let mut missing = false;
        let mut appeared = false;
        for d in &dirs {
            if watched.contains(&d) {
                continue;
            }
            if d.is_dir() && watcher.watch(d, RecursiveMode::Recursive).is_ok() {
                watched.push(d);
                appeared = true;
            } else {
                missing = true;
            }
        }
        // A directory that showed up after startup may already hold entries.
        if appeared && !first_pass {
            on_change();
        }
        first_pass = false;
        let next = if missing {
            rx.recv_timeout(RETRY_MISSING)
        } else {
            rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        match next {
            Ok(()) => {
                // Wait for the burst to end.
                while rx.recv_timeout(debounce).is_ok() {}
                on_change();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn poll(dirs: &[PathBuf], on_change: &dyn Fn()) {
    let mut last = signature(dirs);
    loop {
        std::thread::sleep(POLL_EVERY);
        let now = signature(dirs);
        if now != last {
            // Let a burst settle, then report once.
            std::thread::sleep(DEBOUNCE);
            last = signature(dirs);
            on_change();
        }
    }
}

/// Hash of every `.desktop` path, size and mtime below `dirs`.
pub fn signature(dirs: &[PathBuf]) -> u64 {
    let mut h = DefaultHasher::new();
    for d in dirs {
        hash_dir(d, 0, &mut h);
    }
    h.finish()
}

fn hash_dir(dir: &Path, depth: usize, h: &mut DefaultHasher) {
    if depth > 4 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = read.flatten().map(|e| e.path()).collect();
    items.sort();
    for p in items {
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            hash_dir(&p, depth + 1, h);
        } else if p.extension().is_some_and(|e| e == "desktop") {
            p.hash(h);
            meta.len().hash(h);
            meta.modified().ok().hash(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, ModifyKind};

    #[test]
    fn only_content_changes_are_relevant() {
        assert!(is_relevant(&EventKind::Create(CreateKind::File)));
        assert!(is_relevant(&EventKind::Modify(ModifyKind::Any)));
        assert!(!is_relevant(&EventKind::Access(AccessKind::Any)));
    }

    #[test]
    fn paths_filter_out_noise() {
        assert!(is_relevant_path(Path::new("/a/b/firefox.desktop")));
        assert!(is_relevant_path(Path::new("/a/b/subdir")));
        assert!(!is_relevant_path(Path::new("/a/b/.firefox.desktop.swp")));
        assert!(!is_relevant_path(Path::new("/a/b/mimeinfo.cache")));
    }

    #[test]
    fn signature_changes_with_files() {
        let dir = std::env::temp_dir().join(format!("aurora-launcher-sig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = vec![dir.clone()];
        let empty = signature(&dirs);
        assert_eq!(empty, signature(&dirs));
        std::fs::write(dir.join("a.desktop"), "x").unwrap();
        let one = signature(&dirs);
        assert_ne!(empty, one);
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        assert_eq!(one, signature(&dirs));
        std::fs::write(dir.join("a.desktop"), "xx").unwrap();
        assert_ne!(one, signature(&dirs));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
