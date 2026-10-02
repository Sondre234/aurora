//! The DRM half of display control. Not exercised by the nested backend, so it is kept to
//! the same few calls niri makes.
//!
//! - Power off: the DRM compositor clears the CRTC (planes off, connectors detached,
//!   ACTIVE=0) and the output stops rendering; the next queued frame re-enables it with a
//!   full modeset. Meanwhile a 1 s timer stands in for its vblank (FIFO barriers, frame
//!   callbacks), so clients slow down instead of blocking until the output wakes.
//! - Gamma: the atomic `GAMMA_LUT` blob when the driver has it, else the legacy ramp. The
//!   wanted ramp is kept on the surface and pushed again after a session resume or power on,
//!   since another DRM master may have changed it meanwhile.
//! - VRR: `VRR_ENABLED` through the DRM compositor, toggled before the frame that needs it.
use std::time::Duration;

use smithay::{
    backend::{
        drm::{DrmNode, VrrSupport},
        renderer::element::RenderElementStates,
    },
    output::Output,
    reexports::{
        calloop::{
            LoopHandle, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{connector, crtc},
    },
};

use smithay_drm_extras::display_info;

use super::device::{ConnectorOutput, Surface, UdevOutputId, connector_name};
use crate::{
    backend::Backend,
    config::VrrMode,
    display::{
        output_management::{HeadState, ModeInfo},
        vrr::{self, Capability},
    },
    state::Aurora,
};

/// How often an output that is off stands in for its vblank.
const OFF_TICK: Duration = Duration::from_secs(1);

pub(super) fn arm_off_tick(
    handle: &LoopHandle<'static, Aurora>,
    node: DrmNode,
    crtc: crtc::Handle,
) -> Option<RegistrationToken> {
    let timer = Timer::from_duration(OFF_TICK);
    match handle.insert_source(timer, move |_, _, state| state.off_tick(node, crtc)) {
        Ok(token) => Some(token),
        Err(err) => {
            tracing::warn!("power: cannot arm the off tick: {err}");
            None
        }
    }
}

pub fn vrr_capability(drm_output: &ConnectorOutput, connector: connector::Handle) -> Capability {
    match drm_output.with_compositor(|c| c.vrr_supported(connector)) {
        Ok(VrrSupport::Supported) => Capability::Supported,
        Ok(VrrSupport::RequiresModeset) => Capability::RequiresModeset,
        Ok(VrrSupport::NotSupported) => Capability::Unsupported,
        Err(err) => {
            tracing::debug!("vrr: capability unknown: {err}");
            Capability::Unsupported
        }
    }
}

/// Brings the CRTC's VRR in line with the policy before a frame is built; the change rides
/// on that frame's commit. True when it changed.
pub fn sync_vrr(surface: &mut Surface, mode: VrrMode, fullscreen: bool) -> bool {
    let want = vrr::target(mode, fullscreen, surface.vrr);
    if surface.drm_output.with_compositor(|c| c.vrr_enabled()) == want {
        surface.vrr_refused = None;
        return false;
    }
    if surface.vrr_refused == Some(want) {
        return false;
    }
    let name = surface.output.name();
    match surface.drm_output.with_compositor(|c| c.use_vrr(want)) {
        Ok(()) => {
            surface.vrr_refused = None;
            tracing::info!("vrr: output={name} {}", if want { "on" } else { "off" });
            true
        }
        Err(err) => {
            surface.vrr_refused = Some(want);
            tracing::warn!("vrr: output={name} refused {want}: {err}");
            false
        }
    }
}

impl Aurora {
    /// Every connector with a CRTC, live or disabled, as an output-management head.
    pub fn drm_heads(&self) -> Vec<HeadState> {
        let Backend::Drm(drm) = &self.backend else {
            return Vec::new();
        };
        let mut heads = Vec::new();
        for device in drm.devices.values() {
            for (info, crtc) in device.scanner.crtcs() {
                if let Some(surface) = device.surfaces.get(&crtc) {
                    let mut head = HeadState::from_output(&surface.output, true);
                    // The connector's own list, so every mode is offered, not only the ones
                    // the output was told about.
                    let current = head.current_mode.map(|i| head.modes[i]);
                    head.modes = connector_modes(info);
                    head.current_mode = current.and_then(|c| {
                        head.modes.iter().position(|m| {
                            (m.width, m.height, m.refresh_mhz) == (c.width, c.height, c.refresh_mhz)
                        })
                    });
                    head.adaptive_sync = surface.drm_output.with_compositor(|c| c.vrr_enabled());
                    head.vrr_capable = surface.vrr != Capability::Unsupported;
                    heads.push(head);
                    continue;
                }
                let name = connector_name(info);
                let edid =
                    display_info::for_connector(device.output_manager.device(), info.handle());
                let field = |f: Option<String>| f.unwrap_or_else(|| "Unknown".into());
                let make = field(edid.as_ref().and_then(|i| i.make()));
                let model = field(edid.as_ref().and_then(|i| i.model()));
                let serial = field(edid.as_ref().and_then(|i| i.serial()));
                let (w, h) = info.size().unwrap_or((0, 0));
                heads.push(HeadState {
                    description: format!("{make} - {model} - {name}"),
                    name,
                    make,
                    model,
                    serial,
                    physical_size: (w as i32, h as i32),
                    modes: connector_modes(info),
                    enabled: false,
                    current_mode: None,
                    position: (0, 0),
                    transform: smithay::utils::Transform::Normal,
                    scale: 1.0,
                    adaptive_sync: false,
                    can_modeset: true,
                    // Unknown until it is lit; refusing VRR here would be a guess.
                    vrr_capable: true,
                });
            }
        }
        heads
    }

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
            // Pushed at the vblank of that frame.
            surface.gamma_pending |= surface.gamma.is_some();
        } else {
            // Inactive: the resume path clears it once the device is ours again.
            if session_active
                && let Err(err) = surface.drm_output.with_compositor(|c| c.clear())
            {
                tracing::warn!("power: cannot clear {}: {err}", output.name());
            }
            surface.render.off_tick = arm_off_tick(&handle, id.device_id, id.crtc);
        }
    }

    /// One tick of an output that is off: what its vblank would do, minus the frame. FIFO
    /// barriers of what it last showed are released and frame callbacks go out, so clients
    /// crawl along at one frame per tick instead of stalling until the output wakes.
    fn off_tick(&mut self, node: DrmNode, crtc: crtc::Handle) -> TimeoutAction {
        let Backend::Drm(drm) = &mut self.backend else {
            return TimeoutAction::Drop;
        };
        let session_active = drm.session_active;
        let Some(surface) = drm
            .devices
            .get_mut(&node)
            .and_then(|d| d.surfaces.get_mut(&crtc))
        else {
            return TimeoutAction::Drop;
        };
        let output = surface.output.clone();
        if !crate::display::power::is_off(&output) {
            surface.render.off_tick = None;
            return TimeoutAction::Drop;
        }
        if session_active {
            self.latch_fifo_barriers(&output);
            self.signal_fifo_barriers(&output);
            let now = std::time::Duration::from(self.clock.now());
            self.post_repaint(&output, now, None, &RenderElementStates::default());
            let _ = self.display_handle.flush_clients();
        }
        TimeoutAction::ToDuration(OFF_TICK)
    }

    /// Entries per channel of the output's gamma ramp; `None` when it has none.
    pub fn drm_gamma_size(&self, output: &Output) -> Option<u32> {
        let id = output.user_data().get::<UdevOutputId>().copied()?;
        let Backend::Drm(drm) = &self.backend else {
            return None;
        };
        let device = drm.devices.get(&id.device_id)?;
        gamma::size(device.output_manager.device(), id.crtc)
    }

    /// Remembers the ramp (`None` restores the default) and pushes it to the hardware now
    /// when possible, else at the next vblank or session resume. `Err` only when the
    /// hardware refused it outright.
    pub fn drm_set_gamma(&mut self, output: &Output, ramp: Option<Vec<u16>>) -> Result<(), String> {
        let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
            return Err("not a drm output".into());
        };
        let Backend::Drm(drm) = &mut self.backend else {
            return Err("not a drm output".into());
        };
        let Some(surface) = drm
            .devices
            .get_mut(&id.device_id)
            .and_then(|d| d.surfaces.get_mut(&id.crtc))
        else {
            return Err("the output is gone".into());
        };
        surface.gamma = ramp;
        surface.gamma_pending = true;
        self.drm_push_gamma(id.device_id, id.crtc)
    }

    /// Pushes a pending ramp. The commit can collide with a page flip in flight (EBUSY); it
    /// then stays pending for the next vblank. Any other error gives up on that ramp.
    pub fn drm_push_gamma(&mut self, node: DrmNode, crtc: crtc::Handle) -> Result<(), String> {
        let Backend::Drm(drm) = &mut self.backend else {
            return Ok(());
        };
        // Resume sets it pending again.
        if !drm.session_active {
            return Ok(());
        }
        let Some(device) = drm.devices.get_mut(&node) else {
            return Ok(());
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return Ok(());
        };
        if !surface.gamma_pending {
            return Ok(());
        }
        let dev = device.output_manager.device();
        let name = surface.output.name();
        if gamma::size(dev, crtc).is_none() {
            surface.gamma_pending = false;
            return Err(format!("{name} has no gamma ramp"));
        }
        match gamma::set(dev, crtc, surface.gamma.as_deref()) {
            Ok(()) => {
                surface.gamma_pending = false;
                let what = if surface.gamma.is_some() {
                    "set"
                } else {
                    "reset"
                };
                tracing::debug!("gamma: output={name} {what}");
                Ok(())
            }
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) => {
                tracing::debug!("gamma: {name} busy, retrying at the next vblank");
                Ok(())
            }
            Err(err) => {
                surface.gamma_pending = false;
                Err(format!("cannot set the gamma of {name}: {err}"))
            }
        }
    }

    /// After a vblank: a ramp that collided with the flip goes out now.
    pub fn drm_retry_gamma(&mut self, node: DrmNode, crtc: crtc::Handle) {
        if let Err(err) = self.drm_push_gamma(node, crtc) {
            tracing::warn!("gamma: {err}");
        }
    }
}

