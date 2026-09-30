use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Duration};

use crate::config::Config;
use crate::focus::FocusTarget;
use crate::layers::Hit;
use crate::protocols::Protocols;
use crate::wm::{Wm, window::WindowElement};
use crate::{backend::Backend, dmabuf::SurfaceDmabufFeedback};
use smithay::input::keyboard::Keycode;

use smithay::{
    backend::renderer::element::{
        RenderElementStates, default_primary_scanout_output_compare, utils::select_dmabuf_feedback,
    },
    desktop::{
        PopupManager, Space, Window, WindowSurface, WindowSurfaceType, layer_map_for_output,
        utils::{
            OutputPresentationFeedback, send_frames_surface_tree,
            surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
            update_surface_primary_scanout_output, with_surfaces_surface_tree,
        },
    },
    input::{
        Seat, SeatState,
        keyboard::KeyboardHandle,
        pointer::{CursorImageStatus, PointerHandle},
    },
    output::Output,
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic,
        },
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Clock, Logical, Monotonic, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufGlobal, DmabufState},
        drm_syncobj::DrmSyncobjState,
        selection::data_device::DataDeviceState,
        shell::{wlr_layer::Layer, xdg::XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

/// Keyboard repeat delay (ms) and rate (keys per second).
pub const REPEAT_DELAY: i32 = 250;
pub const REPEAT_RATE: i32 = 40;

pub struct Aurora {
    pub backend: Backend,
    pub clock: Clock<Monotonic>,
    pub cursor_status: CursorImageStatus,
    /// Keys whose press was taken by a compositor shortcut; their release is swallowed too.
    pub suppressed_keys: Vec<Keycode>,
    pub input: crate::input::InputState,
    /// `--qa`: enables the debug input actions.
    pub qa: bool,
    pub config: Arc<Config>,
    pub config_path: PathBuf,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub handle: LoopHandle<'static, Aurora>,

    pub wm: Wm,
    /// QA outputs made by `debug-add-output`, by name.
    pub headless: std::collections::HashMap<String, crate::backend::headless::HeadlessOutput>,
    pub layer_focus: crate::layers::LayerFocus,
    pub space: Space<WindowElement>,
    pub xwayland: crate::xwayland::XWaylandState,
    pub popups: PopupManager,
    /// The live xdg_popup grab with its root surface, kept so compositor focus changes can end it.
    pub popup_grab: Option<(
        smithay::desktop::PopupGrab<Aurora>,
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    )>,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub protocols: Protocols,
    pub captures: crate::capture::Captures,
    pub seat_state: SeatState<Aurora>,
    pub data_device_state: DataDeviceState,
    pub dmabuf_state: DmabufState,
    #[allow(dead_code)] // held so the linux-dmabuf global stays alive
    pub dmabuf_global: Option<DmabufGlobal>,
    pub syncobj_state: Option<DrmSyncobjState>,

    pub seat: Seat<Self>,
    pub keyboard: KeyboardHandle<Self>,
    pub pointer: PointerHandle<Self>,
}

impl Aurora {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        backend: Backend,
        config: Arc<Config>,
        config_path: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let clock = Clock::new();
        let protocols = Protocols::new(
            &dh,
            &event_loop.handle(),
            clock.id() as u32,
            config.general.allow_virtual_keyboard,
        );

        let captures = crate::capture::Captures::new(&dh);
        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, backend.seat_name());
        // Hotplug tracking arrives with the DRM backend (M1).
        let keyboard = seat
            .add_keyboard(Default::default(), REPEAT_DELAY, REPEAT_RATE)
            .map_err(|err| format!("failed to add the keyboard: {err}"))?;
        let pointer = seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop)?;

        Ok(Self {
            backend,
            clock,
            cursor_status: CursorImageStatus::default_named(),
            suppressed_keys: Vec::new(),
            input: Default::default(),
            qa: false,
            config,
            config_path,
            socket_name,
            display_handle: dh,
            loop_signal: event_loop.get_signal(),
            handle: event_loop.handle(),
            wm: Wm::default(),
            headless: Default::default(),
            layer_focus: Default::default(),
            space: Space::default(),
            xwayland: Default::default(),
            popups: PopupManager::default(),
            popup_grab: None,
            compositor_state,
            xdg_shell_state,
            shm_state,
            protocols,
            captures,
            seat_state,
            data_device_state,
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            syncobj_state: None,
            seat,
            keyboard,
            pointer,
        })
    }

    fn init_wayland_listener(
        display: Display<Aurora>,
        event_loop: &mut EventLoop<Self>,
    ) -> Result<OsString, Box<dyn std::error::Error>> {
        let listening_socket = ListeningSocketSource::new_auto()
            .map_err(|err| format!("failed to create the wayland socket: {err}"))?;
        let socket_name = listening_socket.socket_name().to_os_string();
        let handle = event_loop.handle();

        handle
            .insert_source(listening_socket, |stream, _, state| {
                if let Err(err) = state
                    .display_handle
                    .insert_client(stream, Arc::new(ClientState::default()))
                {
                    tracing::warn!(%err, "failed to accept a wayland client");
                }
            })
            .map_err(|err| format!("failed to register the wayland socket: {err}"))?;

        handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Safety: the display is owned by this source and never dropped while it runs.
                    unsafe { display.get_mut().dispatch_clients(state)? };
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|err| format!("failed to register the wayland display: {err}"))?;

        Ok(socket_name)
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(FocusTarget, Point<f64, Logical>)> {
        match self.hit_test(pos) {
            Hit::Layer(hit) => Some((FocusTarget::Wl(hit.surface), hit.loc)),
            Hit::Window(window, location) => window_focus(&window, pos, location),
            Hit::Unmanaged(window, location) => window_focus(&window, pos, location),
            Hit::Nothing => None,
        }
    }

    /// Sends frame callbacks for everything drawn on `output`, plus dmabuf feedback when the
    /// output has scanout feedback. Surfaces whose primary scanout output is elsewhere are
    /// throttled to one callback a second.
    pub fn post_repaint(
        &mut self,
        output: &Output,
        time: Duration,
        dmabuf_feedback: Option<&SurfaceDmabufFeedback>,
        states: &RenderElementStates,
    ) {
        let throttle = Some(Duration::from_secs(1));
        self.send_output_scale(output);

        for window in self.space.elements() {
            if self.space.outputs_for_element(window).contains(output) {
                window.send_frame(output, time, throttle, surface_primary_scanout_output);
                if let Some(win) = self.wm.windows.get_mut(&window.id()) {
                    win.frames_sent += 1;
                }
            }
        }
        for window in self.xwayland.unmanaged.elements() {
            if self
                .xwayland
                .unmanaged
                .outputs_for_element(window)
                .contains(output)
            {
                window.send_frame(output, time, throttle, surface_primary_scanout_output);
            }
        }
        // Layers hidden by a fullscreen window have no primary output worth trusting, so they
        // fall to the throttle.
        let top_hidden = crate::scene::top_hidden(output);
        for layer in layer_map_for_output(output).layers() {
            let hidden = top_hidden && layer.layer() == Layer::Top;
            layer.send_frame(output, time, throttle, |s, d| {
                if hidden {
                    None
                } else {
                    surface_primary_scanout_output(s, d)
                }
            });
        }
        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
            send_frames_surface_tree(
                surface,
                output,
                time,
                throttle,
                surface_primary_scanout_output,
            );
        }

        // Scanout feedback exists only on DRM outputs.
        let Some(fb) = dmabuf_feedback else { return };
        let select = |surface: &WlSurface, _: &_| {
            select_dmabuf_feedback(surface, states, &fb.render_feedback, &fb.scanout_feedback)
        };
        for window in self.space.elements() {
            if self.space.outputs_for_element(window).contains(output) {
                window.send_dmabuf_feedback(output, surface_primary_scanout_output, select);
            }
        }
        for window in self.xwayland.unmanaged.elements() {
            if self
                .xwayland
                .unmanaged
                .outputs_for_element(window)
                .contains(output)
            {
                window.send_dmabuf_feedback(output, surface_primary_scanout_output, select);
            }
        }
        for layer in layer_map_for_output(output).layers() {
            layer.send_dmabuf_feedback(output, surface_primary_scanout_output, select);
        }
    }
}

