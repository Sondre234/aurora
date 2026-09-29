use std::time::Duration;

use smithay::reexports::calloop::{
    LoopHandle,
    signals::{Signal, Signals},
    timer::{TimeoutAction, Timer},
};

use crate::state::Aurora;

const WARN_BEFORE: Duration = Duration::from_secs(10);

/// Stops the event loop after `dur`, with a warning shortly before.
pub fn insert_timeout(handle: &LoopHandle<'static, Aurora>, dur: Duration) {
    if let Some(warn_at) = dur.checked_sub(WARN_BEFORE).filter(|d| !d.is_zero()) {
        let _ = handle.insert_source(Timer::from_duration(warn_at), |_, _, _| {
            tracing::warn!("timeout in {}s", WARN_BEFORE.as_secs());
            TimeoutAction::Drop
        });
    }
    let result = handle.insert_source(Timer::from_duration(dur), |_, _, state| {
        tracing::warn!("quitting: --timeout elapsed");
        state.loop_signal.stop();
        TimeoutAction::Drop
    });
    if let Err(err) = result {
        tracing::error!(%err, "failed to arm the timeout timer");
    }
}

/// SIGINT/SIGTERM/SIGHUP stop the loop so drop guards restore the TTY.
pub fn insert_signals(handle: &LoopHandle<'static, Aurora>) {
    let signals = match Signals::new(&[Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP]) {
        Ok(signals) => signals,
        Err(err) => return tracing::error!(%err, "failed to set up signal handling"),
    };
    let result = handle.insert_source(signals, |event, _, state| {
        tracing::warn!(signal = ?event.signal(), "quitting: signal received");
        state.loop_signal.stop();
    });
    if let Err(err) = result {
        tracing::error!(%err, "failed to register signal source");
    }
}

/// Grace period between the timeout timer and the watchdog.
const WATCHDOG_GRACE: Duration = Duration::from_secs(5);

/// Last resort if the event loop is wedged and cannot honour `--timeout`: a plain thread
/// that hard-exits the process. It touches no DRM state; exiting closes the seat fds, so
/// logind/seatd hands the VT back.
pub fn spawn_watchdog(timeout: Duration) {
    let wait = timeout + WATCHDOG_GRACE;
    let spawned = std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            std::thread::sleep(wait);
            tracing::error!("watchdog: event loop did not stop after --timeout, hard exit");
            // Safety: _exit is async-signal-safe and skips destructors on purpose.
            unsafe { libc::_exit(1) }
        });
    if let Err(err) = spawned {
        tracing::error!(%err, "failed to start the watchdog thread");
    }
}
