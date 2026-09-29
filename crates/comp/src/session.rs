use smithay::{
    backend::drm::DrmError,
    backend::input::KeyState,
    backend::session::{
        Event as SessionEvent,
        libseat::{LibSeatSession, LibSeatSessionNotifier},
    },
    input::keyboard::FilterResult,
    reexports::calloop::LoopHandle,
    utils::SERIAL_COUNTER,
};

use crate::{backend::Backend, state::Aurora};

/// Opens the seat session. Must run before anything else touches devices, so a
/// failure exits with nothing to clean up.
pub fn open() -> Result<(LibSeatSession, LibSeatSessionNotifier), Box<dyn std::error::Error>> {
    LibSeatSession::new().map_err(|err| {
        tracing::error!(
            %err,
            "could not open a libseat session: seatd must be running (systemctl start seatd, \
             user in the seat group) or logind needs a real TTY login; nothing was touched"
        );
        format!("libseat session failed: {err}").into()
    })
}

/// Registers the notifier; this must be the first source so pause/resume is never starved.
pub fn insert_notifier(
    handle: &LoopHandle<'static, Aurora>,
    notifier: LibSeatSessionNotifier,
) -> Result<(), Box<dyn std::error::Error>> {
    handle
        .insert_source(notifier, |event, &mut (), state| match event {
            SessionEvent::PauseSession => state.pause_session(),
            SessionEvent::ActivateSession => state.activate_session(),
        })
        .map_err(|err| format!("failed to register the session source: {err}"))?;
    Ok(())
}

impl Aurora {
    fn pause_session(&mut self) {
        tracing::info!("session disabled (VT switched away or seat taken)");
        let handle = self.handle.clone();
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        drm.libinput.suspend();
        drm.session_active = false;
        for device in drm.devices.values_mut() {
            device.output_manager.pause();
            // Nothing may render or reschedule until the session comes back; every
            // output restarts from scratch on resume.
            for surface in device.surfaces.values_mut() {
                surface.render.cancel(&handle);
            }
        }

        // The VT switch swallowed the releases; without them keys would stay stuck for clients.
        self.suppressed_keys.clear();
        let keyboard = self.keyboard.clone();
        for keycode in keyboard.pressed_keys() {
            keyboard.input::<(), _>(
                self,
                keycode,
                KeyState::Released,
                SERIAL_COUNTER.next_serial(),
                smithay::backend::input::InputTime::now(),
                |_, _, _| FilterResult::Forward,
            );
        }
        let _ = self.display_handle.flush_clients();
    }

    fn activate_session(&mut self) {
        tracing::info!("session enabled");
        let led_state = self.keyboard.led_state();
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        if let Err(err) = drm.libinput.resume() {
            tracing::error!(?err, "failed to resume libinput");
        }
        drm.session_active = true;
        for keyboard in &mut drm.keyboards {
            keyboard.led_update(led_state.into());
        }

        let mut nodes = Vec::new();
        for (node, device) in &mut drm.devices {
            nodes.push(*node);
            let mut manager = device.output_manager.lock();
            // Optimistic first: keep the hardware state and let a failed test reset it
            // at the next frame. Only if that fails, pay for a full modeset.
            if let Err(err) = manager.activate(false) {
                tracing::warn!(%node, %err, "drm activate failed, retrying with connectors disabled");
                if let Err(err) = manager.activate(true) {
                    tracing::error!(%node, %err, "drm activate failed again");
                    if matches!(err, DrmError::TestFailed(_))
                        && let Err(err) = manager.device_mut().reset_state()
                    {
                        tracing::error!(%node, %err, "failed to reset the drm device");
                    }
                }
            }
        }

        // Connectors may have come or gone while another VT owned the display.
        for node in nodes {
            self.drm_device_changed(node);
        }
        self.resume_rendering();
    }
}
