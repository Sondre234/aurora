//! The one place actions run, whether they come from a bind, a repeat timer, a mouse bind
//! or the QA hooks.
use aurora_layout::{self as layout, Edges, Kind, Side, WinId};
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
            // Outputs, floating, fullscreen and mouse drags come with later steps.
            Action::FocusOutput(_)
            | Action::MoveToOutput(_)
            | Action::ToggleFloating
            | Action::Fullscreen
            | Action::Maximize
            | Action::DragMove
            | Action::DragResize
            | Action::None => {}
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

    fn focused_with_ws(&self) -> Option<(WinId, u32)> {
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
            return;
        };
        let side = match dir {
            Dir::Left | Dir::Up => Side::First,
            Dir::Right | Dir::Down => Side::Second,
        };
        let changed = if swap {
            workspace.tiling.swap(id, target)
        } else {
            workspace.tiling.move_beside(id, target, side)
        };
        if changed {
            self.relayout_ws(ws);
        }
    }

    fn resize_split(&mut self, dir: Dir, px: i32) {
        let (edges, dx, dy) = match dir {
            Dir::Left => (Edges::LEFT, -px, 0),
            Dir::Right => (Edges::RIGHT, px, 0),
            Dir::Up => (Edges::TOP, 0, -px),
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
