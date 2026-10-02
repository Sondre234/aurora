//! Window modes: floating, fullscreen and maximized. The layout owns the state; these only
//! flip it and relayout, so a window returns to its tile slot when the mode is cleared.
use aurora_layout::{FsMode, InsertHint, Point, WinId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use super::Phase;
use super::{
    apply::floating_rect,
    layout_params,
    workspaces::{carry_rect, clamp_into},
};
use crate::Aurora;

/// Whether the toplevel declared itself a modal dialog (xdg-dialog-v1).
pub fn is_modal(element: &crate::wm::window::WindowElement) -> bool {
    use smithay::wayland::shell::xdg::{XdgToplevelSurfaceData, dialog::ToplevelDialogHint};
    let Some(toplevel) = element.toplevel() else {
        return false;
    };
    smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| {
                d.lock()
                    .ok()
                    .map(|d| d.dialog_hint == ToplevelDialogHint::Modal)
            })
            .unwrap_or(false)
    })
}

impl Aurora {
    /// Takes a placed, tiled window out of the tiling into a float centred on its parent (or
    /// the work area), as for a modal dialog. Fullscreen and floating windows are left alone.
    pub fn float_modal(&mut self, id: WinId) {
        let Some(win) = self.wm.windows.get(&id) else {
            return;
        };
        if !win.placed {
            return;
        }
        let (ws, constraints, parent) = (win.ws, win.constraints, win.parent);
        let parent_rect = parent
            .and_then(|p| self.wm.windows.get(&p))
            .filter(|p| p.ws == ws)
            .map(|p| p.target);
        let params = layout_params(&self.config);
        let Some(work) = self
            .wm
            .output_for_ws(ws)
            .and_then(|o| self.work_area(&o))
            .map(|(work, _)| work)
        else {
            return;
        };
        let Some(workspace) = self.wm.workspaces.get_mut(&ws) else {
            return;
        };
        if workspace.is_floating(id) || workspace.fullscreen().is_some_and(|(f, _)| f == id) {
            return;
        }
        let rect = floating_rect(work, parent_rect, constraints, None, params.border);
        workspace.add_floating(id, rect);
        self.relayout_ws(ws);
    }

    pub fn toggle_floating(&mut self) {
        let Some((id, ws)) = self.focused_with_ws() else {
            return;
        };
        let Some(win) = self.wm.windows.get(&id) else {
            return;
        };
        let (remembered, constraints) = (win.float_rect, win.constraints);
        let params = layout_params(&self.config);
        let work = self
            .wm
            .output_for_ws(ws)
            .and_then(|o| self.work_area(&o))
            .map(|(work, _)| work);
        let frame = self.ws_frame(ws);
        let pointer = self.pointer.current_location();
        let Some(workspace) = self.wm.workspaces.get_mut(&ws) else {
            return;
        };
        // Fullscreen and maximized windows keep their mode until it is cleared.
        if workspace.fullscreen().is_some_and(|(f, _)| f == id) {
            return;
        }
        if workspace.is_floating(id) {
            let hint = InsertHint {
                after: workspace
                    .mru()
                    .iter()
                    .copied()
                    .find(|&m| m != id && !workspace.is_floating(m)),
                side: params.new_window_side,
                pointer: Some(Point {
                    x: pointer.x as i32,
                    y: pointer.y as i32,
                }),
            };
            workspace.set_tiled(id, hint, constraints);
        } else {
            // The remembered rectangle may come from another output or an older geometry:
            // bring it into the current frame and keep it on the work area.
            let rect = match (remembered, frame, work) {
                (Some((r, from)), Some(to), Some(work)) => {
                    Some(clamp_into(carry_rect(r, from, to), work))
                }
                (_, _, Some(work)) => {
                    Some(floating_rect(work, None, constraints, None, params.border))
                }
                _ => None,
            };
            let Some(rect) = rect else { return };
            workspace.add_floating(id, rect);
        }
        self.relayout_ws(ws);
    }

    /// The fullscreen and maximize actions: set the mode, or clear it when it already holds.
    pub fn toggle_mode(&mut self, mode: FsMode) {
        let Some((id, ws)) = self.focused_with_ws() else {
            return;
        };
        let Some(workspace) = self.wm.workspaces.get_mut(&ws) else {
            return;
        };
        let held = workspace.fullscreen() == Some((id, mode));
        workspace.set_fullscreen(id, (!held).then_some(mode));
        self.relayout_ws(ws);
    }

    /// A client asked for a mode (`set` true) or to leave it. Before the window is placed the
    /// wish is remembered for `place`; a window that does not hold the mode ignores a leave.
    pub fn request_mode(&mut self, surface: &WlSurface, mode: FsMode, set: bool) {
        let Some(id) = self.wm.id_of(surface) else {
            return;
        };
        self.request_mode_id(id, mode, set);
    }

    pub fn request_mode_id(&mut self, id: WinId, mode: FsMode, set: bool) {
        let Some(win) = self.wm.windows.get_mut(&id) else {
            return;
        };
        if !win.placed {
            win.want_mode = set.then_some(mode);
            return;
        }
        let ws = win.ws;
        let toplevel = win.element.toplevel().cloned();
        if let Some(workspace) = self.wm.workspaces.get_mut(&ws) {
            if set {
                workspace.set_fullscreen(id, Some(mode));
            } else if workspace.fullscreen() == Some((id, mode)) {
                workspace.set_fullscreen(id, None);
            }
        }
        self.relayout_ws(ws);
        // Whatever just went out of sight cannot keep the keyboard.
        if set
            && self.wm.focused.is_some_and(|f| {
                !self.wm.is_visible(f) && self.wm.windows.get(&f).is_some_and(|w| w.ws == ws)
            })
            && self
                .wm
                .windows
                .get(&id)
                .is_some_and(|w| w.phase == Phase::Mapped)
        {
            self.focus_window(Some(id), true);
        }
        // A request always gets an answer, even when the window is hidden or unchanged.
        if let Some(toplevel) = toplevel {
            toplevel.send_pending_configure();
        }
    }
}
