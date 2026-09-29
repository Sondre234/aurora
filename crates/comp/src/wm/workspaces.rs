//! Workspace switching and moving windows between workspaces. Every mutation ends in
//! `normalize`, which restores the invariants: each window is in exactly one workspace,
//! each output shows a distinct valid workspace, and hidden workspaces have nothing mapped.
use std::collections::HashSet;

use aurora_layout::{InsertHint, Rect, WinId};
use smithay::{backend::input::InputTime, utils::SERIAL_COUNTER};

use super::{Phase, layout_params};
use crate::{Aurora, action::WsTarget};

impl Aurora {
    pub fn switch_workspace(&mut self, target: WsTarget) {
        let count = self.config.general.workspaces;
        match target {
            WsTarget::Num(n) => self.show_workspace(n),
            WsTarget::Next | WsTarget::Prev => {
                let Some(cur) = self
                    .wm
                    .active_output
                    .as_ref()
                    .and_then(|o| self.wm.active_ws.get(o))
                    .copied()
                else {
                    return;
                };
                // Skip workspaces another output shows: cycling should not jump outputs.
                let next = (1..count)
                    .map(|step| match target {
                        WsTarget::Prev => (cur - 1 + count - step) % count + 1,
                        _ => (cur - 1 + step) % count + 1,
                    })
                    .find(|n| self.wm.output_for_ws(*n).is_none());
                if let Some(n) = next {
                    self.show_workspace(n);
                }
            }
        }
    }

    /// `workspace N`: visible elsewhere means focus that output; hidden replaces what the
    /// focused output (or the output the workspace is pinned to) shows. Selecting the
    /// workspace already shown goes back to the previous one.
    fn show_workspace(&mut self, ws: u32) {
        if ws == 0 || ws > self.config.general.workspaces {
            return;
        }
        let Some(focused) = self.wm.active_output.clone() else {
            return;
        };
        match self.wm.output_for_ws(ws) {
            Some(shown_on) if shown_on == focused => {
                if let Some(prev) = self.wm.previous.get(&focused).copied().filter(|p| *p != ws) {
                    self.show_workspace(prev);
                }
            }
            Some(shown_on) => self.focus_output_ws(&shown_on),
            None => {
                let pinned = self
                    .config
                    .workspace_rules
                    .iter()
                    .find(|r| r.id == ws)
                    .and_then(|r| r.output.as_deref())
                    .and_then(|name| self.wm.outputs.iter().find(|o| o.name() == name))
                    .cloned();
                let output = pinned.unwrap_or(focused);
                if let Some(old) = self.wm.active_ws.insert(output.clone(), ws) {
                    self.wm.previous.insert(output.clone(), old);
                    self.wm.ws_output.remove(&old);
                    self.wm.last_layout.remove(&old);
                }
                self.wm.ws_output.insert(ws, output.clone());
                self.normalize();
                self.relayout_ws(ws);
                self.focus_output_ws(&output);
            }
        }
    }

    /// Makes `output` the active one and focuses the most recently used window of the
    /// workspace it shows.
    pub(crate) fn focus_output_ws(&mut self, output: &smithay::output::Output) {
        self.wm.active_output = Some(output.clone());
        let Some(ws) = self.wm.active_ws.get(output).copied() else {
            return;
        };
        let mapped = |id: &WinId| {
            self.wm
                .windows
                .get(id)
                .is_some_and(|w| w.phase == Phase::Mapped)
                && self.wm.is_visible(*id)
        };
        let target = self
            .wm
            .workspaces
            .get(&ws)
            .and_then(|w| w.mru().iter().copied().find(mapped))
            .or_else(|| {
                self.wm
                    .windows
                    .values()
                    .filter(|w| w.ws == ws && w.phase == Phase::Mapped && self.wm.is_visible(w.id))
                    .map(|w| w.id)
                    .min()
            });
        self.focus_window(target, true);
        self.refresh_pointer_focus();
    }

