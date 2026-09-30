//! Wayland runtime: layer-shell and ext-session-lock surface runners on a calloop loop.
//!
//! A service builds an [`App`], connects a [`Client`], creates surfaces with a [`Ui`]
//! each, adds its own calloop sources (IPC socket, D-Bus, timers) and calls
//! [`Client::run`]:
//!
//! ```ignore
//! struct Bar { /* service state */ }
//! impl App for Bar {
//!     fn event(&mut self, rt: &mut Runtime<Self>, ev: Event) { /* react */ }
//! }
//!
//! let mut client = Client::connect(TextSystem::new(), Bar { /* .. */ })?;
//! let ui = Ui::new(client.runtime().text().clone(), root_node);
//! let id = client.runtime().create_layer(
//!     LayerConfig { layer: Layer::Top, anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
//!                   size: (0, 32), exclusive_zone: 32, ..Default::default() },
//!     ui,
//! )?;
//! client.handle().insert_source(my_source, |event, _, state| {
//!     // `state.app` is your App, `state.rt` the Runtime.
//!     state.rt.ui(id).map(|ui| ui.set_text(Id(1), "updated"));
//! })?;
//! client.run()?;
//! ```
//!
//! What the runner does for you:
//!
//! - **Pacing.** After every dispatch round, each configured and visible surface whose
//!   [`Ui`] reports `needs_redraw` is painted and committed, at most one commit per
//!   `wl_surface.frame` callback. A surface that nothing touches never redraws: an idle
//!   service blocks in `epoll` with zero wakeups. To repaint after changing a `Ui`, just
//!   change it (`edit`, `set_text`, ...); [`Runtime::request_redraw`] forces a full repaint.
//! - **Buffers.** Triple-buffered `wl_shm` pool of ARGB8888, recreated only on a size or
//!   scale change. Each buffer tracks the damage it still owes, so only changed regions
//!   are repainted and `damage_buffer` reports exactly those.
//! - **Scale.** `wp_fractional_scale_v1` plus `wp_viewporter` when the compositor offers
//!   both (buffer = round(logical x scale), destination = logical size); otherwise the
//!   integer output scale through `wl_surface.set_buffer_scale`.
//! - **Input.** Pointer and keyboard (xkbcommon, with key repeat) are mapped to
//!   [`crate::Input`] and delivered to the surface that has focus; what the widgets
//!   produce arrives as [`Event::Ui`]. Keys no widget consumes (Escape, Up/Down while
//!   typing, ...) arrive as `UiEvent::Key`.
//! - **Hide/show.** [`Runtime::hide`] unmaps a layer surface while keeping its buffers,
//!   caches and `Ui` warm; [`Runtime::show`] re-maps it with its stored configuration.
//!
//! Everything runs on one thread; callbacks get `&mut State<A>`. Do not block in them.

mod convert;
mod pool;

use std::fmt;
use std::io;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle, LoopSignal};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_keyboard::WlKeyboard,
    wl_output::{self, WlOutput},
    wl_pointer::WlPointer,
    wl_seat::WlSeat,
    wl_surface::WlSurface,
};
use smithay_client_toolkit::reexports::client::{Connection, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent as SctkKey, KeyboardHandler, Modifiers, RawModifiers,
};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
    SessionLockSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};

pub use smithay_client_toolkit::reexports::calloop;
pub use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};

use crate::damage::Damage;
use crate::geom::{Point, Rect, Size};
use crate::input::{Input, Mods, UiEvent};
use crate::skia::{PaintCaches, SkiaPainter};
use crate::text::TextSystem;
use crate::ui::Ui;

use convert::{
    device_rect, device_size, finish_frame, logical_rect, map_button, map_key, map_mods,
    repaint_region,
};
use pool::{BufferData, BufferPool};

