//! Watching the current directory with inotify (through `notify`), debounced: a burst of
//! events (an archive unpacking, our own copy) becomes one refresh after a quiet moment.
//! Our own reads (access events) never trigger a refresh.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};

pub const DEBOUNCE: Duration = Duration::from_millis(150);

enum Cmd {
    Watch(PathBuf),
    Event,
}

/// Handle to the watcher thread.
pub struct DirWatcher {
    tx: mpsc::Sender<Cmd>,
}

impl DirWatcher {
    /// `on_change` runs on the watcher thread once per debounced burst.
    pub fn start(on_change: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let handler_tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("files-watch".into())
            .spawn(move || run(rx, handler_tx, &on_change));
        if let Err(err) = spawned {
            tracing::warn!("files: cannot start the directory watcher: {err}");
        }
        Self { tx }
    }

    /// Watch `path` instead of whatever was watched before.
    pub fn watch(&self, path: PathBuf) {
        let _ = self.tx.send(Cmd::Watch(path));
    }
}

/// True for event kinds that change what a listing would show.
pub fn is_relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

fn run(rx: mpsc::Receiver<Cmd>, events: mpsc::Sender<Cmd>, on_change: &dyn Fn()) {
    let handler = move |res: notify::Result<notify::Event>| {
        if res.is_ok_and(|ev| is_relevant(&ev.kind)) {
            let _ = events.send(Cmd::Event);
        }
    };
    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(w) => w,
        Err(err) => {
            tracing::warn!("files: inotify unavailable, no automatic refresh: {err}");
            return;
        }
    };
    let mut current: Option<PathBuf> = None;
    let mut apply = |watcher: &mut notify::RecommendedWatcher, path: PathBuf| {
        if let Some(old) = current.take() {
            let _ = watcher.unwatch(&old);
        }
        match watcher.watch(&path, RecursiveMode::NonRecursive) {
            Ok(()) => current = Some(path),
            Err(err) => tracing::debug!("files: cannot watch {}: {err}", path.display()),
        }
    };
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Watch(path) => apply(&mut watcher, path),
            Cmd::Event => {
                loop {
                    match rx.recv_timeout(DEBOUNCE) {
                        Ok(Cmd::Event) => {}
                        Ok(Cmd::Watch(path)) => apply(&mut watcher, path),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                on_change();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind};

    #[test]
    fn only_content_changes_count() {
        assert!(is_relevant(&EventKind::Create(CreateKind::File)));
        assert!(is_relevant(&EventKind::Remove(
            notify::event::RemoveKind::Any
        )));
        assert!(!is_relevant(&EventKind::Access(AccessKind::Any)));
    }

    #[test]
    fn a_change_in_a_temp_dir_is_reported_once_per_burst() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let w = DirWatcher::start(move || {
            let _ = tx.send(());
        });
        w.watch(dir.path().to_path_buf());
        std::thread::sleep(Duration::from_millis(200));
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}")), "").unwrap();
        }
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "one burst, one callback"
        );
    }
}