    /// The windows under the pointer changed without the pointer moving.
    fn refresh_pointer_focus(&mut self) {
        let pointer = self.pointer.clone();
        let pos = pointer.current_location();
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        pointer.frame(self);
        // Focus keeps following the keyboard until the pointer actually crosses a window.
        self.wm.hover = self.space.element_under(pos).map(|(w, _)| w.id());
    }

    /// `move-to-workspace N` for the focused window and its transients.
    pub fn move_to_workspace(&mut self, dst: u32) {
        if dst == 0 || dst > self.config.general.workspaces {
            return;
        }
        let Some(id) = self.wm.focused else { return };
        let Some(src) = self.wm.windows.get(&id).map(|w| w.ws) else {
            return;
        };
        if src == dst {
            return;
        }
        self.relocate_with_children(id, dst);
        // Chosen after the move so a transient child that travelled along is not a candidate.
        let next = self
            .wm
            .workspaces
            .get(&src)
            .and_then(|w| w.focus_after_close(id));
        self.relayout_ws(src);
        self.relayout_ws(dst);
        if self.config.general.move_follows {
            self.show_workspace(dst);
            self.focus_window(Some(id), true);
        } else {
            self.focus_window(next, true);
        }
        self.normalize();
    }

    pub(super) fn relocate_with_children(&mut self, id: WinId, dst: u32) {
        let Some(src) = self.wm.windows.get(&id).map(|w| w.ws) else {
            return;
        };
        let mut moving = vec![id];
        moving.extend(
            self.wm
                .windows
                .values()
                .filter(|w| w.parent == Some(id) && w.ws == src && w.placed)
                .map(|w| w.id),
        );
        moving.sort_by_key(|m| m != &id);
        for m in moving {
            self.relocate(m, dst);
        }
    }

    /// Layout-only move of one window; callers relayout and fix focus.
    pub(super) fn relocate(&mut self, id: WinId, dst: u32) {
        let Some(win) = self.wm.windows.get(&id) else {
            return;
        };
        let (src, constraints, parent) = (win.ws, win.constraints, win.parent);
        if src == dst {
            return;
        }
        let floating = self
            .wm
            .workspaces
            .get(&src)
            .and_then(|w| w.floating_rect(id));
        let rect = floating.map(|r| self.carry_floating(src, dst, r));
        if let Some(workspace) = self.wm.workspaces.get_mut(&src) {
            workspace.remove(id);
        }
        let side = layout_params(&self.config).new_window_side;
        let workspace = self.wm.workspaces.entry(dst).or_default();
        match rect {
            Some(rect) => workspace.add_floating(id, rect),
            None => {
                let hint = InsertHint {
                    after: workspace.mru().first().copied(),
                    side,
                    pointer: None,
                };
                workspace.add_tiled(id, hint, constraints);
            }
        }
        workspace.set_parent(id, parent.filter(|p| workspace.contains(*p)));
        if let Some(win) = self.wm.windows.get_mut(&id) {
            win.ws = dst;
            win.rescued_from = None;
        }
    }

    /// The frame a workspace's floating rectangles are expressed in: the output rectangle it
    /// was last laid out on. A workspace never laid out yet takes the shown (or active) output.
    pub(super) fn ws_frame(&mut self, ws: u32) -> Option<Rect> {
        if let Some(frame) = self.wm.last_full.get(&ws) {
            return Some(*frame);
        }
        let out = self
            .wm
            .output_for_ws(ws)
            .or_else(|| self.wm.active_output.clone())?;
        let full = self.work_area(&out)?.1;
        self.wm.last_full.insert(ws, full);
        Some(full)
    }

    /// Expresses a floating rectangle of `src` in the frame of `dst`. The frame of `dst` stays
    /// as it was, so the rebase on its next layout keeps translating from the right origin.
    fn carry_floating(&mut self, src: u32, dst: u32, r: Rect) -> Rect {
        match (self.ws_frame(src), self.ws_frame(dst)) {
            (Some(a), Some(b)) => carry_rect(r, a, b),
            _ => r,
        }
    }

