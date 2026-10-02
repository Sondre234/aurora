//! Display control: monitor power (DPMS), and the protocols that let privileged clients
//! drive the outputs. The backend-specific half (what "off" means on DRM) lives in
//! `backend/drm/display.rs`; everything here is backend independent.
use smithay::{output::Output, reexports::wayland_server::DisplayHandle};

use crate::state::Aurora;

pub mod power;

/// Protocol globals and bookkeeping of the display stream, one field of `Aurora`.
pub struct DisplayState {
    pub power: power::PowerState,
}

impl DisplayState {
    pub fn new(dh: &DisplayHandle) -> Self {
        Self {
            power: power::PowerState::new(dh),
        }
    }
}

impl Aurora {
    /// A new output joined the layout (called from `add_output`).
    pub fn display_output_added(&mut self, _output: &Output) {}

    /// An output is going away and is still alive (called from `wm_output_removed`).
    pub fn display_output_removed(&mut self, output: &Output) {
        self.power_output_removed(output);
    }
}
