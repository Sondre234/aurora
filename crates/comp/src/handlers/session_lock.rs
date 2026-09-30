//! ext-session-lock: the handler hands everything to `Aurora`'s lock methods (see `lock`).
use smithay::{
    reexports::wayland_server::protocol::wl_output::WlOutput,
    wayland::session_lock::{
        LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
    },
};

use crate::Aurora;

impl SessionLockHandler for Aurora {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.protocols.session_lock
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        self.lock_request(confirmation);
    }

    fn unlock(&mut self) {
        self.lock_unlock();
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        self.lock_new_surface(surface, output);
    }
}
