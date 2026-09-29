mod cursor;
mod device;
pub mod gpu;
mod render;

use std::collections::HashMap;

use smithay::{
    backend::{
        drm::DrmNode,
        libinput::LibinputInputBackend,
        renderer::gles::GlesRenderer,
        session::{Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent},
    },
    reexports::{
        calloop::LoopHandle,
        input::{Device, Libinput},
    },
};

use crate::state::Aurora;

/// Everything the DRM session owns. Field order is drop order: outputs and devices
/// go first, the seat session last so the TTY is only released once nothing uses it.
pub struct DrmBackend {
    pub devices: HashMap<DrmNode, device::Device>,
    pub renderer: Option<GlesRenderer>,
    pub cursors: cursor::CursorCache,
    pub primary_gpu: DrmNode,
    pub libinput: Libinput,
    /// Keyboards seen by libinput, kept to push LED state to them.
    pub keyboards: Vec<Device>,
    /// False while another VT owns the seat; gates input and rendering.
    pub session_active: bool,
    pub seat_name: String,
    pub session: LibSeatSession,
}

impl DrmBackend {
    pub fn new(session: LibSeatSession, libinput: Libinput, primary_gpu: DrmNode) -> Self {
        // Launched from a non-active VT: stay dark until ActivateSession arrives.
        let session_active = session.is_active();
        if !session_active {
            tracing::warn!("seat is not active yet, waiting for the session to be enabled");
        }
        Self {
            devices: HashMap::new(),
            renderer: None,
            cursors: cursor::CursorCache::from_env(),
            primary_gpu,
            libinput,
            keyboards: Vec::new(),
            session_active,
            seat_name: session.seat(),
            session,
        }
    }

    pub fn input_source(&self) -> LibinputInputBackend {
        LibinputInputBackend::new(self.libinput.clone())
    }
}

/// Brings up the primary GPU, scans its connectors and starts watching for hotplug.
/// Renders nothing yet.
pub fn init(handle: &LoopHandle<'static, Aurora>, state: &mut Aurora) -> Result<(), String> {
    let seat = state.backend.seat_name();
    let udev = UdevBackend::new(&seat).map_err(|err| format!("udev backend failed: {err}"))?;

    for (id, path) in udev.device_list() {
        let node = match DrmNode::from_dev_id(id) {
            Ok(node) => node,
            Err(err) => {
                tracing::warn!(id, %err, "udev device is not a drm node");
                continue;
            }
        };
        if let Err(err) = state.drm_device_added(node, path) {
            tracing::error!(%node, %err, "failed to initialize drm device");
        }
    }

    let super::Backend::Drm(drm) = &state.backend else {
        return Ok(());
    };
    if drm.devices.is_empty() {
        return Err(format!(
            "the primary gpu {} could not be initialized",
            drm.primary_gpu
        ));
    }

    handle
        .insert_source(udev, |event, _, state| match event {
            UdevEvent::Added { device_id, path } => match DrmNode::from_dev_id(device_id) {
                Ok(node) => {
                    if let Err(err) = state.drm_device_added(node, &path) {
                        tracing::error!(%node, %err, "failed to initialize drm device");
                    }
                }
                Err(err) => tracing::warn!(device_id, %err, "udev device is not a drm node"),
            },
            UdevEvent::Changed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    state.drm_device_changed(node);
                }
            }
            UdevEvent::Removed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    state.drm_device_removed(node);
                }
            }
        })
        .map_err(|err| format!("failed to register the udev source: {err}"))?;
    Ok(())
}
