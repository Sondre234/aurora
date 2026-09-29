//! linux-dmabuf global and the per-surface feedback that steers clients towards buffers the
//! display hardware can scan out directly.

use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, format::FormatSet},
        drm::{DrmNode, DrmSurface},
        egl::EGLDevice,
        renderer::{ImportDma, gles::GlesRenderer},
    },
    reexports::wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags,
    wayland::dmabuf::{
        DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState,
        ImportNotifier,
    },
};

use crate::{backend::Backend, state::Aurora};

/// Feedback handed to a surface's client depending on whether it is being scanned out.
#[derive(Debug, Clone)]
pub struct SurfaceDmabufFeedback {
    pub render_feedback: DmabufFeedback,
    pub scanout_feedback: DmabufFeedback,
}

impl Aurora {
    /// Creates the dmabuf global for a renderer. Without a node to name (a nested
    /// backend on a driver with no render node) the global carries formats only.
    pub fn init_dmabuf(&mut self, node: Option<DrmNode>, formats: FormatSet) {
        let dh = &self.display_handle;
        let global = match node
            .map(|n| DmabufFeedbackBuilder::new(n.dev_id(), formats.iter().copied()).build())
        {
            Some(Ok(feedback)) => {
                tracing::info!(node = ?node, formats = formats.iter().count(), "dmabuf global with feedback");
                self.dmabuf_state
                    .create_global_with_default_feedback::<Aurora>(dh, &feedback)
            }
            other => {
                if let Some(Err(err)) = other {
                    tracing::warn!(%err, "dmabuf feedback build failed, falling back to plain global");
                }
                tracing::info!(
                    formats = formats.iter().count(),
                    "dmabuf global without feedback"
                );
                self.dmabuf_state
                    .create_global::<Aurora>(dh, formats.iter().copied())
            }
        };
        self.dmabuf_global = Some(global);
    }
}

/// Render node of the EGL display behind `renderer`, if it has one.
pub fn renderer_node(renderer: &GlesRenderer) -> Option<DrmNode> {
    let device = EGLDevice::device_for_display(renderer.egl_context().display()).ok()?;
    device.try_get_render_node().ok().flatten()
}

/// Builds the feedback pair for one output. The scanout tranche is limited to formats the
/// renderer can also sample, so a buffer the planes reject always has a render fallback.
pub fn surface_feedback(
    render_node: DrmNode,
    render_formats: &FormatSet,
    surface: &DrmSurface,
) -> Option<SurfaceDmabufFeedback> {
    let scanout_dev = surface.device_fd().dev_id().ok()?;
    let plane_formats = surface
        .plane_info()
        .formats
        .iter()
        .copied()
        .collect::<FormatSet>()
        .intersection(render_formats)
        .copied()
        .collect::<FormatSet>();

    let builder = DmabufFeedbackBuilder::new(render_node.dev_id(), render_formats.iter().copied());
    let render_feedback = builder.clone().build().ok()?;
    let scanout_feedback = builder
        .add_preference_tranche(scanout_dev, TrancheFlags::Scanout, plane_formats, 4u32..=6)
        .add_preference_tranche(
            render_node.dev_id(),
            TrancheFlags::Sampling,
            render_formats.iter().copied(),
            4u32..=6,
        )
        .build()
        .ok()?;
    Some(SurfaceDmabufFeedback {
        render_feedback,
        scanout_feedback,
    })
}

impl DmabufHandler for Aurora {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        match &mut self.backend {
            Backend::Drm(drm) => {
                // Must be the node the scanout exporter filters on, or direct scanout never matches.
                let node = drm
                    .devices
                    .get(&drm.primary_gpu)
                    .map_or(drm.primary_gpu, |d| d.render_node);
                let imported = drm
                    .renderer
                    .as_mut()
                    .is_some_and(|r| r.import_dmabuf(&dmabuf, None).is_ok());
                if !imported {
                    notifier.failed();
                    return;
                }
                if dmabuf.node().is_none() {
                    dmabuf.set_node(node);
                }
            }
            // The nested renderer lives in the winit event closure; the host validates the
            // buffer when it is first drawn.
            Backend::Winit => {}
        }
        let _ = notifier.successful::<Aurora>();
    }
}
