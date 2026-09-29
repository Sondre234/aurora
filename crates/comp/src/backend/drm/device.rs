use std::{collections::HashMap, path::Path};

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmNode,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDevice, EGLDisplay, context::ContextPriority},
        renderer::{ImportDma, ImportMemWl, gles::GlesRenderer},
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

use super::render::{RenderState, vblank_handler};
use crate::{
    backend::Backend,
    config::ModeSpec,
    dmabuf::{SurfaceDmabufFeedback, surface_feedback},
    outputs::choose_mode,
    state::Aurora,
    wm::outputs::rule_scale,
};

pub type Allocator = GbmAllocator<DrmDeviceFd>;
pub type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
pub type Feedback = Option<OutputPresentationFeedback>;
pub type OutputManager = DrmOutputManager<Allocator, Exporter, Feedback, DrmDeviceFd>;
pub type ConnectorOutput = DrmOutput<Allocator, Exporter, Feedback, DrmDeviceFd>;
/// Element type the DRM compositor is instantiated with; the render loop uses the same.
pub type Element = crate::scene::OutputElement;

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

pub struct UdevOutputId {
    pub device_id: DrmNode,
    pub crtc: crtc::Handle,
}

/// A live connector. Field order matters: the DRM output goes before its global goes away.
pub struct Surface {
    pub drm_output: ConnectorOutput,
    pub output: Output,
    pub render: RenderState,
    pub dmabuf_feedback: Option<SurfaceDmabufFeedback>,
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
    /// Connectors the config turned off, kept so a reload can turn them on again.
    disabled: HashMap<crtc::Handle, connector::Info>,
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
        tracing::info!(
            path = %path.display(),
            session_active = drm.session.is_active(),
            "opened drm device through libseat"
        );
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

        self.init_dmabuf(Some(render_node), renderer.dmabuf_formats());
        self.init_syncobj(output_manager.device().device_fd().clone());

        let registration_token = self
            .handle
            .insert_source(notifier, vblank_handler(node))
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
                disabled: HashMap::new(),
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
        // activate_session rescans; scanning now would record connectors we cannot initialize.
        if !drm.session_active {
            return;
        }
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

    /// The connector that stays on when the config disables every connected one, so a bad
    /// config or reload never leaves the session without a display: the primary, else the
    /// first by name.
    fn forced_output(&self) -> Option<String> {
        let Backend::Drm(drm) = &self.backend else {
            return None;
        };
        let mut names: Vec<String> = drm
            .devices
            .values()
            .flat_map(|d| d.scanner.crtcs().map(|(info, _)| connector_name(info)))
            .collect();
        if names
            .iter()
            .any(|n| self.output_rule(n).is_none_or(|r| r.enabled))
        {
            return None;
        }
        names.sort();
        names
            .iter()
            .find(|n| self.output_rule(n).is_some_and(|r| r.primary))
            .or(names.first())
            .cloned()
    }

    fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let name = connector_name(&connector);
        let rule = self.output_rule(&name).cloned();
        let forced = self.forced_output().as_deref() == Some(name.as_str());
        if forced {
            tracing::warn!("output: {name} stays on although the config disables every output");
        }
        if !forced && rule.as_ref().is_some_and(|r| !r.enabled) {
            tracing::info!("output: {name} disabled by config");
            if let Backend::Drm(drm) = &mut self.backend
                && let Some(device) = drm.devices.get_mut(&node)
            {
                device.disabled.insert(crtc, connector);
            }
            return;
        }
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
        let configured = configured_mode(
            &connector,
            rule.as_ref().and_then(|r| r.mode.as_ref()),
            &name,
        );
        let Some(drm_mode) = configured.or_else(|| pick_mode(&connector)) else {
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
        // The DRM compositor refuses an output without a current mode.
        output.set_preferred(wl_mode);
        output.change_current_state(Some(wl_mode), None, Some(rule_scale(rule.as_ref())), None);

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

        let dmabuf_feedback = drm_output.with_compositor(|c| {
            surface_feedback(device.render_node, &renderer.dmabuf_formats(), c.surface())
        });
        let global = output.create_global::<Aurora>(&self.display_handle);
        output.user_data().insert_if_missing(|| UdevOutputId {
            device_id: node,
            crtc,
        });

        tracing::info!(
            connector = %name, ?crtc, size = ?wl_mode.size, refresh_mhz = wl_mode.refresh,
            nvidia = is_nvidia, "output initialized"
        );
        // The first frame is queued on the loop, after this setup has finished.
        let mut render = RenderState::default();
        render.damage(&self.handle, node, crtc);
        let registered = output.clone();
        device.surfaces.insert(
            crtc,
            Surface {
                drm_output,
                output,
                global: Some(global),
                render,
                dmabuf_feedback,
                dh: self.display_handle.clone(),
            },
        );
        self.add_output(&registered);
    }

