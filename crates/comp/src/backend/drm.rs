use smithay::{
    backend::{libinput::LibinputInputBackend, session::libseat::LibSeatSession},
    reexports::input::{Device, Libinput},
};

/// Seat session, libinput context and input-side state of the DRM backend.
pub struct DrmBackend {
    pub session: LibSeatSession,
    pub seat_name: String,
    pub libinput: Libinput,
    /// Keyboards seen by libinput, kept to push LED state to them.
    pub keyboards: Vec<Device>,
    /// False while another VT owns the seat; gates input and rendering.
    pub session_active: bool,
}

impl DrmBackend {
    pub fn new(session: LibSeatSession, libinput: Libinput) -> Self {
        use smithay::backend::session::Session;
        Self {
            seat_name: session.seat(),
            session,
            libinput,
            keyboards: Vec::new(),
            session_active: true,
        }
    }

    pub fn input_source(&self) -> LibinputInputBackend {
        LibinputInputBackend::new(self.libinput.clone())
    }
}
