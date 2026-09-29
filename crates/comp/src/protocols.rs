//! Protocol globals that only need to stay alive; their handlers hold no state in `Aurora`.
use smithay::{
    reexports::wayland_server::DisplayHandle,
    wayland::{
        fractional_scale::FractionalScaleManagerState, output::OutputManagerState,
        presentation::PresentationState, selection::primary_selection::PrimarySelectionState,
        shell::wlr_layer::WlrLayerShellState, viewporter::ViewporterState,
        xwayland_shell::XWaylandShellState,
    },
};

use crate::{state::Aurora, virtual_input::VirtualKeyboardGlobal};

pub struct Protocols {
    _output_manager: OutputManagerState,
    _presentation: PresentationState,
    _fractional_scale: FractionalScaleManagerState,
    _viewporter: ViewporterState,
    pub layer_shell: WlrLayerShellState,
    pub primary_selection: PrimarySelectionState,
    pub xwayland_shell: XWaylandShellState,
    pub virtual_keyboard: VirtualKeyboardGlobal,
}

impl Protocols {
    pub fn new(dh: &DisplayHandle, clock_id: u32, allow_virtual_keyboard: bool) -> Self {
        Self {
            _output_manager: OutputManagerState::new_with_xdg_output::<Aurora>(dh),
            _presentation: PresentationState::new::<Aurora>(dh, clock_id),
            _fractional_scale: FractionalScaleManagerState::new::<Aurora>(dh),
            _viewporter: ViewporterState::new::<Aurora>(dh),
            layer_shell: WlrLayerShellState::new::<Aurora>(dh),
            primary_selection: PrimarySelectionState::new::<Aurora>(dh),
            xwayland_shell: XWaylandShellState::new::<Aurora>(dh),
            virtual_keyboard: VirtualKeyboardGlobal::new(dh, allow_virtual_keyboard),
        }
    }
}
