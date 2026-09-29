//! Protocol globals that only need to stay alive; their handlers hold no state in `Aurora`.
use smithay::{
    reexports::wayland_server::DisplayHandle,
    wayland::{output::OutputManagerState, presentation::PresentationState},
};

use crate::state::Aurora;

pub struct Protocols {
    _output_manager: OutputManagerState,
    _presentation: PresentationState,
}

impl Protocols {
    pub fn new(dh: &DisplayHandle, clock_id: u32) -> Self {
        Self {
            _output_manager: OutputManagerState::new_with_xdg_output::<Aurora>(dh),
            _presentation: PresentationState::new::<Aurora>(dh, clock_id),
        }
    }
}
