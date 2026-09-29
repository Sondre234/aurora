use smithay::{
    backend::{
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        session::libseat::LibSeatSession,
    },
    reexports::{
        calloop::LoopHandle,
        input::{AccelProfile, Device, DeviceCapability, Libinput},
    },
};

use crate::{backend::Backend, state::Aurora};

/// Creates the libinput context bound to the session's seat.
pub fn new_context(
    session: &LibSeatSession,
    seat: &str,
) -> Result<Libinput, Box<dyn std::error::Error>> {
    let mut context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    context
        .udev_assign_seat(seat)
        .map_err(|()| format!("libinput could not assign seat {seat}"))?;
    Ok(context)
}

pub fn insert_source(
    handle: &LoopHandle<'static, Aurora>,
    source: LibinputInputBackend,
) -> Result<(), Box<dyn std::error::Error>> {
    handle
        .insert_source(source, |mut event, _, state| {
            match &mut event {
                InputEvent::DeviceAdded { device } => state.device_added(device),
                InputEvent::DeviceRemoved { device } => state.device_removed(device),
                _ => {}
            }
            state.process_input_event(event);
        })
        .map_err(|err| format!("failed to register libinput: {err}"))?;
    Ok(())
}

impl Aurora {
    fn device_added(&mut self, device: &mut Device) {
        tracing::info!(name = %device.name(), "input device added");
        configure_device(device);
        if device.has_capability(DeviceCapability::Keyboard)
            && let Backend::Drm(drm) = &mut self.backend
        {
            device.led_update(self.keyboard.clone().led_state().into());
            drm.keyboards.push(device.clone());
        }
    }

    fn device_removed(&mut self, device: &Device) {
        tracing::info!(name = %device.name(), "input device removed");
        if let Backend::Drm(drm) = &mut self.backend {
            drm.keyboards.retain(|d| d != device);
        }
    }
}

/// Sane defaults: adaptive acceleration on pointers, tap-to-click and
/// disable-while-typing on touchpads, no natural scrolling.
fn configure_device(device: &mut Device) {
    if device.has_capability(DeviceCapability::Pointer) {
        if device.config_accel_is_available() {
            let _ = device.config_accel_set_profile(AccelProfile::Adaptive);
            let _ = device.config_accel_set_speed(0.0);
        }
        // Only touchpads report tap fingers.
        if device.config_tap_finger_count() > 0 {
            let _ = device.config_tap_set_enabled(true);
            let _ = device.config_scroll_set_natural_scroll_enabled(false);
            let _ = device.config_dwt_set_enabled(true);
        }
    }
}