/// Hardware gamma of one CRTC: the atomic `GAMMA_LUT` blob when the driver has it (with its
/// size from `GAMMA_LUT_SIZE`), else the legacy per-CRTC ramp.
mod gamma {
    use std::{io, os::fd::AsFd};

    use smithay::{
        backend::drm::DrmDevice,
        reexports::drm::control::{
            AtomicCommitFlags, Device as _, atomic::AtomicModeReq, crtc, property,
        },
    };

    /// `(GAMMA_LUT, GAMMA_LUT_SIZE value)` of the CRTC, when the driver exposes both.
    fn lut_props(dev: &DrmDevice, crtc: crtc::Handle) -> Option<(property::Handle, u64)> {
        let props = dev.get_properties(crtc).ok()?;
        let (mut lut, mut size) = (None, None);
        for (handle, value) in props.iter() {
            let Ok(info) = dev.get_property(*handle) else {
                continue;
            };
            match info.name().to_str() {
                Ok("GAMMA_LUT") => lut = Some(*handle),
                Ok("GAMMA_LUT_SIZE") => size = Some(*value),
                _ => {}
            }
        }
        Some((lut?, size?))
    }

    /// Entries per channel, `None` when the CRTC has no gamma at all.
    pub fn size(dev: &DrmDevice, crtc: crtc::Handle) -> Option<u32> {
        if dev.is_atomic()
            && let Some((_, size)) = lut_props(dev, crtc)
        {
            return u32::try_from(size).ok().filter(|n| *n > 0);
        }
        dev.get_crtc(crtc)
            .ok()
            .map(|c| c.gamma_length())
            .filter(|n| *n > 0)
    }

