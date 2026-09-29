//! XWayland lifecycle: spawn, window manager start, respawn and shutdown. Window handling
//! lives in `handlers/xwm.rs` and `wm/x11.rs`.
//!
//! Smithay hardcodes `-terminate`, so the server exits when its last X client goes away. We
//! start it again on the same display number, which keeps `DISPLAY` in every child valid.
use std::{
    collections::VecDeque,
    fs,
    os::unix::net::UnixStream,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use smithay::{
    desktop::{Space, Window},
    reexports::{calloop::RegistrationToken, wayland_server::Client},
    utils::{Logical, Point, Size},
    wayland::compositor::CompositorHandler,
    xwayland::{X11Wm, XWayland, XWaylandEvent},
};
use xcursor::{CursorTheme, parser::parse_xcursor};

use crate::state::Aurora;

/// Restarts allowed inside `RESTART_WINDOW` before the server stays down.
const MAX_RESTARTS: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// A server that lived at least this long exited because its clients left, not because it
/// crashed, so its restart does not count against the limit.
const HEALTHY_UPTIME: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct XWaylandState {
    /// The server's event source. Removing it drops the `XWayland`, which disconnects the
    /// server's wayland client and releases the display lock and sockets.
    token: Option<RegistrationToken>,
    /// Display number of the last server; a restart reuses it.
    pub display: Option<u32>,
    /// Only cleared from the teardown idle callback (see `XwmHandler::xwm_state`).
    pub wm: Option<X11Wm>,
    /// Override-redirect windows (menus, tooltips, some games): never tiled, placed by the
    /// client. Mapped outputs mirror the main space so frame callbacks reach them.
    pub unmanaged: Space<Window>,
    restarts: VecDeque<Instant>,
    started: Option<Instant>,
    down: bool,
}

impl Aurora {
    /// Starts the server. Failing to is not fatal: the session just has no X11.
    pub fn start_xwayland(&mut self) {
        if self.xwayland.down || self.xwayland.token.is_some() {
            return;
        }
        let spawned = XWayland::spawn(
            &self.display_handle,
            self.xwayland.display.or_else(free_display),
            std::iter::empty::<(String, String)>(),
            std::iter::empty::<String>(),
            true,
            Stdio::null(),
            Stdio::null(),
            |_| (),
        );
        let (server, client) = match spawned {
            Ok(pair) => pair,
            Err(err) => return tracing::warn!("xwayland: cannot start: {err}"),
        };
        self.xwayland.display = Some(server.display_number());
        self.xwayland.started = Some(Instant::now());
        let inserted = self
            .handle
            .insert_source(server, move |event, _, state| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => state.xwayland_ready(x11_socket, display_number, client.clone()),
                XWaylandEvent::Error => {
                    tracing::warn!("xwayland: exited during startup");
                    state.queue_xwayland_restart();
                }
            });
        match inserted {
            Ok(token) => self.xwayland.token = Some(token),
            Err(err) => tracing::warn!("xwayland: cannot register the server: {err}"),
        }
    }

    fn xwayland_ready(&mut self, socket: UnixStream, number: u32, client: Client) {
        // X11 clients are not scaled: their coordinates are the global logical ones.
        self.client_compositor_state(&client).set_client_scale(1.0);
        let mut wm =
            match X11Wm::start_wm(self.handle.clone(), &self.display_handle, socket, client) {
                Ok(wm) => wm,
                Err(err) => {
                    tracing::warn!("xwayland: window manager failed to start: {err}");
                    return self.queue_xwayland_restart();
                }
            };
        // Smithay drops every X event carrying the sequence number of this request, and events
        // only carry a newer one once the manager sends another request. So the cursor, which
        // takes several, must come after it or the manager would ignore its clients forever.
        let primary = self.primary_output();
        if let Err(err) = wm.set_randr_primary_output(primary.as_ref()) {
            tracing::debug!("xwayland: cannot set the primary output: {err}");
        }
        let cursor = cursor_image().unwrap_or_else(fallback_cursor);
        if let Err(err) = wm.set_cursor(&cursor.pixels, cursor.size, cursor.hotspot) {
            tracing::warn!("xwayland: cannot set the cursor: {err}");
        }
        self.xwayland.wm = Some(wm);
        tracing::info!("xwayland: ready display=:{number}");
    }

    /// The server is gone (or never came up). Teardown waits for an idle callback so it never
    /// runs inside one of the window manager's own event callbacks.
    pub fn queue_xwayland_restart(&mut self) {
        self.handle
            .insert_idle(|state| state.xwayland_teardown(true));
    }

    /// Drops everything tied to the server. `restart` starts a new one unless it keeps
    /// crashing.
    fn xwayland_teardown(&mut self, restart: bool) {
        // Window manager first: its X windows are gone with the server anyway.
        self.xwayland.wm = None;
        if let Some(token) = self.xwayland.token.take() {
            self.handle.remove(token);
        }
        self.x11_forget_all();
        tracing::info!("xwayland: exited");
        if !restart {
            return;
        }
        let now = Instant::now();
        if self
            .xwayland
            .started
            .is_none_or(|s| now.duration_since(s) < HEALTHY_UPTIME)
        {
            self.xwayland.restarts.push_back(now);
        }
        while self
            .xwayland
            .restarts
            .front()
            .is_some_and(|t| now.duration_since(*t) > RESTART_WINDOW)
        {
            self.xwayland.restarts.pop_front();
        }
        if self.xwayland.restarts.len() >= MAX_RESTARTS {
            self.xwayland.down = true;
            return tracing::warn!(
                "xwayland: exited {MAX_RESTARTS} times in {}s, staying down",
                RESTART_WINDOW.as_secs()
            );
        }
        self.start_xwayland();
    }

    /// After the event loop returned and before the state drops, on every exit path.
    pub fn shutdown_xwayland(&mut self) {
        self.xwayland.down = true;
        if self.xwayland.wm.is_some() || self.xwayland.token.is_some() {
            self.xwayland_teardown(false);
        }
    }
}

/// The default cursor as the X server wants it: ARGB pixels, size and hotspot.
struct XCursor {
    pixels: Vec<u8>,
    size: Size<u16, Logical>,
    hotspot: Point<u16, Logical>,
}

/// The first display number nobody has a lock or socket for. Smithay would probe from :0
/// and trip over other sessions' leftovers; choosing here leaves their files alone.
fn free_display() -> Option<u32> {
    (0..33).find(|n| {
        !Path::new(&format!("/tmp/.X{n}-lock")).exists()
            && !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
    })
}

/// A white square, for when no cursor theme is installed.
fn fallback_cursor() -> XCursor {
    XCursor {
        pixels: vec![255; 8 * 8 * 4],
        size: (8, 8).into(),
        hotspot: (0, 0).into(),
    }
}

fn cursor_image() -> Option<XCursor> {
    let theme = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
    let target: i32 = std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|s| *s > 0)
        .unwrap_or(24);
    let theme = CursorTheme::load(&theme);
    let images = ["default", "left_ptr"].into_iter().find_map(|name| {
        let path = theme.load_icon(name)?;
        parse_xcursor(&fs::read(path).ok()?)
    })?;
    let nearest = images
        .iter()
        .min_by_key(|i| (target - i.size as i32).abs())?;
    let image = images
        .iter()
        .find(|i| i.width == nearest.width && i.height == nearest.height)?;
    let dim = |v: u32| u16::try_from(v).ok();
    Some(XCursor {
        pixels: image.pixels_rgba.clone(),
        size: (dim(image.width)?, dim(image.height)?).into(),
        hotspot: (dim(image.xhot)?, dim(image.yhot)?).into(),
    })
}