/// Failure to start or drive the runtime.
#[derive(Debug)]
pub enum Error {
    Connect(String),
    /// A required Wayland global is missing.
    Global(&'static str),
    Shm(io::Error),
    Loop(String),
    /// The operation does not apply (e.g. no lock in progress).
    State(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Connect(e) => write!(f, "wayland connection: {e}"),
            Error::Global(g) => write!(f, "compositor lacks required global {g}"),
            Error::Shm(e) => write!(f, "shm pool: {e}"),
            Error::Loop(e) => write!(f, "event loop: {e}"),
            Error::State(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Handle of a surface created through the [`Runtime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceId(u32);

/// A connected output.
#[derive(Debug, Clone)]
pub struct Output {
    /// The `wl_output` global name.
    pub id: u32,
    /// Connector-style name (`DP-1`) when the compositor reports one.
    pub name: Option<String>,
    /// Size in logical pixels.
    pub logical_size: Option<(i32, i32)>,
    /// Integer output scale.
    pub scale: i32,
    wl: WlOutput,
}

impl PartialEq for Output {
    fn eq(&self, o: &Self) -> bool {
        self.id == o.id
    }
}

/// Configuration of a layer-shell surface. `size` of 0 on an axis means "let the
/// compositor decide" (needs anchors on both edges of that axis).
#[derive(Debug, Clone)]
pub struct LayerConfig {
    pub layer: Layer,
    pub namespace: String,
    pub anchor: Anchor,
    pub size: (u32, u32),
    pub exclusive_zone: i32,
    /// top, right, bottom, left
    pub margin: (i32, i32, i32, i32),
    pub keyboard: KeyboardInteractivity,
    /// `None` lets the compositor pick the output.
    pub output: Option<Output>,
}

impl Default for LayerConfig {
    fn default() -> Self {
        Self {
            layer: Layer::Top,
            namespace: "aurora".into(),
            anchor: Anchor::empty(),
            size: (0, 0),
            exclusive_zone: 0,
            margin: (0, 0, 0, 0),
            keyboard: KeyboardInteractivity::None,
            output: None,
        }
    }
}

/// What the runtime tells the [`App`].
#[derive(Debug)]
pub enum Event {
    /// A widget of a surface's [`Ui`] produced an event.
    Ui {
        surface: SurfaceId,
        event: UiEvent,
    },
    /// The compositor sized the surface (first configure and every resize), logical px.
    Configured {
        surface: SurfaceId,
        size: Size,
    },
    /// The effective scale of a surface changed.
    ScaleChanged {
        surface: SurfaceId,
        scale: f32,
    },
    /// The compositor closed the layer surface (output gone, ...); it is already dropped.
    Closed {
        surface: SurfaceId,
    },
    OutputAdded(Output),
    OutputChanged(Output),
    OutputRemoved(Output),
    /// The compositor confirmed the session is locked.
    Locked,
    /// The lock ended: denied, replaced by another client, or unlocked.
    LockFinished,
}

/// A service implements this to receive [`Event`]s.
pub trait App: Sized + 'static {
    fn event(&mut self, rt: &mut Runtime<Self>, event: Event);
}

/// The loop data: your app next to the runtime. Custom calloop sources receive
/// `&mut State<A>`.
pub struct State<A: App> {
    pub rt: Runtime<A>,
    pub app: A,
}

enum Role {
    Layer(LayerSurface),
    Lock(#[allow(dead_code)] SessionLockSurface),
}

struct Surf {
    id: SurfaceId,
    role: Role,
    wl: WlSurface,
    ui: Ui,
    layer_cfg: Option<LayerConfig>,
    /// Logical size from the last configure.
    size: (u32, u32),
    configured: bool,
    hidden: bool,
    frame_pending: bool,
    frac: Option<WpFractionalScaleV1>,
    viewport: Option<WpViewport>,
    /// Preferred fractional scale in 120ths, once the compositor sent one.
    frac120: Option<u32>,
    int_scale: i32,
    pool: Option<BufferPool>,
}

impl Surf {
    fn scale120(&self) -> u32 {
        match (self.frac120, &self.viewport) {
            (Some(s), Some(_)) => s,
            _ => self.int_scale.max(1) as u32 * 120,
        }
    }

    fn scale(&self) -> f32 {
        self.scale120() as f32 / 120.0
    }
}

/// The Wayland side: globals, surfaces, input state and the paint caches.
pub struct Runtime<A: App> {
    conn: Connection,
    qh: QueueHandle<State<A>>,
    loop_handle: LoopHandle<'static, State<A>>,
    signal: LoopSignal,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: Option<LayerShell>,
    lock_state: SessionLockState,
    lock: Option<SessionLock>,
    viewporter: Option<WpViewporter>,
    frac_mgr: Option<WpFractionalScaleManagerV1>,
    surfs: Vec<Surf>,
    next_id: u32,
    caches: PaintCaches,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    kb_focus: Option<SurfaceId>,
    mods: Mods,
    outputs: Vec<Output>,
}

/// No-event protocol objects (viewporter, viewport, fractional-scale manager).
struct Noop;

impl<I: Proxy, S> Dispatch2<I, S> for Noop {
    fn event(&self, _: &mut S, _: &I, _: <I as Proxy>::Event, _: &Connection, _: &QueueHandle<S>) {}
}

struct FractionalData(WlSurface);

impl<A: App> Dispatch2<WpFractionalScaleV1, State<A>> for FractionalData {
    fn event(
        &self,
        state: &mut State<A>,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<A>>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.preferred_scale(&self.0, scale);
        }
    }
}

impl<A: App> Dispatch2<WlBuffer, State<A>> for BufferData {
    fn event(
        &self,
        _: &mut State<A>,
        _: &WlBuffer,
        event: wl_buffer::Event,
        _: &Connection,
        _: &QueueHandle<State<A>>,
    ) {
        if let wl_buffer::Event::Release = event {
            self.0.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

delegate_registry!(@<A: App> State<A>);
delegate_dispatch2!(@<A: App> State<A>);

/// Owns the event loop. Build with [`Client::connect`], create surfaces, add sources,
/// then [`Client::run`].
pub struct Client<A: App> {
    event_loop: EventLoop<'static, State<A>>,
    state: State<A>,
}

impl<A: App> Client<A> {
    /// Connect to `$WAYLAND_DISPLAY` and bind the globals (two roundtrips, so outputs are
    /// known when this returns). `text` should be the process-wide [`TextSystem`].
    pub fn connect(text: TextSystem, app: A) -> Result<Self, Error> {
        let conn = Connection::connect_to_env().map_err(|e| Error::Connect(e.to_string()))?;
        let (globals, mut queue) =
            registry_queue_init::<State<A>>(&conn).map_err(|e| Error::Connect(e.to_string()))?;
        let qh = queue.handle();
        let event_loop: EventLoop<'static, State<A>> =
            EventLoop::try_new().map_err(|e| Error::Loop(e.to_string()))?;
        let loop_handle = event_loop.handle();

        let compositor =
            CompositorState::bind(&globals, &qh).map_err(|_| Error::Global("wl_compositor"))?;
        let shm = Shm::bind(&globals, &qh).map_err(|_| Error::Global("wl_shm"))?;
        let layer_shell = LayerShell::bind(&globals, &qh).ok();
        let viewporter = globals.bind::<WpViewporter, _, _>(&qh, 1..=1, Noop).ok();
        let frac_mgr = globals
            .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, Noop)
            .ok();
        let rt = Runtime {
            conn: conn.clone(),
            qh: qh.clone(),
            loop_handle: loop_handle.clone(),
            signal: event_loop.get_signal(),
            registry_state: RegistryState::new(&globals),
            seat_state: SeatState::new(&globals, &qh),
            output_state: OutputState::new(&globals, &qh),
            shm,
            compositor,
            layer_shell,
            lock_state: SessionLockState::new(&globals, &qh),
            lock: None,
            viewporter,
            frac_mgr,
            surfs: Vec::new(),
            next_id: 1,
            caches: PaintCaches::new(text),
            pointer: None,
            keyboard: None,
            kb_focus: None,
            mods: Mods::default(),
            outputs: Vec::new(),
        };
        let mut state = State { rt, app };
        for _ in 0..2 {
            queue
                .roundtrip(&mut state)
                .map_err(|e| Error::Connect(e.to_string()))?;
        }
        state.rt.outputs = state.rt.collect_outputs();
        WaylandSource::new(conn, queue)
            .insert(loop_handle)
            .map_err(|e| Error::Loop(e.to_string()))?;
        Ok(Self { event_loop, state })
    }

    /// Loop handle for adding sources (sockets, timers, channels). Their callbacks get
    /// `&mut State<A>`.
    pub fn handle(&self) -> LoopHandle<'static, State<A>> {
        self.event_loop.handle()
    }

    pub fn runtime(&mut self) -> &mut Runtime<A> {
        &mut self.state.rt
    }

    pub fn app(&mut self) -> &mut A {
        &mut self.state.app
    }

    /// Run until [`Runtime::quit`]. Returns the app.
    pub fn run(self) -> Result<A, Error> {
        let Client {
            mut event_loop,
            mut state,
        } = self;
        event_loop
            .run(None, &mut state, |s| s.flush())
            .map_err(|e| Error::Loop(e.to_string()))?;
        Ok(state.app)
    }
}

impl<A: App> State<A> {
    /// Paint and commit every surface that needs it. The runner calls this after each
    /// dispatch round; call it yourself only from code that bypasses the loop.
    pub fn flush(&mut self) {
        self.rt.render_pending();
    }

    fn emit(&mut self, event: Event) {
        self.app.event(&mut self.rt, event);
    }

    fn deliver(&mut self, sid: SurfaceId, input: Input) {
        let events = match self.rt.surf_mut(sid) {
            Some(s) => s.ui.handle(input),
            None => return,
        };
        for event in events {
            self.emit(Event::Ui {
                surface: sid,
                event,
            });
        }
    }

    fn key(&mut self, ev: &SctkKey) {
        if let Some(sid) = self.rt.kb_focus {
            let k = map_key(ev, self.rt.mods);
            self.deliver(sid, Input::Key(k));
        }
    }

    fn preferred_scale(&mut self, wl: &WlSurface, scale120: u32) {
        let Some(s) = self.rt.surfs.iter_mut().find(|s| &s.wl == wl) else {
            return;
        };
        s.frac120 = Some(scale120.max(1));
        let (sid, scale) = (s.id, s.scale());
        s.ui.set_scale(scale);
        self.emit(Event::ScaleChanged {
            surface: sid,
            scale,
        });
    }

    fn configured(&mut self, wl: &WlSurface, new: (u32, u32)) {
        let Some(s) = self.rt.surfs.iter_mut().find(|s| &s.wl == wl) else {
            return;
        };
        let fallback = s.layer_cfg.as_ref().map_or((1, 1), |c| c.size);
        let size = (
            if new.0 > 0 { new.0 } else { fallback.0.max(1) },
            if new.1 > 0 { new.1 } else { fallback.1.max(1) },
        );
        s.size = size;
        s.configured = true;
        s.hidden = false;
        let logical = Size::new(size.0 as f32, size.1 as f32);
        s.ui.set_size(logical);
        s.ui.set_scale(s.scale());
        let sid = s.id;
        self.emit(Event::Configured {
            surface: sid,
            size: logical,
        });
    }
}

impl<A: App> Runtime<A> {
    /// Shared text engine; clone it into every [`Ui`].
    pub fn text(&self) -> &TextSystem {
        self.caches.text()
    }

    /// Raster cache counters (images, shadows).
    pub fn paint_caches(&self) -> &PaintCaches {
        &self.caches
    }

    /// Handle for adding calloop sources from inside callbacks.
    pub fn loop_handle(&self) -> &LoopHandle<'static, State<A>> {
        &self.loop_handle
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Stop the loop after the current dispatch round.
    pub fn quit(&self) {
        self.signal.stop();
    }

    /// Currently known outputs.
    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }

    fn collect_outputs(&self) -> Vec<Output> {
        self.output_state
            .outputs()
            .filter_map(|wl| self.output_of(&wl))
            .collect()
    }

    fn output_of(&self, wl: &WlOutput) -> Option<Output> {
        let i = self.output_state.info(wl)?;
        Some(Output {
            id: i.id,
            name: i.name,
            logical_size: i.logical_size,
            scale: i.scale_factor,
            wl: wl.clone(),
        })
    }

    fn surf(&self, id: SurfaceId) -> Option<&Surf> {
        self.surfs.iter().find(|s| s.id == id)
    }

    fn surf_mut(&mut self, id: SurfaceId) -> Option<&mut Surf> {
        self.surfs.iter_mut().find(|s| s.id == id)
    }

    fn sid_of(&self, wl: &WlSurface) -> Option<SurfaceId> {
        self.surfs.iter().find(|s| &s.wl == wl).map(|s| s.id)
    }

    /// The widget tree of a surface.
    pub fn ui(&mut self, id: SurfaceId) -> Option<&mut Ui> {
        self.surf_mut(id).map(|s| &mut s.ui)
    }

    /// Logical size from the last configure.
    pub fn size(&self, id: SurfaceId) -> Option<Size> {
        self.surf(id)
            .map(|s| Size::new(s.size.0 as f32, s.size.1 as f32))
    }

    /// Effective scale of a surface (fractional when negotiated).
    pub fn scale(&self, id: SurfaceId) -> Option<f32> {
        self.surf(id).map(Surf::scale)
    }

    /// Force a full repaint of a surface at the next opportunity.
    pub fn request_redraw(&mut self, id: SurfaceId) {
        if let Some(s) = self.surf_mut(id) {
            s.ui.invalidate_all();
        }
    }

    fn new_surface(
        &mut self,
    ) -> (
        SurfaceId,
        WlSurface,
        Option<WpFractionalScaleV1>,
        Option<WpViewport>,
    ) {
        let wl = self.compositor.create_surface(&self.qh);
        let frac = self
            .frac_mgr
            .as_ref()
            .map(|m| m.get_fractional_scale(&wl, &self.qh, FractionalData(wl.clone())));
        let viewport = self
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&wl, &self.qh, Noop));
        let id = SurfaceId(self.next_id);
        self.next_id += 1;
        (id, wl, frac, viewport)
    }

    /// Create a layer-shell surface showing `ui`. The surface is mapped once the
    /// compositor configures it ([`Event::Configured`]).
    pub fn create_layer(&mut self, cfg: LayerConfig, ui: Ui) -> Result<SurfaceId, Error> {
        if self.layer_shell.is_none() {
            return Err(Error::Global("zwlr_layer_shell_v1"));
        }
        let (id, wl, frac, viewport) = self.new_surface();
        let Some(shell) = self.layer_shell.as_ref() else {
            return Err(Error::Global("zwlr_layer_shell_v1"));
        };
        let layer = shell.create_layer_surface(
            &self.qh,
            wl.clone(),
            cfg.layer,
            Some(cfg.namespace.clone()),
            cfg.output.as_ref().map(|o| &o.wl),
        );
        apply_layer_config(&layer, &cfg);
        layer.commit();
        self.surfs.push(Surf {
            id,
            role: Role::Layer(layer),
            wl,
            ui,
            layer_cfg: Some(cfg),
            size: (0, 0),
            configured: false,
            hidden: false,
            frame_pending: false,
            frac,
            viewport,
            frac120: None,
            int_scale: 1,
            pool: None,
        });
        Ok(id)
    }

    /// Change a live layer surface: the closure may call `set_size`, `set_anchor`,
    /// `set_exclusive_zone`, `set_margin`, `set_keyboard_interactivity`, `set_layer`; the
    /// runtime commits afterwards. The stored configuration used by [`Runtime::show`] is
    /// updated only for what you pass through [`Runtime::update_layer_config`].
    pub fn configure_layer(&mut self, id: SurfaceId, f: impl FnOnce(&LayerSurface)) {
        if let Some(Surf {
            role: Role::Layer(l),
            wl,
            ..
        }) = self.surf_mut(id)
        {
            f(l);
            wl.commit();
        }
    }

    /// Change the stored configuration and apply it to the live surface.
    pub fn update_layer_config(&mut self, id: SurfaceId, f: impl FnOnce(&mut LayerConfig)) {
        if let Some(s) = self.surf_mut(id)
            && let (Some(cfg), Role::Layer(l)) = (&mut s.layer_cfg, &s.role)
        {
            f(cfg);
            apply_layer_config(l, cfg);
            s.wl.commit();
        }
    }

    /// Unmap a layer surface but keep its `Ui`, buffers and caches warm.
    pub fn hide(&mut self, id: SurfaceId) {
        if let Some(s) = self.surf_mut(id)
            && matches!(s.role, Role::Layer(_))
            && !s.hidden
        {
            s.wl.attach(None, 0, 0);
            s.wl.commit();
            s.hidden = true;
            s.configured = false;
            s.frame_pending = false;
        }
    }

    /// Map a hidden layer surface again (it is painted after the compositor configures it).
    pub fn show(&mut self, id: SurfaceId) {
        if let Some(s) = self.surf_mut(id)
            && s.hidden
            && let (Some(cfg), Role::Layer(l)) = (&s.layer_cfg, &s.role)
        {
            apply_layer_config(l, cfg);
            s.ui.invalidate_all();
            if let Some(p) = &mut s.pool {
                p.owed = full_owed(p.size());
            }
            s.wl.commit();
            // `hidden` clears when the configure arrives.
        }
    }

    pub fn is_hidden(&self, id: SurfaceId) -> bool {
        self.surf(id).is_some_and(|s| s.hidden)
    }

    /// Destroy a surface and everything attached to it.
    pub fn destroy(&mut self, id: SurfaceId) {
        if let Some(i) = self.surfs.iter().position(|s| s.id == id) {
            let s = self.surfs.remove(i);
            if let Some(f) = &s.frac {
                f.destroy();
            }
            if let Some(v) = &s.viewport {
                v.destroy();
            }
            if self.kb_focus == Some(id) {
                self.kb_focus = None;
            }
        }
    }

    /// Ask the compositor to lock the session. Follow up by creating one lock surface per
    /// output; [`Event::Locked`] arrives once every output has one.
    pub fn lock_session(&mut self) -> Result<(), Error> {
        if self.lock.is_some() {
            return Err(Error::State("lock already requested"));
        }
        self.lock = Some(
            self.lock_state
                .lock(&self.qh)
                .map_err(|_| Error::Global("ext_session_lock_manager_v1"))?,
        );
        Ok(())
    }

    /// Create the lock surface for one output. Only valid while a lock is requested.
    pub fn create_lock_surface(&mut self, output: &Output, ui: Ui) -> Result<SurfaceId, Error> {
        if self.lock.is_none() {
            return Err(Error::State("no session lock requested"));
        }
        let (id, wl, frac, viewport) = self.new_surface();
        let Some(lock) = self.lock.as_ref() else {
            return Err(Error::State("no session lock requested"));
        };
        let surface = lock.create_lock_surface(wl.clone(), &output.wl, &self.qh);
        self.surfs.push(Surf {
            id,
            role: Role::Lock(surface),
            wl,
            ui,
            layer_cfg: None,
            size: (0, 0),
            configured: false,
            hidden: false,
            frame_pending: false,
            frac,
            viewport,
            frac120: None,
            int_scale: 1,
            pool: None,
        });
        Ok(id)
    }

    pub fn is_locked(&self) -> bool {
        self.lock.as_ref().is_some_and(SessionLock::is_locked)
    }

    /// Unlock the session (only after the user authenticated) and drop the lock surfaces.
    /// A lock client that just exits leaves the session locked by protocol design.
    pub fn unlock(&mut self) -> Result<(), Error> {
        let lock = self.lock.take().ok_or(Error::State("not locked"))?;
        lock.unlock();
        let ids: Vec<_> = self
            .surfs
            .iter()
            .filter(|s| matches!(s.role, Role::Lock(_)))
            .map(|s| s.id)
            .collect();
        for id in ids {
            self.destroy(id);
        }
        // Make sure the compositor processed the unlock before we may exit.
        self.conn
            .roundtrip()
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(())
    }

    fn ready(s: &Surf) -> bool {
        s.configured
            && !s.hidden
            && !s.frame_pending
            && s.size.0 > 0
            && s.size.1 > 0
            && s.ui.needs_redraw()
    }

    fn render_pending(&mut self) {
        for i in 0..self.surfs.len() {
            if Self::ready(&self.surfs[i]) {
                self.render(i);
            }
        }
    }

    /// Paint and commit surface `i`: owed damage only, one buffer, one frame callback.
    fn render(&mut self, i: usize) {
        let Runtime {
            surfs,
            caches,
            shm,
            qh,
            ..
        } = self;
        let s = &mut surfs[i];
        let scale120 = s.scale120();
        let scale = scale120 as f32 / 120.0;
        let (dw, dh) = device_size(s.size, scale120);
        if s.pool.as_ref().map(BufferPool::size) != Some((dw, dh)) {
            match BufferPool::new(shm, qh, dw, dh) {
                Ok(p) => s.pool = Some(p),
                Err(e) => {
                    tracing::error!(target: "ui", "shm pool {dw}x{dh}: {e}");
                    return;
                }
            }
            s.ui.invalidate_all();
        }
        let Some(pool) = s.pool.as_mut() else { return };
        // All buffers still with the compositor: a release event will trigger another flush.
        let Some(slot) = pool.acquire() else { return };

        s.ui.set_size(Size::new(s.size.0 as f32, s.size.1 as f32));
        s.ui.set_scale(scale);
        s.ui.layout();
        let frame: Vec<Rect> =
            s.ui.take_damage()
                .into_iter()
                .map(|r| device_rect(r, scale, (dw, dh)))
                .collect();
        let region = repaint_region(&pool.owed, slot, &frame);
        if region.is_empty() {
            return;
        }
        let logical: Vec<Rect> = region
            .rects()
            .iter()
            .map(|r| logical_rect(*r, scale))
            .collect();
        {
            let Some(mut painter) = SkiaPainter::new(pool.canvas(slot), dw, dh, scale, caches)
            else {
                return;
            };
            s.ui.paint(&mut painter, &logical);
        }
        finish_frame(&mut pool.owed, slot, &frame);

        match (&s.viewport, s.frac120) {
            (Some(vp), Some(_)) => vp.set_destination(s.size.0 as i32, s.size.1 as i32),
            _ => s.wl.set_buffer_scale(s.int_scale.max(1)),
        }
        s.wl.attach(Some(pool.buffer(slot)), 0, 0);
        for r in &frame {
            s.wl.damage_buffer(r.x as i32, r.y as i32, r.w as i32, r.h as i32);
        }
        s.wl.frame(qh, FrameCallbackData(s.wl.clone()));
        s.wl.commit();
        pool.mark_busy(slot);
        s.frame_pending = true;
        tracing::trace!(target: "ui", "frame {dw}x{dh} scale={scale} damage={}", frame.len());
    }
}

fn full_owed(size: (u32, u32)) -> Vec<Damage> {
    (0..3)
        .map(|_| {
            let mut d = Damage::new();
            d.add(Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32));
            d
        })
        .collect()
}

fn apply_layer_config(l: &LayerSurface, c: &LayerConfig) {
    l.set_layer(c.layer);
    l.set_anchor(c.anchor);
    l.set_size(c.size.0, c.size.1);
    l.set_exclusive_zone(c.exclusive_zone);
    l.set_margin(c.margin.0, c.margin.1, c.margin.2, c.margin.3);
    l.set_keyboard_interactivity(c.keyboard);
}

impl<A: App> CompositorHandler for State<A> {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &WlSurface,
        new_factor: i32,
    ) {
        let Some(s) = self.rt.surfs.iter_mut().find(|s| &s.wl == surface) else {
            return;
        };
        s.int_scale = new_factor.max(1);
        if s.frac120.is_none() || s.viewport.is_none() {
            let (sid, scale) = (s.id, s.scale());
            s.ui.set_scale(scale);
            self.emit(Event::ScaleChanged {
                surface: sid,
                scale,
            });
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &WlSurface, _: u32) {
        if let Some(s) = self.rt.surfs.iter_mut().find(|s| &s.wl == surface) {
            s.frame_pending = false;
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl<A: App> OutputHandler for State<A> {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.rt.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        if let Some(o) = self.rt.output_of(&output) {
            self.rt.outputs.retain(|e| e.id != o.id);
            self.rt.outputs.push(o.clone());
            self.emit(Event::OutputAdded(o));
        }
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        if let Some(o) = self.rt.output_of(&output) {
            match self.rt.outputs.iter_mut().find(|e| e.id == o.id) {
                Some(e) => *e = o.clone(),
                None => self.rt.outputs.push(o.clone()),
            }
            self.emit(Event::OutputChanged(o));
        }
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        if let Some(i) = self.rt.outputs.iter().position(|o| o.wl == output) {
            let o = self.rt.outputs.remove(i);
            self.emit(Event::OutputRemoved(o));
        }
    }
}

impl<A: App> LayerShellHandler for State<A> {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(sid) = self.rt.sid_of(layer.wl_surface()) {
            self.rt.destroy(sid);
            self.emit(Event::Closed { surface: sid });
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.configured(layer.wl_surface(), configure.new_size);
    }
}

impl<A: App> SessionLockHandler for State<A> {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.emit(Event::Locked);
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.rt.lock = None;
        self.emit(Event::LockFinished);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        self.configured(surface.wl_surface(), configure.new_size);
    }
}

impl<A: App> SeatHandler for State<A> {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.rt.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard if self.rt.keyboard.is_none() => {
                let lh = self.rt.loop_handle.clone();
                let cb = Box::new(|s: &mut State<A>, _: &WlKeyboard, ev: SctkKey| s.key(&ev));
                match self
                    .rt
                    .seat_state
                    .get_keyboard_with_repeat(qh, &seat, None, lh, cb)
                {
                    Ok(k) => self.rt.keyboard = Some(k),
                    Err(e) => tracing::warn!(target: "ui", "keyboard: {e}"),
                }
            }
            Capability::Pointer if self.rt.pointer.is_none() => {
                match self.rt.seat_state.get_pointer(qh, &seat) {
                    Ok(p) => self.rt.pointer = Some(p),
                    Err(e) => tracing::warn!(target: "ui", "pointer: {e}"),
                }
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => {
                if let Some(k) = self.rt.keyboard.take() {
                    k.release();
                }
                self.rt.kb_focus = None;
            }
            Capability::Pointer => {
                if let Some(p) = self.rt.pointer.take() {
                    p.release();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl<A: App> KeyboardHandler for State<A> {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        surface: &WlSurface,
        _: u32,
        _: &[u32],
        _: &[smithay_client_toolkit::seat::keyboard::Keysym],
    ) {
        self.rt.kb_focus = self.rt.sid_of(surface);
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        surface: &WlSurface,
        _: u32,
    ) {
        if self.rt.sid_of(surface) == self.rt.kb_focus {
            self.rt.kb_focus = None;
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: SctkKey,
    ) {
        self.key(&event);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: SctkKey,
    ) {
        self.key(&event);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        _: SctkKey,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.rt.mods = map_mods(modifiers);
    }
}

impl<A: App> PointerHandler for State<A> {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlPointer,
        events: &[PointerEvent],
    ) {
        for e in events {
            let Some(sid) = self.rt.sid_of(&e.surface) else {
                continue;
            };
            let pos = Point::new(e.position.0 as f32, e.position.1 as f32);
            let input = match e.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    Input::PointerMove(pos)
                }
                PointerEventKind::Leave { .. } => Input::PointerLeave,
                PointerEventKind::Press { button, .. } => {
                    Input::PointerDown(pos, map_button(button))
                }
                PointerEventKind::Release { button, .. } => {
                    Input::PointerUp(pos, map_button(button))
                }
                PointerEventKind::Axis {
                    vertical,
                    horizontal,
                    ..
                } => {
                    // Wheels report steps (value120), touchpads absolute pixels.
                    let px = |a: smithay_client_toolkit::seat::pointer::AxisScroll| {
                        if a.absolute != 0.0 {
                            a.absolute as f32
                        } else {
                            a.value120 as f32 / 120.0 * 30.0
                        }
                    };
                    Input::Scroll {
                        pos,
                        dx: px(horizontal),
                        dy: px(vertical),
                    }
                }
            };
            self.deliver(sid, input);
        }
    }
}

impl<A: App> ShmHandler for State<A> {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.rt.shm
    }
}

impl<A: App> ProvidesRegistryState for State<A> {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.rt.registry_state
    }
    registry_handlers![OutputState, SeatState];
}
