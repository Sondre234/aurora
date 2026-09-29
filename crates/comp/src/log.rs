use std::{fs, path::PathBuf, sync::Mutex};

use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

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

/// Logs to stderr and to the log file, so a black screen can be diagnosed afterwards.
pub fn init() {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr = tracing_subscriber::fmt::layer().with_filter(filter());

    // Unbuffered `File` writes: every event reaches the kernel before we can crash or hang.
    let file = open_log_file();
    let file_layer = file
        .as_ref()
        .ok()
        .and_then(|(f, _)| f.try_clone().ok())
        .map(|f| {
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Mutex::new(f))
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
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("aurora panicked: {info}\n{backtrace}");
        previous(info);
    }));
}