    /// Sets the ramp (red, green and blue tables of `size` entries back to back), or the
    /// driver default for `None`.
    pub fn set(dev: &DrmDevice, crtc: crtc::Handle, ramp: Option<&[u16]>) -> io::Result<()> {
        if dev.is_atomic()
            && let Some((lut, size)) = lut_props(dev, crtc)
        {
            let blob = match ramp {
                Some(ramp) => {
                    let mut data = crate::display::gamma::lut_bytes(ramp, size as usize)
                        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                    drm_ffi::mode::create_property_blob(dev.as_fd(), &mut data)?.blob_id
                }
                None => 0,
            };
            let mut req = AtomicModeReq::new();
            req.add_property(crtc, lut, property::Value::Blob(u64::from(blob)));
            let result = dev.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req);
            // The CRTC state holds its own reference; ours is not needed past the commit.
            if blob != 0
                && let Err(err) = dev.destroy_property_blob(u64::from(blob))
            {
                tracing::debug!("gamma: cannot destroy blob {blob}: {err}");
            }
            return result;
        }
        let len = dev.get_crtc(crtc)?.gamma_length() as usize;
        let linear;
        let ramp = match ramp {
            Some(ramp) => ramp,
            None => {
                linear = crate::display::gamma::linear_ramp(len);
                &linear
            }
        };
        if ramp.len() != len * 3 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let (red, rest) = ramp.split_at(len);
        let (green, blue) = rest.split_at(len);
        dev.set_gamma(crtc, red, green, blue)
    }
}

fn connector_modes(info: &connector::Info) -> Vec<ModeInfo> {
    info.modes()
        .iter()
        .map(|m| {
            let wl = smithay::output::Mode::from(*m);
            ModeInfo {
                width: wl.size.w,
                height: wl.size.h,
                refresh_mhz: wl.refresh,
                preferred: m
                    .mode_type()
                    .contains(smithay::reexports::drm::control::ModeTypeFlags::PREFERRED),
            }
        })
        .collect()
}
