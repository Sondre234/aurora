use std::{collections::HashMap, path::Path};

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDevice, EGLDisplay, context::ContextPriority},
        renderer::{
            ImportMemWl, element::surface::WaylandSurfaceRenderElement, gles::GlesRenderer,
        },
        session::Session,
    },
    desktop::utils::OutputPresentationFeedback,
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        calloop::RegistrationToken,
        drm::{
            Device as _,
            control::{ModeTypeFlags, connector, crtc},
        },
        rustix::fs::OFlags,
        wayland_server::{DisplayHandle, backend::GlobalId},
    },
    utils::DeviceFd,
};
use smithay_drm_extras::{
    display_info,
    drm_scanner::{DrmScanEvent, DrmScanner},
};

use crate::{backend::Backend, state::Aurora};

pub type Allocator = GbmAllocator<DrmDeviceFd>;
pub type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
pub type Feedback = Option<OutputPresentationFeedback>;
pub type OutputManager = DrmOutputManager<Allocator, Exporter, Feedback, DrmDeviceFd>;
pub type ConnectorOutput = DrmOutput<Allocator, Exporter, Feedback, DrmDeviceFd>;
/// Element type the DRM compositor is instantiated with; the render loop uses the same.
pub type Element = WaylandSurfaceRenderElement<GlesRenderer>;

// Not Argb2101010-only: some drivers expose just one channel order, so both are offered.
const FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];
const FORMATS_8BIT: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

/// Identifies the DRM output behind a wl_output, stored in its user data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // read by the render loop
pub struct UdevOutputId {
    pub device_id: DrmNode,
    pub crtc: crtc::Handle,
}

/// A live connector. Field order matters: the DRM output goes before its global goes away.
pub struct Surface {
    #[allow(dead_code)] // driven by the render loop
    pub drm_output: ConnectorOutput,
    pub output: Output,
    global: Option<GlobalId>,
    dh: DisplayHandle,
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.output.leave_all();
        if let Some(global) = self.global.take() {
            self.dh.remove_global::<Aurora>(global);
        }
    }
}

/// One opened DRM device. Surfaces drop before the output manager, which drops before the fd.
pub struct Device {
    pub surfaces: HashMap<crtc::Handle, Surface>,
    pub output_manager: OutputManager,
    scanner: DrmScanner,
    #[allow(dead_code)] // used for dmabuf feedback
    pub render_node: DrmNode,
    registration_token: RegistrationToken,
}

impl Aurora {
    /// Opens a DRM node and builds the renderer and output manager on it.
    /// Only the primary GPU is used; other nodes are skipped with a warning.
    pub fn drm_device_added(&mut self, node: DrmNode, path: &Path) -> Result<(), String> {
        let Backend::Drm(drm) = &mut self.backend else {
            return Ok(());
        };
        if node != drm.primary_gpu {
            tracing::warn!(%node, path = %path.display(), "skipping non-primary gpu (multi-gpu is a non-goal)");
            return Ok(());
        }
        if drm.devices.contains_key(&node) {
            return Ok(());
        }

        let fd = drm
            .session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(|err| format!("failed to open {} through libseat: {err}", path.display()))?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm_device, notifier) =
            DrmDevice::new(fd.clone(), true).map_err(|err| format!("drm device init: {err}"))?;
        let gbm = GbmDevice::new(fd).map_err(|err| format!("gbm device init: {err}"))?;

        // Safety: the display is created from a live gbm device that outlives the renderer.
        let egl_display = unsafe { EGLDisplay::new(gbm.clone()) }
            .map_err(|err| format!("egl display init: {err}"))?;
        let egl_device = EGLDevice::device_for_display(&egl_display)
            .map_err(|err| format!("egl device lookup: {err}"))?;
        if egl_device.is_software() {
            return Err("egl picked a software renderer; refusing to run on it".into());
        }
        let render_node = egl_device
            .try_get_render_node()
            .ok()
            .flatten()
            .unwrap_or(node);

