use smithay::{
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
        tracing::info!("session paused");
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        drm.libinput.suspend();
        drm.session_active = false;

        // The VT switch swallowed the releases; without them keys would stay stuck for clients.
        self.suppressed_keys.clear();
        let keyboard = self.seat.get_keyboard().unwrap();
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
        // DRM pause arrives with the DRM device in a later step.
    }

    fn activate_session(&mut self) {
        tracing::info!("session resumed");
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        if drm.libinput.resume().is_err() {
            tracing::error!("failed to resume libinput");
        }
        drm.session_active = true;

        let led_state = self.seat.get_keyboard().unwrap().led_state();
        for keyboard in &mut drm.keyboards {
            keyboard.led_update(led_state.into());
        }
        // DRM activation and re-render arrive with the DRM device in a later step.
    }
}
