//! The optional link to the compositor: live theme and the active output.
//!
//! One background thread blocks in `read` on the IPC socket (zero wakeups while nothing
//! changes), speaks the `aurora-ipc` handshake, subscribes to Theme and Focus, asks for
//! the theme once and forwards changes through `send`. When the socket is missing or the
//! compositor goes away it reconnects with [`Backoff`]. notifd works without any of this
//! (default theme, compositor-chosen output).

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use aurora_ipc::{
    Body, Event, Frame, FrameError, Request, Response, Snapshot, ThemeSnapshot, Topic,
    check_first_frame, read_frame, socket_path, write_frame,
};

/// What the main loop learns from the compositor.
#[derive(Clone, Debug, PartialEq)]
pub enum IpcMsg {
    Theme(Box<ThemeSnapshot>),
    /// Name of the output with focus (`None` when unknown).
    ActiveOutput(Option<String>),
}

/// Exponential reconnect delay: 500 ms doubling to 10 s, reset after a good connection.
#[derive(Debug, Clone)]
pub struct Backoff {
    next_ms: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next_ms: Self::MIN }
    }
}

impl Backoff {
    const MIN: u64 = 500;
    const MAX: u64 = 10_000;

    pub fn next_delay(&mut self) -> Duration {
        let d = self.next_ms;
        self.next_ms = (self.next_ms * 2).min(Self::MAX);
        Duration::from_millis(d)
    }

    pub fn reset(&mut self) {
        self.next_ms = Self::MIN;
    }
}

/// Folds frames of one connection into [`IpcMsg`]s. Pure, so it is tested without a socket.
#[derive(Debug, Default)]
pub struct Tracker {
    snapshot: Snapshot,
    last_rev: Option<u64>,
    last_output: Option<Option<String>>,
}

/// Request id used for the one `GetTheme`.
pub const THEME_REQUEST: u64 = 1;

impl Tracker {
    pub fn on_frame(&mut self, frame: Frame) -> Vec<IpcMsg> {
        let mut out = Vec::new();
        match frame.body {
            Body::Event(ev) => {
                if let Event::Theme(t) = &ev {
                    self.theme(t.clone(), &mut out);
                }
                self.snapshot.apply(&ev);
                let active = self.snapshot.active_output.clone();
                if self.last_output.as_ref() != Some(&active) {
                    self.last_output = Some(active.clone());
                    out.push(IpcMsg::ActiveOutput(active));
                }
            }
            Body::Response(Response::Theme(t)) if frame.id == THEME_REQUEST => {
                self.theme(t, &mut out);
            }
            _ => {}
        }
        out
    }

    fn theme(&mut self, t: ThemeSnapshot, out: &mut Vec<IpcMsg>) {
        // Drop a duplicate of what we have; revisions are per compositor run and this
        // tracker lives for one connection only.
        if self.last_rev != Some(t.rev) {
            self.last_rev = Some(t.rev);
            out.push(IpcMsg::Theme(Box::new(t)));
        }
    }
}

enum End {
    /// The peer is gone or misbehaved; retry.
    Lost(String),
}

fn session(path: &Path, send: &dyn Fn(IpcMsg), connected: &mut bool) -> Result<(), End> {
    let lost = |what: &str, e: &dyn std::fmt::Display| End::Lost(format!("{what}: {e}"));
    let mut sock = UnixStream::connect(path).map_err(|e| lost("connect", &e))?;
    write_frame(&mut sock, &Frame::hello("notifd")).map_err(|e| lost("hello", &e))?;
    match read_frame(&mut sock).map_err(|e| lost("hello reply", &e))? {
        Some(f) => {
            check_first_frame(&f).map_err(|e| lost("handshake", &e))?;
        }
        None => return Err(End::Lost("closed during handshake".into())),
    }
    write_frame(
        &mut sock,
        &Frame::new(0, Body::Subscribe(vec![Topic::Theme, Topic::Focus])),
    )
    .map_err(|e| lost("subscribe", &e))?;
    write_frame(&mut sock, &Frame::request(THEME_REQUEST, Request::GetTheme))
        .map_err(|e| lost("get theme", &e))?;
    *connected = true;
    tracing::info!("notifd: ipc connected path={}", path.display());
    let mut tracker = Tracker::default();
    loop {
        match read_frame(&mut sock) {
            Ok(Some(frame)) => tracker.on_frame(frame).into_iter().for_each(send),
            Ok(None) => return Err(End::Lost("compositor closed the connection".into())),
            // One undecodable frame (newer protocol) does not desynchronise the stream.
            Err(FrameError::Decode(e)) => tracing::debug!("notifd: ipc skipped a frame: {e}"),
            Err(e) => return Err(lost("read", &e)),
        }
    }
}

