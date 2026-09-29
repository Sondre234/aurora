use smithay::{
    desktop::{PopupKind, PopupManager, find_popup_root_surface, get_popup_toplevel_coords},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::{wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
    },
    utils::Serial,
    wayland::shell::xdg::{
        PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    },
};

use crate::{
    Aurora,
    wm::grabs::{DragKind, xdg_edges},
};
use aurora_layout::FsMode;

impl XdgShellHandler for Aurora {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.new_wm_window(surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.wm_window_destroyed(surface.wl_surface());
        self.queue_redraw_all();
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        self.queue_redraw_all();
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, serial: Serial) {
        self.xdg_drag(surface.wl_surface(), DragKind::Move, None, serial);
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        self.xdg_drag(
            surface.wl_surface(),
            DragKind::Resize,
            xdg_edges(edges),
            serial,
        );
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        self.request_mode(surface.wl_surface(), FsMode::Fullscreen, true);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.request_mode(surface.wl_surface(), FsMode::Fullscreen, false);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.request_mode(surface.wl_surface(), FsMode::Maximized, true);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.request_mode(surface.wl_surface(), FsMode::Maximized, false);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // TODO popup grabs
    }
}

/// Should be called on `WlSurface::commit`
pub fn handle_commit(popups: &mut PopupManager, surface: &WlSurface) {
    popups.commit(surface);
    if let Some(popup) = popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    // NOTE: This should never fail as the initial configure is always
                    // allowed.
                    if let Err(err) = xdg.send_configure() {
                        tracing::warn!(%err, "initial popup configure failed");
                    }
                }
            }
            PopupKind::InputMethod(ref _input_method) => {}
        }
    }
}

impl Aurora {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self.wm.window_of(&root).map(|w| &w.element) else {
            self.unconstrain_layer_popup(popup, &root);
            return;
        };
        let Some(window_geo) = self.space.element_geometry(window) else {
            return;
        };

        // Output with the largest overlap with the window; first output if none overlaps.
        let overlap = |g: smithay::utils::Rectangle<i32, smithay::utils::Logical>| {
            g.intersection(window_geo)
                .map_or(0, |r| i64::from(r.size.w) * i64::from(r.size.h))
        };
        let Some(output_geo) = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .max_by_key(|g| overlap(*g))
        else {
            return;
        };

        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