        let context = EGLContext::new_with_priority(&egl_display, ContextPriority::High)
            .map_err(|err| format!("egl context init: {err}"))?;
        // Safety: the context was just created for this display and is moved into the renderer.
        let renderer = unsafe {
            GlesRenderer::supported_capabilities(&context)
                .and_then(|caps| GlesRenderer::with_capabilities(context, caps))
        }
        .map_err(|err| format!("gles renderer init: {err}"))?;

        let render_formats: FormatSet = renderer.egl_context().dmabuf_render_formats().clone();
        let formats = if std::env::var_os("AURORA_DISABLE_10BIT").is_some() {
            FORMATS_8BIT
        } else {
            FORMATS
        };
        tracing::info!(
            %node, %render_node, formats = ?formats,
            render_formats = render_formats.iter().count(),
            "gpu renderer ready"
        );

        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = GbmFramebufferExporter::new(gbm.clone(), Some(render_node).into());
        let output_manager = DrmOutputManager::new(
            drm_device,
            allocator,
            exporter,
            Some(gbm),
            formats.iter().copied(),
            render_formats,
        );

        self.shm_state.update_formats(renderer.shm_formats());

        let registration_token = self
            .handle
            .insert_source(notifier, move |event, _, _state: &mut Aurora| match event {
                // Frame pacing arrives with the render loop.
                DrmEvent::VBlank(crtc) => tracing::trace!(?crtc, "vblank"),
                DrmEvent::Error(err) => tracing::error!(%err, "drm event error"),
            })
            .map_err(|err| format!("failed to register the drm source: {err}"))?;

        let Backend::Drm(drm) = &mut self.backend else {
            return Ok(());
        };
        drm.renderer = Some(renderer);
        drm.devices.insert(
            node,
            Device {
                surfaces: HashMap::new(),
                output_manager,
                scanner: DrmScanner::new(),
                render_node,
                registration_token,
            },
        );

