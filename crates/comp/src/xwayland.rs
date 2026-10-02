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

/// Failed starts allowed inside `RESTART_WINDOW` before the server stays down.
const MAX_RESTARTS: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(60);

pub struct Covering {
    pub window: Window,
    pub loc: Point<i32, Logical>,
    pub ws: u32,
    pub shown: bool,
    /// Covers a whole output (a game): it takes the keyboard when it comes back.
    pub takes_keyboard: bool,
}

#[derive(Default)]
pub struct XWaylandState {
    /// The server's event source. Removing it drops the `XWayland`, which disconnects the
    /// server's wayland client and releases the display lock and sockets.
    token: Option<RegistrationToken>,
    /// Display number of the last server; a restart reuses it.
    pub display: Option<u32>,
    /// Only moved out from the teardown idle callback (see `XwmHandler::xwm_state`).
    pub wm: Option<X11Wm>,
    /// The last manager whose server is gone. Selection transfers registered their own fd
    /// sources with it, and those keep calling `xwm_state` until they notice the dead
    /// connection, so it lives until the state drops.
    pub retired: Option<X11Wm>,
    /// Override-redirect windows (menus, tooltips, some games): never tiled, placed by the
    /// client. Mapped outputs mirror the main space so frame callbacks reach them.
    pub unmanaged: Space<Window>,
    /// Every override-redirect window with the workspace it belongs to: it leaves the
    /// unmanaged space while that workspace is hidden.
    pub covering: Vec<Covering>,
    /// `xwayland.scale` as of the current server's start. The window manager reads the client
    /// scale once when it starts, so it must not change under a running server.
    scale: f64,
    restarts: VecDeque<Instant>,
    /// The current server's window manager came up. `-terminate` makes a server that served
    /// its clients exit normally, which is not a crash however short its life.
    ready: bool,
    down: bool,
}

impl Aurora {
    /// Starts the server. Failing to is not fatal: the session just has no X11.
    pub fn start_xwayland(&mut self) {
        if self.xwayland.down || self.xwayland.token.is_some() {
            return;
        }
        let settings = self.config.xwayland.clone();
        self.xwayland.scale = settings.scale;
        // Toolkits that read the server's resolution grow with the scale; at 1 the server
        // gets no extra arguments at all.
        let args: Vec<String> = settings
            .dpi()
            .into_iter()
            .flat_map(|dpi| ["-dpi".to_owned(), dpi.to_string()])
            .collect();
        let spawned = XWayland::spawn(
            &self.display_handle,
            self.xwayland.display.or_else(free_display),
            std::iter::empty::<(String, String)>(),
            args,
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
        self.xwayland.ready = false;
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
        // Smithay maps between the X client's pixels and our logical coordinates by this
        // client scale (geometry, sizes, pointer, xdg-output), so X windows get `scale` X
        // pixels per logical pixel and render 1:1 at the matching output scale. It must be
        // set before the window manager starts, which reads it once. Scale 1 is the default
        // identity mapping.
        let scale = self.xwayland.scale;
        self.client_compositor_state(&client)
            .set_client_scale(scale);
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
        self.xwayland.ready = true;
        self.xwayland.restarts.clear();
        tracing::info!("xwayland: ready display=:{number}");
        // DISPLAY may be new to activated services if the first server never came up.
        self.import_session_env();
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
        if let Some(old) = self.xwayland.wm.take() {
            self.xwayland.retired = Some(old);
        }
        if let Some(token) = self.xwayland.token.take() {
            self.handle.remove(token);
        }
        self.x11_forget_all();
        tracing::info!("xwayland: exited");
        if !restart {
            return;
        }
        let now = Instant::now();
        if !self.xwayland.ready {
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