/// The surface of `window` at `pos` and where it sits, as a seat focus target.
fn window_focus(
    window: &Window,
    pos: Point<f64, Logical>,
    location: Point<i32, Logical>,
) -> Option<(FocusTarget, Point<f64, Logical>)> {
    let (surface, p) = window.surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)?;
    let target = match window.underlying_surface() {
        WindowSurface::X11(x11) => FocusTarget::X11(x11.clone()),
        WindowSurface::Wayland(_) => FocusTarget::Wl(surface),
    };
    Some((target, (p + location).to_f64()))
}

/// Records which output each surface is mostly presented on, from the last frame's element states.
pub fn update_primary_scanout_output(
    space: &Space<WindowElement>,
    unmanaged: &Space<Window>,
    output: &Output,
    cursor_status: &CursorImageStatus,
    states: &RenderElementStates,
) {
    let update = |surface: &WlSurface, data: &smithay::wayland::compositor::SurfaceData| {
        update_surface_primary_scanout_output(
            surface,
            output,
            data,
            None,
            states,
            default_primary_scanout_output_compare,
        );
    };
    for window in space.elements() {
        window.with_surfaces(update);
    }
    for window in unmanaged.elements() {
        window.with_surfaces(update);
    }
    for layer in layer_map_for_output(output).layers() {
        layer.with_surfaces(update);
    }
    if let CursorImageStatus::Surface(surface) = cursor_status {
        with_surfaces_surface_tree(surface, update);
    }
}

/// Collects the presentation feedback requested by everything visible on `output`.
pub fn take_presentation_feedback(
    output: &Output,
    space: &Space<WindowElement>,
    unmanaged: &Space<Window>,
    states: &RenderElementStates,
) -> OutputPresentationFeedback {
    let mut feedback = OutputPresentationFeedback::new(output);
    let flags = |surface: &WlSurface, _: &_| {
        surface_presentation_feedback_flags_from_states(surface, None, states)
    };

    for window in space.elements() {
        if space.outputs_for_element(window).contains(output) {
            window.take_presentation_feedback(&mut feedback, surface_primary_scanout_output, flags);
        }
    }
    for window in unmanaged.elements() {
        if unmanaged.outputs_for_element(window).contains(output) {
            window.take_presentation_feedback(&mut feedback, surface_primary_scanout_output, flags);
        }
    }
    for layer in layer_map_for_output(output).layers() {
        layer.take_presentation_feedback(&mut feedback, surface_primary_scanout_output, flags);
    }
    feedback
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