/// Starts the IPC thread. Does nothing when no socket path can be derived.
pub fn spawn(send: impl Fn(IpcMsg) + Send + 'static) {
    let Some(path) = socket_path() else {
        tracing::info!("notifd: ipc disabled (no AURORA_IPC_SOCK or XDG_RUNTIME_DIR)");
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("notifd-ipc".into())
        .spawn(move || {
            let mut backoff = Backoff::default();
            let mut warned = false;
            loop {
                let mut connected = false;
                let result = session(&path, &send, &mut connected);
                if connected {
                    // A connection that worked: start the delays over and log the next loss.
                    backoff.reset();
                    warned = false;
                }
                let End::Lost(why) = match result {
                    Ok(()) => End::Lost("ended".into()),
                    Err(e) => e,
                };
                // Quiet while the compositor is simply not there.
                if !warned {
                    tracing::info!("notifd: ipc unavailable ({why}), retrying");
                    warned = true;
                }
                std::thread::sleep(backoff.next_delay());
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("notifd: cannot start ipc thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_ipc::{ErrorCode, Theme};

    fn theme_event(rev: u64) -> Frame {
        Frame::event(Event::Theme(ThemeSnapshot::new(rev, Theme::default())))
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = Backoff::default();
        let ms: Vec<u128> = (0..7).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(ms, vec![500, 1000, 2000, 4000, 8000, 10_000, 10_000]);
        b.reset();
        assert_eq!(b.next_delay().as_millis(), 500);
    }

    #[test]
    fn theme_response_and_events_forward_once_per_revision() {
        let mut t = Tracker::default();
        let resp = Frame::new(
            THEME_REQUEST,
            Body::Response(Response::Theme(ThemeSnapshot::new(3, Theme::default()))),
        );
        let out = t.on_frame(resp);
        assert!(matches!(&out[..], [IpcMsg::Theme(s)] if s.rev == 3));
        // Same revision again (the event after our request): dropped.
        assert!(
            t.on_frame(theme_event(3))
                .iter()
                .all(|m| !matches!(m, IpcMsg::Theme(_)))
        );
        let out = t.on_frame(theme_event(4));
        assert!(
            out.iter()
                .any(|m| matches!(m, IpcMsg::Theme(s) if s.rev == 4))
        );
    }

    #[test]
    fn other_request_ids_and_errors_are_ignored() {
        let mut t = Tracker::default();
        let stray = Frame::new(
            9,
            Body::Response(Response::Theme(ThemeSnapshot::new(1, Theme::default()))),
        );
        assert!(t.on_frame(stray).is_empty());
        assert!(
            t.on_frame(Frame::error(1, ErrorCode::Denied, "no"))
                .is_empty()
        );
    }

    #[test]
    fn active_output_is_reported_on_change_only() {
        let mut t = Tracker::default();
        let focus = |o: Option<&str>| {
            Frame::event(Event::FocusChanged {
                window: None,
                output: o.map(String::from),
            })
        };
        // First event establishes the state, including "unknown".
        assert_eq!(t.on_frame(focus(None)), vec![IpcMsg::ActiveOutput(None)]);
        assert_eq!(
            t.on_frame(focus(Some("DP-1"))),
            vec![IpcMsg::ActiveOutput(Some("DP-1".into()))]
        );
        assert!(t.on_frame(focus(Some("DP-1"))).is_empty());
        assert_eq!(
            t.on_frame(focus(Some("DP-2"))),
            vec![IpcMsg::ActiveOutput(Some("DP-2".into()))]
        );
    }

    #[test]
    fn snapshot_carries_the_active_output() {
        let mut t = Tracker::default();
        let snap = Snapshot {
            active_output: Some("HDMI-A-1".into()),
            ..Snapshot::default()
        };
        let out = t.on_frame(Frame::event(Event::Snapshot(snap)));
        assert_eq!(out, vec![IpcMsg::ActiveOutput(Some("HDMI-A-1".into()))]);
    }
}
