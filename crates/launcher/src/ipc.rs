//! The launcher's connection to the compositor IPC.
//!
//! A reader thread keeps the connection alive (reconnecting with backoff), subscribes to
//! theme and focus, and forwards what it learns as [`IpcMsg`]; the main thread writes
//! `Spawn` requests straight to a cloned stream (tiny frames, write timeout) and learns
//! the outcome from [`IpcMsg::Reply`]. When the socket is unreachable [`IpcLink::spawn`]
//! returns `None` and the caller launches the app itself.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aurora_ipc::{
    Body, Event, Frame, Request, Response, ThemeSnapshot, Topic, WindowId, check_first_frame,
    read_frame, socket_path, write_frame,
};

/// What the IPC thread reports to the event loop.
#[derive(Debug)]
pub enum IpcMsg {
    Connected,
    Disconnected,
    Theme(ThemeSnapshot),
    /// The focused window changed (or the initial snapshot arrived).
    Focus(Option<WindowId>),
    /// The compositor answered request `id`.
    Reply {
        id: u64,
        ok: bool,
        message: String,
    },
}

/// Request id 1 is the initial `GetTheme`; spawns count up from 2.
const FIRST_SPAWN_ID: u64 = 2;
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct IpcLink {
    writer: Arc<Mutex<Option<UnixStream>>>,
    next_id: Arc<AtomicU64>,
}

impl IpcLink {
    /// Starts the connection thread. `notify` runs on that thread for every message.
    pub fn start(notify: impl Fn(IpcMsg) + Send + 'static) -> Self {
        let link = Self {
            writer: Arc::new(Mutex::new(None)),
            next_id: Arc::new(AtomicU64::new(FIRST_SPAWN_ID)),
        };
        let writer = link.writer.clone();
        let spawned = std::thread::Builder::new()
            .name("launcher-ipc".into())
            .spawn(move || connection_loop(&writer, &notify));
        if let Err(err) = spawned {
            tracing::warn!("launcher: cannot start the ipc thread: {err}");
        }
        link
    }

    /// Sends a `Spawn` request. `Some(id)` when it was written (the outcome arrives as
    /// [`IpcMsg::Reply`]); `None` when there is no usable connection.
    pub fn spawn(&self, argv: Vec<String>) -> Option<u64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.writer.lock().ok()?;
        let stream = guard.as_mut()?;
        match write_frame(stream, &Frame::request(id, Request::Spawn { argv })) {
            Ok(()) => Some(id),
            Err(err) => {
                tracing::warn!("launcher: ipc write failed: {err}");
                *guard = None;
                None
            }
        }
    }

    pub fn connected(&self) -> bool {
        self.writer.lock().is_ok_and(|g| g.is_some())
    }
}

fn connection_loop(writer: &Mutex<Option<UnixStream>>, notify: &dyn Fn(IpcMsg)) {
    let Some(path) = socket_path() else {
        tracing::info!("launcher: no ipc socket configured, launching directly");
        return;
    };
    let mut delay = Duration::from_millis(250);
    loop {
        if let Ok(stream) = UnixStream::connect(&path) {
            delay = Duration::from_millis(250);
            match session(stream, writer, notify) {
                Ok(()) => tracing::info!("launcher: ipc connection closed"),
                Err(err) => tracing::warn!("launcher: ipc connection lost: {err}"),
            }
            if let Ok(mut w) = writer.lock() {
                *w = None;
            }
            notify(IpcMsg::Disconnected);
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

fn session(
    mut stream: UnixStream,
    writer: &Mutex<Option<UnixStream>>,
    notify: &dyn Fn(IpcMsg),
) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| e.to_string();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| err(&e))?;
    write_frame(&mut stream, &Frame::hello("launcher")).map_err(|e| err(&e))?;
    let first = read_frame(&mut stream)
        .map_err(|e| err(&e))?
        .ok_or("closed during the handshake")?;
    check_first_frame(&first).map_err(|e| err(&e))?;
    stream.set_read_timeout(None).map_err(|e| err(&e))?;

    let mut out = stream.try_clone().map_err(|e| err(&e))?;
    out.set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|e| err(&e))?;
    write_frame(
        &mut out,
        &Frame::new(0, Body::Subscribe(vec![Topic::Theme, Topic::Focus])),
    )
    .map_err(|e| err(&e))?;
    write_frame(&mut out, &Frame::request(1, Request::GetTheme)).map_err(|e| err(&e))?;
    if let Ok(mut w) = writer.lock() {
        *w = Some(out);
    }
    notify(IpcMsg::Connected);

    while let Some(frame) = read_frame(&mut stream).map_err(|e| err(&e))? {
        match frame.body {
            Body::Event(Event::Theme(t)) | Body::Response(Response::Theme(t)) => {
                notify(IpcMsg::Theme(t));
            }
            Body::Event(Event::FocusChanged { window, .. }) => notify(IpcMsg::Focus(window)),
            Body::Event(Event::Snapshot(s)) => notify(IpcMsg::Focus(s.focused_window)),
            Body::Response(_) if frame.id >= FIRST_SPAWN_ID => notify(IpcMsg::Reply {
                id: frame.id,
                ok: true,
                message: String::new(),
            }),
            Body::Error(e) if frame.id >= FIRST_SPAWN_ID => notify(IpcMsg::Reply {
                id: frame.id,
                ok: false,
                message: format!("{:?}: {}", e.code, e.message),
            }),
            _ => {}
        }
    }
    Ok(())
}
