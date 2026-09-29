//! The X11 window manager callbacks and the selection bridge between X11 and Wayland. The
//! window logic itself is in `wm/x11.rs`.
use std::os::fd::OwnedFd;

use aurora_layout::FsMode;
use smithay::{
    utils::{Logical, Rectangle},
    wayland::{
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XwmHandler,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId},
    },
};

use crate::{Aurora, focus::FocusTarget, wm::grabs::DragKind};

impl XWaylandShellHandler for Aurora {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.protocols.xwayland_shell
    }

    fn surface_associated(
        &mut self,
        _xwm: XwmId,
        surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        window: X11Surface,
    ) {
        self.x11_surface_associated(&surface, &window);
    }
}

impl XwmHandler for Aurora {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        // In-flight selection transfers outlive their manager (see `XWaylandState::retired`);
        // a stale id finds no transfer in whichever manager answers and the source is removed.
        let x = &mut self.xwayland;
        match (x.wm.as_mut(), x.retired.as_mut()) {
            (Some(wm), _) if wm.id() == xwm => wm,
            (_, Some(old)) => old,
            (Some(wm), None) => wm,
            // Smithay only calls this for a manager that existed, and one is retired, never
            // dropped, before the next starts.
            (None, None) => unreachable!("xwm_state without any X11 window manager"),
        }
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!("x11: new window xid={}", window.window_id());
    }
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_map_request(window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_map_override_redirect(window);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_gone(&window);
        if !window.is_override_redirect()
            && let Err(err) = window.set_mapped(false)
        {
            tracing::debug!("x11: cannot unmap the window: {err}");
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_gone(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        self.x11_configure_request(&window, x, y, w, h);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        self.x11_configure_notify(&window, geometry);
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        if property == WmWindowProperty::NormalHints {
            self.x11_hints_changed(&window);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_mode_request(&window, FsMode::Fullscreen, true);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_mode_request(&window, FsMode::Fullscreen, false);
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_mode_request(&window, FsMode::Maximized, true);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_mode_request(&window, FsMode::Maximized, false);
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32, edge: ResizeEdge) {
        self.x11_drag(&window, DragKind::Resize, Some(edge));
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        self.x11_drag(&window, DragKind::Move, None);
    }

    fn active_window_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _timestamp: u32,
        _currently_active_window: Option<X11Surface>,
    ) {
        self.x11_activate(&window);
    }

    /// Only the focused X11 client may touch the clipboard: a background X client cannot
    /// read or replace what the user copied.
    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        matches!(
            self.keyboard.current_focus(),
            Some(FocusTarget::X11(x11)) if x11.xwm_id() == Some(xwm)
        )
    }

    /// An X client reads a Wayland selection.
    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        let result = match selection {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime_type, fd)
                    .map_err(|err| format!("{err:?}"))
            }
            SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime_type, fd)
                .map_err(|err| format!("{err:?}")),
        };
        if let Err(err) = result {
            tracing::warn!("xwayland: cannot read the wayland {selection:?} selection: {err}");
        }
    }

    /// An X client took a selection: offer it to Wayland clients.
    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ())
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ())
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat)
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat)
                }
            }
        }
    }

    fn disconnected(&mut self, xwm: XwmId) {
        // A manager that was already replaced reports its old channel closing; only the
        // current one means the server is gone.
        if self.xwayland.wm.as_ref().map(|w| w.id()) == Some(xwm) {
            self.queue_xwayland_restart();
        }
    }
}