    /// Restores the workspace invariants. Cheap and idempotent; runs after every mutation.
    pub fn normalize(&mut self) {
        let count = self.config.general.workspaces;

        // Every output shows a distinct workspace within 1..=count.
        let mut seen = HashSet::new();
        let mut assigned = Vec::new();
        for output in &self.wm.outputs {
            let keep = self
                .wm
                .active_ws
                .get(output)
                .copied()
                .filter(|w| (1..=count).contains(w) && !seen.contains(w));
            let ws = keep.or_else(|| (1..=count).find(|w| !seen.contains(w)));
            if let Some(ws) = ws {
                seen.insert(ws);
                assigned.push((output.clone(), ws));
            }
        }
        if assigned
            .iter()
            .any(|(o, w)| self.wm.active_ws.get(o) != Some(w))
            || assigned.len() != self.wm.active_ws.len()
        {
            self.wm.active_ws = assigned.iter().cloned().collect();
            self.wm.ws_output = assigned.iter().map(|(o, w)| (*w, o.clone())).collect();
        }

        // Windows of removed workspaces go to the last kept one; a window missing from its
        // workspace's layout is put back.
        let stray: Vec<(WinId, u32)> = self
            .wm
            .windows
            .values()
            .filter(|w| w.placed)
            .filter_map(|w| {
                if w.ws > count {
                    Some((w.id, count))
                } else if !self
                    .wm
                    .workspaces
                    .get(&w.ws)
                    .is_some_and(|l| l.contains(w.id))
                {
                    Some((w.id, w.ws))
                } else {
                    None
                }
            })
            .collect();
        for (id, ws) in stray {
            if let Some(win) = self.wm.windows.get_mut(&id)
                && win.ws == ws
            {
                // Same workspace: not in its layout, so relocate would be a no-op.
                win.ws = 0;
            }
            self.relocate(id, ws);
        }

        self.hide_invisible();
        self.sync_covering();

        let visible = self
            .wm
            .outputs
            .iter()
            .filter_map(|o| Some(format!("{}={}", o.name(), self.wm.active_ws.get(o)?)))
            .collect::<Vec<_>>()
            .join(" ");
        let line = format!("ws: visible {visible}");
        if self.wm.last_visible != line {
            tracing::info!("{line}");
            self.wm.last_visible = line;
        }
    }

    /// Windows on workspaces no output shows are not in the Space, so they draw nothing and
    /// get no frame callbacks.
    fn hide_invisible(&mut self) {
        let hidden: Vec<_> = self
            .wm
            .windows
            .values()
            .filter(|w| !self.wm.ws_output.contains_key(&w.ws))
            .map(|w| w.element.clone())
            .collect();
        for element in hidden {
            self.space.unmap_elem(&element);
        }
    }

    /// Called after the config changed: drops running grabs (they hold stale geometry)
    /// and lays everything out again.
    pub fn apply_config(&mut self) {
        if self.pointer.is_grabbed() {
            let pointer = self.pointer.clone();
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), InputTime::now());
        }
        self.relayout_all();
    }
}

/// Moves `r` from the frame `from` to the frame `to`, keeping its offset, then pulls it fully
/// inside `to`. Within one frame the rectangle is left alone.
pub(super) fn carry_rect(r: Rect, from: Rect, to: Rect) -> Rect {
    if from == to {
        return r;
    }
    clamp_into(
        Rect::new(r.x + to.x - from.x, r.y + to.y - from.y, r.w, r.h),
        to,
    )
}

/// Shrinks `r` to fit `area` and moves it inside.
pub(super) fn clamp_into(r: Rect, area: Rect) -> Rect {
    let w = r.w.min(area.w).max(1);
    let h = r.h.min(area.h).max(1);
    Rect::new(
        r.x.clamp(area.x, (area.right() - w).max(area.x)),
        r.y.clamp(area.y, (area.bottom() - h).max(area.y)),
        w,
        h,
    )
}
