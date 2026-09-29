mod compositor;
mod layer_shell;
mod xdg_shell;
mod xwm;

use crate::{Aurora, focus::FocusTarget};

//
// Wl Seat
//

use smithay::input::dnd::{DnDGrab, DndGrabHandler, GrabType, Source};
use smithay::input::pointer::{CursorImageStatus, Focus};
use smithay::input::tablet::TabletSeatHandler;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Serial;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::PointerConstraintsHandler;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use std::os::fd::OwnedFd;

impl SeatHandler for Aurora {
    type KeyboardFocus = FocusTarget;
    type PointerFocus = FocusTarget;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Aurora> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.queue_redraw_all();
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&FocusTarget>) {
        let dh = &self.display_handle;
        let client = focused
            .and_then(|f| f.wl_surface())
            .and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}

impl TabletSeatHandler for Aurora {
    type ToolFocus = WlSurface;
}

impl PointerConstraintsHandler for Aurora {}

//
// Wl Data Device
//

impl SelectionHandler for Aurora {
    type SelectionUserData = ();

    /// A Wayland client took a selection: X clients see it through the bridge.
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if let Some(xwm) = self.xwayland.wm.as_mut()
            && let Err(err) = xwm.new_selection(ty, source.map(|s| s.mime_types()))
        {
            tracing::warn!("xwayland: cannot set the {ty:?} selection: {err}");
        }
    }

    /// An X client reads a Wayland selection.
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if let Some(xwm) = self.xwayland.wm.as_mut()
            && let Err(err) = xwm.send_selection(ty, mime_type, fd)
        {
            tracing::warn!("xwayland: cannot send the {ty:?} selection: {err}");
        }
    }
}

impl PrimarySelectionHandler for Aurora {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.protocols.primary_selection
    }
}

impl DataDeviceHandler for Aurora {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for Aurora {}
impl WaylandDndGrabHandler for Aurora {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        match type_ {
            GrabType::Pointer => {
                let Some(ptr) = seat.get_pointer() else {
                    source.cancel();
                    return;
                };
                let Some(start_data) = ptr.grab_start_data() else {
                    source.cancel();
                    return;
                };

                // create a dnd grab to start the operation
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                ptr.set_grab(self, grab, serial, Focus::Keep);
            }
            GrabType::Touch => {
                // touch is not supported yet
                source.cancel();
            }
        }
    }
}

//
// Wl Output & Xdg Output
//

impl OutputHandler for Aurora {}

smithay::delegate_dispatch2!(Aurora);
