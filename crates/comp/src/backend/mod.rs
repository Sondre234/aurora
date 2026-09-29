pub mod drm;
pub mod winit;

use smithay::{
    backend::session::Session, reexports::wayland_server::protocol::wl_surface::WlSurface,
};

pub use drm::DrmBackend;

/// Clear color behind all windows, shared by both backends.
pub const BACKGROUND: [f32; 4] = [0.06, 0.06, 0.09, 1.0];

pub enum Backend {
    Winit,
    Drm(Box<DrmBackend>),
}

impl Backend {
    pub fn seat_name(&self) -> String {
        match self {
            Backend::Winit => "winit".to_string(),
            Backend::Drm(drm) => drm.seat_name.clone(),
        }
    }

    /// Hook run on commit after buffer bookkeeping. Single-GPU, so buffers are imported by the
    /// renderer at draw time and there is nothing to do; multi-GPU would import here.
    pub fn early_import(&mut self, _surface: &WlSurface) {}

    /// Switches virtual terminal; nothing to do when nested.
    pub fn change_vt(&mut self, vt: i32) {
        match self {
            Backend::Winit => tracing::debug!(vt, "ignoring VT switch in nested backend"),
            Backend::Drm(drm) => {
                if let Err(err) = drm.session.change_vt(vt) {
                    tracing::error!(vt, %err, "VT switch failed");
                }
            }
        }
    }
}
