//! Protocol globals that only need to stay alive; their handlers hold no state in `Aurora`.
use smithay::{
    reexports::wayland_server::DisplayHandle,
    wayland::{output::OutputManagerState, presentation::PresentationState},
};

use crate::{state::Aurora, virtual_input::VirtualKeyboardGlobal};

pub struct Protocols {
    _output_manager: OutputManagerState,
    _presentation: PresentationState,
    pub virtual_keyboard: VirtualKeyboardGlobal,
}

impl Protocols {
    pub fn new(dh: &DisplayHandle, clock_id: u32, allow_virtual_keyboard: bool) -> Self {
        Self {
            _output_manager: OutputManagerState::new_with_xdg_output::<Aurora>(dh),
            _presentation: PresentationState::new::<Aurora>(dh, clock_id),
            virtual_keyboard: VirtualKeyboardGlobal::new(dh, allow_virtual_keyboard),
        }
    }
}
