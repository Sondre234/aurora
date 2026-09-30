//! The one place actions run, whether they come from a bind, a repeat timer, a mouse bind
//! or the QA hooks.
use aurora_layout::{self as layout, Axis, Edges, FsMode, Kind, Side, WinId};
use smithay::desktop::WindowSurface;

use crate::{
    action::{Action, Dir},
    state::Aurora,
};

fn layout_dir(dir: Dir) -> layout::Dir {
    match dir {
        Dir::Left => layout::Dir::Left,
        Dir::Right => layout::Dir::Right,
        Dir::Up => layout::Dir::Up,
        Dir::Down => layout::Dir::Down,
    }
}

impl Aurora {
    pub fn dispatch(&mut self, action: Action) {
        tracing::info!("action: {action}");
        match action {
            Action::Spawn(cmd) => self.spawn(&cmd),
            Action::Close => self.close_focused(),
            Action::Focus(dir) => self.focus_dir(dir),
            Action::Move(dir) => self.move_dir(dir, false),
            Action::Swap(dir) => self.move_dir(dir, true),
            Action::ToggleSplit => self.edit_focused_tiled(|ws, id| ws.tiling.toggle_split(id)),
            Action::ResizeSplit(dir, px) => self.resize_split(dir, px),
            Action::ReloadConfig => self.reload_config(),
            Action::RevokeInhibit => self.revoke_shortcuts_inhibit(),
            Action::Quit => {
                tracing::warn!("quitting: quit action");
                crate::safety::arm_exit_deadline();
                self.loop_signal.stop();
            }
            Action::DebugDump => self.dump_state(),
            Action::DebugPointer(p) => {
                if self.qa {
                    self.debug_pointer(p);
                } else {
                    tracing::warn!("debug-pointer ignored: not running with --qa");
                }
            }
            Action::Workspace(target) => self.switch_workspace(target),
            Action::MoveToWorkspace(n) => self.move_to_workspace(n),
            Action::ToggleFloating => self.toggle_floating(),
            Action::Fullscreen => self.toggle_mode(FsMode::Fullscreen),
            Action::Maximize => self.toggle_mode(FsMode::Maximized),
            Action::FocusOutput(dir) => self.focus_output_dir(dir),
            Action::MoveToOutput(dir) => self.move_to_output_dir(dir),
            Action::DebugAddOutput {
                name,
                size,
                refresh_mhz,
                pos,
            } => {
                if self.qa {
                    self.debug_add_output(name, size, refresh_mhz, pos);
                } else {
                    tracing::warn!("debug-add-output ignored: not running with --qa");
                }
            }
            Action::DebugRemoveOutput(name) => {
                if self.qa {
                    self.debug_remove_output(&name);
                } else {
                    tracing::warn!("debug-remove-output ignored: not running with --qa");
                }
            }
            // Drags start from the button press itself (see `on_pointer_button`), so as a
            // key bind they do nothing.
            Action::DragMove | Action::DragResize | Action::None => {}
        }
    }

    /// Asks the focused window to close; the client decides.
    fn close_focused(&mut self) {
        let Some(win) = self.wm.focused.and_then(|id| self.wm.windows.get(&id)) else {
            return;
        };
        match win.element.underlying_surface() {
            WindowSurface::Wayland(toplevel) => toplevel.send_close(),
            WindowSurface::X11(x11) => {
                if let Err(err) = x11.close() {
                    tracing::warn!(%err, "cannot close the X11 window");
                }
            }
        }
    }

    pub(crate) fn focused_with_ws(&self) -> Option<(WinId, u32)> {
        let id = self.wm.focused?;
        Some((id, self.wm.windows.get(&id)?.ws))
    }

    fn focus_dir(&mut self, dir: Dir) {
        let Some((id, ws)) = self.focused_with_ws() else {
            return;
        };
        let Some((_, mut placed)) = self.ws_placements(ws) else {
            return;
        };
        // A pending window has nothing to focus yet.
        placed.retain(|p| {
            self.wm
                .windows
                .get(&p.id)
                .is_some_and(|w| w.phase == crate::wm::Phase::Mapped)
        });
        let target = self
            .wm
            .workspaces
            .get(&ws)
            .and_then(|w| w.neighbor(&placed, id, layout_dir(dir)));
        if let Some(target) = target {
            self.focus_window(Some(target), true);
        } else if let Some(rect) = placed.iter().find(|p| p.id == id).map(|p| p.outer) {
            self.focus_across_output(ws, rect, dir);
        }
    }

