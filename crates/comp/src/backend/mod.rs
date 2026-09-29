pub mod winit;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

/// DRM/KMS state; filled in by the following M1 steps.
#[allow(dead_code)] // constructed once the DRM backend lands
pub struct DrmBackend {}

pub enum Backend {
    Winit,
    #[allow(dead_code)]
    Drm(DrmBackend),
}

impl Backend {
    pub fn seat_name(&self) -> String {
        match self {
            Backend::Winit => "winit".to_string(),
            Backend::Drm(_) => "seat0".to_string(),
        }
    }

    /// Hook for importing a surface buffer before commit (dmabuf/explicit sync on DRM).
    #[allow(dead_code)]
    pub fn early_import(&mut self, _surface: &WlSurface) {}

    /// Switches virtual terminal; nothing to do when nested.
    #[allow(dead_code)]
    pub fn change_vt(&mut self, vt: i32) {
        match self {
            Backend::Winit => tracing::debug!(vt, "ignoring VT switch in nested backend"),
            Backend::Drm(_) => {}
        }
    }
}
