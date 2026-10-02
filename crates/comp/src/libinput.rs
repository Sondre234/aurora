use smithay::{
    backend::{
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        session::libseat::LibSeatSession,
    },
    reexports::{
        calloop::LoopHandle,
        input::{AccelProfile, Device, DeviceCapability, Libinput, ScrollMethod},
    },
};

use crate::{backend::Backend, config, state::Aurora};

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
        configure_device(device, &self.config.input);
        self.input.devices.push(device.clone());
        if device.has_capability(DeviceCapability::Keyboard)
            && let Backend::Drm(drm) = &mut self.backend
        {
            device.led_update(self.keyboard.clone().led_state().into());
            drm.keyboards.push(device.clone());
        }
    }

    fn device_removed(&mut self, device: &Device) {
        tracing::info!(name = %device.name(), "input device removed");
        self.input.devices.retain(|d| d != device);
        if let Backend::Drm(drm) = &mut self.backend {
            drm.keyboards.retain(|d| d != device);
        }
    }
}

impl Aurora {
    /// Applies `[input.pointer]` / `[input.touchpad]` to every device already there after a
    /// reload changed them; new devices get the current settings in `device_added`.
    pub fn reapply_device_config(&mut self, old: &config::Input) {
        let input = &self.config.input;
        if old.pointer == input.pointer && old.touchpad == input.touchpad {
            return;
        }
        for device in &mut self.input.devices {
            configure_device(device, input);
        }
        tracing::info!(
            devices = self.input.devices.len(),
            "input: device settings applied"
        );
    }
}

/// `[input.touchpad]` for touchpads (the only devices with tap fingers), `[input.pointer]`
/// for every other pointer. What a device does not support is skipped.
fn configure_device(device: &mut Device, input: &config::Input) {
    if !device.has_capability(DeviceCapability::Pointer) {
        return;
    }
    let touchpad = device.config_tap_finger_count() > 0;
    let p = if touchpad {
        &input.touchpad
    } else {
        &input.pointer
    };
    if device.config_accel_is_available() {
        let profile = match p.accel_profile {
            config::AccelProfile::Flat => AccelProfile::Flat,
            config::AccelProfile::Adaptive => AccelProfile::Adaptive,
        };
        let _ = device.config_accel_set_profile(profile);
        let _ = device.config_accel_set_speed(p.accel_speed);
    }
    if device.config_scroll_has_natural_scroll() {
        let _ = device.config_scroll_set_natural_scroll_enabled(p.natural_scroll);
    }
    if device.config_left_handed_is_available() {
        let _ = device.config_left_handed_set(p.left_handed);
    }
    let method = match p.scroll_method {
        None => device.config_scroll_default_method(),
        Some(config::ScrollMethod::None) => Some(ScrollMethod::NoScroll),
        Some(config::ScrollMethod::TwoFinger) => Some(ScrollMethod::TwoFinger),
        Some(config::ScrollMethod::Edge) => Some(ScrollMethod::Edge),
        Some(config::ScrollMethod::OnButtonDown) => Some(ScrollMethod::OnButtonDown),
    };
    if let Some(method) = method
        && device.config_scroll_methods().contains(&method)
    {
        let _ = device.config_scroll_set_method(method);
    }
    if touchpad {
        let _ = device.config_tap_set_enabled(p.tap);
        if device.config_dwt_is_available() {
            let _ = device.config_dwt_set_enabled(p.dwt);
        }
    }
}
