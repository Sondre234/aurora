use std::{ffi::OsString, sync::Arc};

use crate::backend::Backend;
use smithay::input::keyboard::Keycode;

use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState, pointer::CursorImageStatus},
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

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, backend.seat_name());
        // Hotplug tracking arrives with the DRM backend (M1).
        seat.add_keyboard(Default::default(), REPEAT_DELAY, REPEAT_RATE)
            .unwrap();
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        Self {
            backend,
            clock: Clock::new(),
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
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
