//! linux-drm-syncobj explicit sync, and the commit hook that makes clients' buffers
//! wait for their acquire fence. Without the hook NVIDIA shows torn or stale frames.

use smithay::{
    backend::drm::DrmDeviceFd,
    reexports::{
        calloop::Interest,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    wayland::{
        compositor::{
            BufferAssignment, CompositorHandler, SurfaceAttributes, add_blocker,
            add_pre_commit_hook, with_states,
        },
        dmabuf::get_dmabuf,
        drm_syncobj::{
            DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState, supports_syncobj_eventfd,
        },
    },
};

use crate::state::Aurora;

impl Aurora {
    /// Exposes wp_linux_drm_syncobj only when the kernel can signal an eventfd from a
    /// fence; otherwise clients fall back to implicit sync through the dmabuf blocker.
    pub fn init_syncobj(&mut self, device_fd: DrmDeviceFd) {
        if supports_syncobj_eventfd(&device_fd) {
            self.syncobj_state = Some(DrmSyncobjState::new::<Aurora>(
                &self.display_handle,
                device_fd,
            ));
            tracing::info!("explicit sync: wp_linux_drm_syncobj enabled");
        } else {
            tracing::warn!("explicit sync: syncobj eventfd unsupported, using implicit sync only");
        }
    }
}

impl DrmSyncobjHandler for Aurora {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}

/// Holds a commit back until the attached dmabuf is ready to be read.
pub fn install_blocker_hook(surface: &WlSurface) {
    add_pre_commit_hook::<Aurora, _>(surface, |state, _dh, surface| {
        let mut acquire_point = None;
        let dmabuf = with_states(surface, |data| {
            acquire_point.clone_from(
                &data
                    .cached_state
                    .get::<DrmSyncobjCachedState>()
                    .pending()
                    .acquire_point,
            );
            data.cached_state
                .get::<SurfaceAttributes>()
                .pending()
                .buffer
                .as_ref()
                .and_then(|assignment| match assignment {
                    BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                    _ => None,
                })
        });
        let Some(dmabuf) = dmabuf else { return };
        let Some(client) = surface.client() else {
            return;
        };

        // Register `source` so the commit resumes once the blocker clears.
        macro_rules! block_on {
            ($blocker:expr, $source:expr) => {{
                let client = client.clone();
                let registered = state.handle.insert_source($source, move |_, _, data| {
                    let dh = data.display_handle.clone();
                    data.client_compositor_state(&client)
                        .blocker_cleared(data, &dh);
                    Ok(())
                });
                match registered {
                    Ok(_) => {
                        add_blocker(surface, $blocker);
                        true
                    }
                    Err(err) => {
                        tracing::warn!(%err, "failed to register a buffer blocker");
                        false
                    }
                }
            }};
        }

        if let Some(point) = acquire_point
            && let Ok((blocker, source)) = point.generate_blocker()
            && block_on!(blocker, source)
        {
            return;
        }
        if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
            block_on!(blocker, source);
        }
    });
}
