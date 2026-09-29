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
        arm_exit_deadline();
        state.loop_signal.stop();
        TimeoutAction::Drop
    });
    if let Err(err) = result {
        tracing::error!(%err, "failed to arm the timeout timer");
    }
}

/// SIGINT/SIGTERM/SIGHUP stop the loop so drop guards restore the TTY; SIGUSR1 reloads the
/// config and SIGUSR2 logs a state dump.
pub fn insert_signals(handle: &LoopHandle<'static, Aurora>) {
    let signals = match Signals::new(&[
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGHUP,
        Signal::SIGUSR1,
        Signal::SIGUSR2,
    ]) {
        Ok(signals) => signals,
        Err(err) => return tracing::error!(%err, "failed to set up signal handling"),
    };
    let result = handle.insert_source(signals, |event, _, state| {
        if event.signal() == Signal::SIGUSR2 {
            return state.dump_state();
        }
        if event.signal() == Signal::SIGUSR1 {
            tracing::info!("SIGUSR1: reloading config");
            return state.reload_config();
        }
        tracing::warn!(signal = ?event.signal(), "quitting: signal received");
        arm_exit_deadline();
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
            // The log writers may be the very thing that is wedged, so the line goes out on a
            // helper thread and the exit does not wait for it for long.
            let (done, logged) = std::sync::mpsc::channel();
            let _ = std::thread::Builder::new()
                .name("watchdog-log".into())
                .spawn(move || {
                    tracing::error!("watchdog: event loop did not stop after --timeout, hard exit");
                    let _ = done.send(());
                });
            let _ = logged.recv_timeout(Duration::from_millis(500));
            // Safety: _exit is async-signal-safe and skips destructors on purpose.
            unsafe { libc::_exit(1) }
        });
    if let Err(err) = spawned {
        tracing::error!(%err, "failed to start the watchdog thread");
    }
}

/// Bound on teardown once shutdown has begun: dropping the DRM outputs, renderer and seat
/// calls into the driver while master is still held, and that can hang.
const EXIT_DEADLINE: Duration = Duration::from_secs(3);

/// Hard-exits the process after a few seconds, whatever teardown is doing. Idempotent, and
/// a plain thread so it works from the panic hook and does not depend on the event loop.
/// Exiting closes the seat and DRM fds, so the kernel drops master and the VT comes back.
pub fn arm_exit_deadline() {
    static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if ARMED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("exit-deadline".into())
        .spawn(|| {
            std::thread::sleep(EXIT_DEADLINE);
            // Safety: _exit skips destructors on purpose; nothing here can block.
            unsafe { libc::_exit(1) }
        });
    if let Err(err) = spawned {
        tracing::error!(%err, "failed to start the exit deadline thread");
    }
}
