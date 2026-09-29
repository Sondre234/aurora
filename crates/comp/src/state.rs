use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Duration};

use crate::config::Config;
use crate::focus::FocusTarget;
use crate::protocols::Protocols;
use crate::wm::window::WindowElement;
use crate::{backend::Backend, dmabuf::SurfaceDmabufFeedback};
use smithay::input::keyboard::Keycode;

use smithay::{
    backend::renderer::element::{
        RenderElementStates, default_primary_scanout_output_compare, utils::select_dmabuf_feedback,
    },
    desktop::{
        PopupManager, Space, WindowSurface, WindowSurfaceType, layer_map_for_output,
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
        shell::xdg::XdgShellState,
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
    pub config: Arc<Config>,
    pub config_path: PathBuf,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub handle: LoopHandle<'static, Aurora>,

    pub space: Space<WindowElement>,
    pub popups: PopupManager,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    #[allow(dead_code)] // held so the globals stay alive
    pub protocols: Protocols,
    pub seat_state: SeatState<Aurora>,
    pub data_device_state: DataDeviceState,
    pub dmabuf_state: DmabufState,
    #[allow(dead_code)] // held so the linux-dmabuf global stays alive
    pub dmabuf_global: Option<DmabufGlobal>,
    pub syncobj_state: Option<DrmSyncobjState>,

    #[allow(dead_code)] // held so the wl_seat global stays alive
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
        let protocols = Protocols::new(&dh, clock.id() as u32);

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
            config,
            config_path,
            socket_name,
            display_handle: dh,
            loop_signal: event_loop.get_signal(),
            handle: event_loop.handle(),
            space: Space::default(),
            popups: PopupManager::default(),
            compositor_state,
            xdg_shell_state,
            shm_state,
            protocols,
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
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, p)| {
                        let target = match window.underlying_surface() {
                            WindowSurface::X11(x11) => FocusTarget::X11(x11.clone()),
                            WindowSurface::Wayland(_) => FocusTarget::Wl(surface),
                        };
                        (target, (p + location).to_f64())
                    })
            })
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

        for window in self.space.elements() {
            if self.space.outputs_for_element(window).contains(output) {
                window.send_frame(output, time, throttle, surface_primary_scanout_output);
            }
        }
        for layer in layer_map_for_output(output).layers() {
            layer.send_frame(output, time, throttle, surface_primary_scanout_output);
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
        for layer in layer_map_for_output(output).layers() {
            layer.send_dmabuf_feedback(output, surface_primary_scanout_output, select);
        }
    }
}

/// Records which output each surface is mostly presented on, from the last frame's element states.
pub fn update_primary_scanout_output(
    space: &Space<WindowElement>,
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
