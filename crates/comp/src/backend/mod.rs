pub mod drm;
pub mod winit;

use smithay::{
    backend::session::Session, reexports::wayland_server::protocol::wl_surface::WlSurface,
};

pub use drm::DrmBackend;

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

    /// Hook for importing a surface buffer before commit (dmabuf/explicit sync on DRM).
    #[allow(dead_code)]
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