    fn connector_disconnected(
        &mut self,
        node: DrmNode,
        connector: &connector::Info,
        crtc: crtc::Handle,
    ) {
        let name = connector_name(connector);
        tracing::info!(connector = %name, ?crtc, "connector disconnected");
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        let Some(device) = drm.devices.get_mut(&node) else {
            return;
        };
        device.disabled.remove(&crtc);
        if let Some(mut surface) = device.surfaces.remove(&crtc) {
            // Pending repaints and vblank timers must not outlive the output.
            surface.render.cancel(&self.handle);
            // Windows move to another output while this one is still alive.
            self.wm_output_removed(&surface.output);
            // Dropping the surface releases the crtc and removes the wl_output global.
            drop(surface);
        }
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        let (Some(device), Some(renderer)) = (drm.devices.get_mut(&node), drm.renderer.as_mut())
        else {
            return;
        };
        // Black stand-in for the frame, so the remaining outputs re-modeset without glitching.
        if let Err(err) = device
            .output_manager
            .lock()
            .try_to_restore_modifiers::<_, Element>(renderer, &DrmOutputRenderElements::default())
        {
            tracing::debug!(%err, "could not restore modifiers after disconnect");
        }
        self.queue_redraw_all();
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

/// The mode a config rule asks for, or `None` (with a warning) when the connector has no
/// mode of that size.
fn configured_mode(
    connector: &connector::Info,
    spec: Option<&ModeSpec>,
    name: &str,
) -> Option<smithay::reexports::drm::control::Mode> {
    let spec = spec?;
    let modes = connector.modes();
    let list: Vec<_> = modes
        .iter()
        .map(|m| {
            (
                m.size().0 as i32,
                m.size().1 as i32,
                WlMode::from(*m).refresh,
            )
        })
        .collect();
    let mode = choose_mode(&list, spec).and_then(|i| modes.get(i)).copied();
    if mode.is_none() {
        tracing::warn!(
            "output: {name} has no {}x{} mode, using the default one",
            spec.width,
            spec.height
        );
    }
    mode
}

impl Aurora {
    /// Applies a reloaded config to live connectors: disables or re-enables them and switches
    /// modes that changed. A mode the driver refuses keeps the old one. Not exercised by the
    /// nested backend, so it is checked by reading only.
    pub fn drm_apply_output_config(&mut self) {
        let config = self.config.clone();
        let mut disable = Vec::new();
        let mut enable = Vec::new();
        let mut mode_changed = false;
        let forced = self.forced_output();
        {
            let Backend::Drm(drm) = &mut self.backend else {
                return;
            };
            if !drm.session_active {
                return;
            }
            let drm = &mut **drm;
            for (node, device) in drm.devices.iter_mut() {
                let connectors: Vec<_> = device
                    .scanner
                    .crtcs()
                    .map(|(info, crtc)| (info.clone(), crtc))
                    .collect();
                for (info, crtc) in connectors {
                    let name = connector_name(&info);
                    let rule = config.outputs.iter().find(|r| r.name == name);
                    let enabled =
                        rule.is_none_or(|r| r.enabled) || forced.as_deref() == Some(name.as_str());
                    let live = device.surfaces.contains_key(&crtc);
                    if live && !enabled {
                        disable.push((*node, info, crtc));
                        continue;
                    }
                    if !live && enabled && device.disabled.contains_key(&crtc) {
                        enable.push((*node, info, crtc));
                        continue;
                    }
                    let (Some(surface), Some(renderer)) =
                        (device.surfaces.get_mut(&crtc), drm.renderer.as_mut())
                    else {
                        continue;
                    };
                    let want = configured_mode(&info, rule.and_then(|r| r.mode.as_ref()), &name)
                        .or_else(|| pick_mode(&info));
                    let Some(want) = want else { continue };
                    let wl_mode = WlMode::from(want);
                    if surface.output.current_mode() == Some(wl_mode) {
                        continue;
                    }
                    match surface.drm_output.use_mode::<_, Element>(
                        want,
                        renderer,
                        &DrmOutputRenderElements::default(),
                    ) {
                        Ok(()) => {
                            surface
                                .output
                                .change_current_state(Some(wl_mode), None, None, None);
                            tracing::info!(
                                "output: mode name={name} {}x{}@{}",
                                wl_mode.size.w,
                                wl_mode.size.h,
                                wl_mode.refresh
                            );
                            mode_changed = true;
                        }
                        Err(err) => {
                            tracing::warn!(
                                "output: cannot switch {name} to the new mode, keeping the old one: {err}"
                            )
                        }
                    }
                }
            }
        }
        for (node, info, crtc) in disable {
            self.connector_disconnected(node, &info, crtc);
            if let Backend::Drm(drm) = &mut self.backend
                && let Some(device) = drm.devices.get_mut(&node)
            {
                tracing::info!("output: {} disabled by config", connector_name(&info));
                device.disabled.insert(crtc, info);
            }
        }
        for (node, info, crtc) in enable {
            if let Backend::Drm(drm) = &mut self.backend
                && let Some(device) = drm.devices.get_mut(&node)
            {
                device.disabled.remove(&crtc);
            }
            self.connector_connected(node, info, crtc);
        }
        if mode_changed {
            self.arrange_outputs();
        }
    }
}
