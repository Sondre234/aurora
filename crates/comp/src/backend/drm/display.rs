//! The DRM half of display control. Power off is what niri does: the DRM compositor clears
//! the CRTC (planes off, connectors detached, ACTIVE=0) and the output stops rendering; the
//! next queued frame re-enables it with a full modeset. Not exercised by the nested backend,
//! so it is kept to the same few calls niri makes.
use smithay::output::Output;

use super::device::UdevOutputId;
use crate::{backend::Backend, state::Aurora};

impl Aurora {
    pub fn drm_set_power(&mut self, output: &Output, on: bool) {
        let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
            return;
        };
        let handle = self.handle.clone();
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        let session_active = drm.session_active;
        let Some(surface) = drm
            .devices
            .get_mut(&id.device_id)
            .and_then(|d| d.surfaces.get_mut(&id.crtc))
        else {
            return;
        };
        // Whatever was scheduled belongs to the previous power state.
        surface.render.cancel(&handle);
        if on {
            // The first frame re-enables the CRTC; treat a failure like one after a resume.
            surface.render.after_resume = true;
            surface.render.damage(&handle, id.device_id, id.crtc);
        } else if session_active {
            // Inactive: the resume path clears it once the device is ours again.
            if let Err(err) = surface.drm_output.with_compositor(|c| c.clear()) {
                tracing::warn!("power: cannot clear {}: {err}", output.name());
            }
        }
    }
}