        self.drm_device_changed(node);
        Ok(())
    }

    /// Rescans connectors and reacts to the differences.
    pub fn drm_device_changed(&mut self, node: DrmNode) {
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        let Some(device) = drm.devices.get_mut(&node) else {
            return;
        };
        let events = match device
            .scanner
            .scan_connectors(device.output_manager.device())
        {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(%err, %node, "connector scan failed");
                return;
            }
        };
        for event in events {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_connected(node, connector, crtc),
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_disconnected(node, &connector, crtc),
                DrmScanEvent::Connected {
                    connector,
                    crtc: None,
                } => tracing::warn!(
                    connector = %connector_name(&connector),
                    "connected but no free crtc, leaving it dark"
                ),
                _ => {}
            }
        }
    }

    pub fn drm_device_removed(&mut self, node: DrmNode) {
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        let Some(device) = drm.devices.get(&node) else {
            return;
        };
        let connectors: Vec<_> = device
            .scanner
            .crtcs()
            .map(|(info, crtc)| (info.clone(), crtc))
            .collect();
        for (connector, crtc) in connectors {
            self.connector_disconnected(node, &connector, crtc);
        }
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        if let Some(device) = drm.devices.remove(&node) {
            self.handle.remove(device.registration_token);
            tracing::info!(%node, "drm device removed");
        }
    }

    fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let name = connector_name(&connector);
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        let (Some(device), Some(renderer)) = (drm.devices.get_mut(&node), drm.renderer.as_mut())
        else {
            return;
        };

        for mode in connector.modes() {
            tracing::info!(
                connector = %name,
                mode = %format!("{}x{}@{:.3}", mode.size().0, mode.size().1, WlMode::from(*mode).refresh as f64 / 1000.0),
                preferred = mode.mode_type().contains(ModeTypeFlags::PREFERRED),
                "available mode"
            );
        }
        let Some(drm_mode) = pick_mode(&connector) else {
            tracing::warn!(connector = %name, "connector has no modes, skipping");
            return;
        };
        let wl_mode = WlMode::from(drm_mode);

        let drm_device = device.output_manager.device();
        let info = display_info::for_connector(drm_device, connector.handle());
        let make = info
            .as_ref()
            .and_then(|i| i.make())
            .unwrap_or_else(|| "Unknown".into());
        let model = info
            .as_ref()
            .and_then(|i| i.model())
            .unwrap_or_else(|| "Unknown".into());
        let serial = info
            .as_ref()
            .and_then(|i| i.serial())
            .unwrap_or_else(|| "Unknown".into());

        // Overlay planes misbehave on the proprietary NVIDIA driver.
        let is_nvidia = drm_device.get_driver().is_ok_and(|driver| {
            driver
                .name()
                .to_string_lossy()
                .to_lowercase()
                .contains("nvidia")
                || driver
                    .description()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains("nvidia")
        });
        let mut planes = match drm_device.planes(&crtc) {
            Ok(planes) => planes,
            Err(err) => {
                tracing::warn!(connector = %name, %err, "failed to query crtc planes");
                return;
            }
        };
        if is_nvidia {
            planes.overlay = Vec::new();
        }

        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: connector.subpixel().into(),
                make,
                model,
                serial_number: serial,
            },
        );

        let drm_output = match device
            .output_manager
            .lock()
            .initialize_output::<_, Element>(
                crtc,
                drm_mode,
                &[connector.handle()],
                &output,
                Some(planes),
                renderer,
                &DrmOutputRenderElements::default(),
            ) {
            Ok(drm_output) => drm_output,
            Err(err) => {
                tracing::warn!(connector = %name, %err, "failed to initialize drm output");
                return;
            }
        };

        let global = output.create_global::<Aurora>(&self.display_handle);
        // Lay outputs out left to right in discovery order.
        let x = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .map(|geo| geo.size.w)
            .sum::<i32>();
        let position = (x, 0);
        output.set_preferred(wl_mode);
        output.change_current_state(Some(wl_mode), None, None, Some(position.into()));
        self.space.map_output(&output, position);
        output.user_data().insert_if_missing(|| UdevOutputId {
            device_id: node,
            crtc,
        });

        tracing::info!(
            connector = %name, ?crtc, size = ?wl_mode.size, refresh_mhz = wl_mode.refresh,
            position = ?position, nvidia = is_nvidia, "output initialized"
        );
        device.surfaces.insert(
            crtc,
            Surface {
                drm_output,
                output,
                global: Some(global),
                dh: self.display_handle.clone(),
            },
        );
    }

    fn connector_disconnected(
        &mut self,
        node: DrmNode,
        connector: &connector::Info,
        crtc: crtc::Handle,
    ) {
        let name = connector_name(connector);
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        let (Some(device), Some(renderer)) = (drm.devices.get_mut(&node), drm.renderer.as_mut())
        else {
            return;
        };
        tracing::info!(connector = %name, ?crtc, "connector disconnected");
        if let Some(surface) = device.surfaces.remove(&crtc) {
            self.space.unmap_output(&surface.output);
            self.space.refresh();
            // Dropping the surface releases the crtc and removes the wl_output global.
            drop(surface);
        }
        // Black stand-in for the frame, so the remaining outputs re-modeset without glitching.
        if let Err(err) = device
            .output_manager
            .lock()
            .try_to_restore_modifiers::<_, Element>(renderer, &DrmOutputRenderElements::default())
        {
            tracing::debug!(%err, "could not restore modifiers after disconnect");
        }
    }
}

fn connector_name(connector: &connector::Info) -> String {
    format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    )
}

/// Highest refresh rate at the preferred resolution (or at the largest one when the
/// display flags none), so a 1080p@240 fallback never beats a 4K panel's native mode.
fn pick_mode(connector: &connector::Info) -> Option<smithay::reexports::drm::control::Mode> {
    let modes = connector.modes();
    let preferred = modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED));
    let area = |m: &smithay::reexports::drm::control::Mode| m.size().0 as u32 * m.size().1 as u32;
    let size = match preferred {
        Some(m) => m.size(),
        None => modes.iter().max_by_key(|m| area(m))?.size(),
    };
    modes
        .iter()
        .filter(|m| m.size() == size)
        .max_by_key(|m| {
            (
                WlMode::from(**m).refresh,
                m.mode_type().contains(ModeTypeFlags::PREFERRED),
            )
        })
        .copied()
}