    /// Focus by direction past the edge of an output: the window on the neighbouring output
    /// nearest to the focused one along the edge it crosses.
    fn focus_across_output(&mut self, ws: u32, from: layout::Rect, dir: Dir) {
        let Some(output) = self.wm.output_for_ws(ws) else {
            return;
        };
        let Some(target) = self.output_in_direction(&output, dir) else {
            return;
        };
        let Some(dst) = self.wm.active_ws.get(&target).copied() else {
            return;
        };
        let Some((_, mut placed)) = self.ws_placements(dst) else {
            return;
        };
        placed.retain(|p| {
            self.wm
                .windows
                .get(&p.id)
                .is_some_and(|w| w.phase == crate::wm::Phase::Mapped)
                && self.wm.is_visible(p.id)
        });
        let (cx, cy) = (from.x + from.w / 2, from.y + from.h / 2);
        // Closest along the crossed edge first, then closest to the seam.
        let score = |r: &layout::Rect| match dir {
            Dir::Left => ((r.y + r.h / 2 - cy).abs(), (cx - (r.x + r.w)).abs()),
            Dir::Right => ((r.y + r.h / 2 - cy).abs(), (r.x - cx).abs()),
            Dir::Up => ((r.x + r.w / 2 - cx).abs(), (cy - (r.y + r.h)).abs()),
            Dir::Down => ((r.x + r.w / 2 - cx).abs(), (r.y - cy).abs()),
        };
        match placed.iter().min_by_key(|p| score(&p.outer)).map(|p| p.id) {
            Some(next) => {
                self.wm.active_output = Some(target);
                self.focus_window(Some(next), true);
            }
            None => self.focus_output_ws(&target),
        }
    }

    /// Moves (or swaps) the focused tiled window with its neighbour in `dir`.
    fn move_dir(&mut self, dir: Dir, swap: bool) {
        let Some((id, ws)) = self.focused_with_ws() else {
            return;
        };
        let Some((_, mut placed)) = self.ws_placements(ws) else {
            return;
        };
        placed.retain(|p| p.kind == Kind::Tiled || p.id == id);
        let Some(workspace) = self.wm.workspaces.get_mut(&ws) else {
            return;
        };
        if workspace.is_floating(id) {
            return;
        }
        let Some(target) = workspace.neighbor(&placed, id, layout_dir(dir)) else {
            self.move_to_output_dir(dir);
            return;
        };
        let (side, axis) = match dir {
            Dir::Left => (Side::First, Axis::Horizontal),
            Dir::Up => (Side::First, Axis::Vertical),
            Dir::Right => (Side::Second, Axis::Horizontal),
            Dir::Down => (Side::Second, Axis::Vertical),
        };
        let changed = if swap {
            workspace.tiling.swap(id, target)
        } else {
            workspace.tiling.move_beside(id, target, side, Some(axis))
        };
        if changed {
            self.relayout_ws(ws);
        }
    }

    /// Moves the split on the window's right/bottom side (the layout falls back to the left/top
    /// one at the screen edge) in the direction pressed.
    fn resize_split(&mut self, dir: Dir, px: i32) {
        let (edges, dx, dy) = match dir {
            Dir::Left => (Edges::RIGHT, -px, 0),
            Dir::Right => (Edges::RIGHT, px, 0),
            Dir::Up => (Edges::BOTTOM, 0, -px),
            Dir::Down => (Edges::BOTTOM, 0, px),
        };
        self.edit_focused_tiled(|ws, id| {
            ws.tiling
                .resize_start(id, edges)
                .is_some_and(|h| ws.tiling.resize_set(&h, dx, dy))
        });
    }

    /// Runs a layout edit on the focused window if it is tiled, and relayouts when it says
    /// it changed something.
    fn edit_focused_tiled(&mut self, edit: impl FnOnce(&mut layout::Workspace, WinId) -> bool) {
        let Some((id, ws)) = self.focused_with_ws() else {
            return;
        };
        let Some(workspace) = self.wm.workspaces.get_mut(&ws) else {
            return;
        };
        if !workspace.is_floating(id) && edit(workspace, id) {
            self.relayout_ws(ws);
        }
    }
}
