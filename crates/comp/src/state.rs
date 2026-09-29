use std::{ffi::OsString, sync::Arc, time::Duration};

use crate::backend::Backend;
use smithay::input::keyboard::Keycode;

use smithay::{
    backend::renderer::element::{RenderElementStates, default_primary_scanout_output_compare},
    desktop::{
        PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output,
        utils::{
            OutputPresentationFeedback, send_frames_surface_tree,
            surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
            update_surface_primary_scanout_output, with_surfaces_surface_tree,
        },
    },
    input::{Seat, SeatState, pointer::CursorImageStatus},
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
        output::OutputManagerState,
        presentation::PresentationState,
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
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub handle: LoopHandle<'static, Aurora>,

    pub space: Space<Window>,
    pub popups: PopupManager,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    #[allow(dead_code)] // held so the xdg-output globals stay alive
    pub output_manager_state: OutputManagerState,
    #[allow(dead_code)] // held so the wp_presentation global stays alive
    pub presentation_state: PresentationState,
    pub seat_state: SeatState<Aurora>,
    pub data_device_state: DataDeviceState,

    pub seat: Seat<Self>,
}

impl Aurora {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        backend: Backend,
    ) -> Self {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let clock = Clock::new();
        let presentation_state = PresentationState::new::<Self>(&dh, clock.id() as u32);

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, backend.seat_name());
        // Hotplug tracking arrives with the DRM backend (M1).
        seat.add_keyboard(Default::default(), REPEAT_DELAY, REPEAT_RATE)
            .unwrap();
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        Self {
            backend,
            clock,
            cursor_status: CursorImageStatus::default_named(),
            suppressed_keys: Vec::new(),
            socket_name,
            display_handle: dh,
            loop_signal: event_loop.get_signal(),
            handle: event_loop.handle(),
            space: Space::default(),
            popups: PopupManager::default(),
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            presentation_state,
            seat_state,
            data_device_state,
            seat,
        }
    }

    fn init_wayland_listener(
        display: Display<Aurora>,
        event_loop: &mut EventLoop<Self>,
    ) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto().unwrap();
        let socket_name = listening_socket.socket_name().to_os_string();
        let handle = event_loop.handle();

        handle
            .insert_source(listening_socket, |stream, _, state| {
                state
                    .display_handle
                    .insert_client(stream, Arc::new(ClientState::default()))
                    .unwrap();
            })
            .expect("failed to init the wayland listening socket");

        handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Safety: the display is owned by this source and never dropped while it runs.
                    unsafe { display.get_mut().dispatch_clients(state).unwrap() };
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, p)| (surface, (p + location).to_f64()))
            })
    }

    /// Sends frame callbacks for everything drawn on `output`. Surfaces whose primary
    /// scanout output is elsewhere are throttled to one callback a second.
    /// Dmabuf feedback per surface joins this with the dmabuf step.
    pub fn post_repaint(&mut self, output: &Output, time: Duration) {
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
    }
}

/// Records which output each surface is mostly presented on, from the last frame's element states.
pub fn update_primary_scanout_output(
    space: &Space<Window>,
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
    space: &Space<Window>,
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
