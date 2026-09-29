use std::{
    fs, io,
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use tracing::{Level, Metadata};
use tracing_subscriber::{
    EnvFilter, Layer, fmt::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt,
};

/// `$XDG_STATE_HOME/aurora/comp.log`, falling back to `~/.local/state`.
fn log_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("aurora/comp.log"))
}

/// Opens the log file, keeping the previous run as `comp.log.1`.
fn open_log_file() -> Result<(fs::File, PathBuf), String> {
    let path = log_path().ok_or("neither XDG_STATE_HOME nor HOME is set")?;
    let dir = path.parent().ok_or("log path has no parent directory")?;
    fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    if path.exists() {
        let _ = fs::rename(&path, dir.join("comp.log.1"));
    }
    let file =
        fs::File::create(&path).map_err(|err| format!("create {}: {err}", path.display()))?;
    Ok((file, path))
}

/// Log file writer that syncs to disk after every ERROR event. Errors are rare and fatal-ish
/// enough to afford the sync; warnings and below can repeat per frame, and a sync on the
/// compositor thread would cost milliseconds each. Unbuffered writes only survive a process
/// crash; the
/// sync is what keeps the tail across a hard reset after a black screen.
struct SyncedFile(Mutex<fs::File>);

struct SyncedWriter<'a> {
    file: MutexGuard<'a, fs::File>,
    sync: bool,
}

impl SyncedFile {
    fn writer(&self, sync: bool) -> SyncedWriter<'_> {
        SyncedWriter {
            file: self.0.lock().unwrap_or_else(|e| e.into_inner()),
            sync,
        }
    }
}

impl<'a> MakeWriter<'a> for SyncedFile {
    type Writer = SyncedWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        self.writer(false)
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        self.writer(*meta.level() == Level::ERROR)
    }
}

impl io::Write for SyncedWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for SyncedWriter<'_> {
    fn drop(&mut self) {
        if self.sync {
            let _ = self.file.sync_data();
        }
    }
}

/// Logs to stderr and to the log file, so a black screen can be diagnosed afterwards.
pub fn init() {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr = tracing_subscriber::fmt::layer().with_filter(filter());

    // Unbuffered `File` writes, synced for ERROR (see `SyncedFile`).
    let file = open_log_file();
    let file_layer = file
        .as_ref()
        .ok()
        .and_then(|(f, _)| f.try_clone().ok())
        .map(|f| {
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(SyncedFile(Mutex::new(f)))
                .with_filter(filter())
        });

    tracing_subscriber::registry()
        .with(stderr)
        .with(file_layer)
        .init();

    match file {
        Ok((_, path)) => tracing::info!(path = %path.display(), "logging to file"),
        Err(err) => tracing::warn!(%err, "file logging disabled, stderr only"),
    }
}

/// Logs panics (with a backtrace) before the default hook runs; unwinding stays enabled.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::safety::arm_exit_deadline();
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("aurora panicked: {info}\n{backtrace}");
        previous(info);
    }));
}
