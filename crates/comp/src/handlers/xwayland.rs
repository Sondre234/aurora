//! Placeholder XwmHandler: the focus type needs the trait before XWayland is wired up.
//! `X11Wm` only calls it once a window manager was started, and none is yet.
use std::os::fd::OwnedFd;

use smithay::{
    utils::{Logical, Rectangle},
    wayland::selection::SelectionTarget,
    xwayland::{
        X11Surface, X11Wm, XwmHandler,
        xwm::{Reorder, ResizeEdge, XwmId},
    },
};

use crate::Aurora;

impl XwmHandler for Aurora {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        unreachable!("no X11 window manager is started yet")
    }
    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn map_window_request(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn unmapped_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn destroyed_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn configure_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        _w: Option<u32>,
        _h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
    }
    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
    }
    fn resize_request(&mut self, _: XwmId, _: X11Surface, _button: u32, _edge: ResizeEdge) {}
    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}
    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        false
    }
    fn send_selection(&mut self, _: XwmId, _: SelectionTarget, _mime_type: String, _fd: OwnedFd) {}
}
