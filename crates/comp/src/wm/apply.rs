//! Window lifecycle and the one place that projects layout results onto the `Space`.
//! Relayout runs on events (map, close, config, output changes), never per frame.
use aurora_layout::{
    Constraints, Edges, FsMode, InsertHint, Kind, Placement, Point, Rect, Size, WinId,
};
use smithay::{
    desktop::{layer_map_for_output, space::SpaceElement},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Rectangle},
    wayland::{
        compositor::with_states,
        shell::xdg::{SurfaceCachedState, ToplevelSurface, XdgToplevelSurfaceData},
    },
};

use super::{
    Phase, WinData, layout_params, rules,
    window::{WindowElement, Z_FLOATING, Z_FULLSCREEN, Z_TILED},
};
use crate::Aurora;

pub(super) fn rect_of(r: Rectangle<i32, Logical>) -> Rect {
    Rect::new(r.loc.x, r.loc.y, r.size.w, r.size.h)
}

/// Size hints of a toplevel, 0 meaning unconstrained.
fn read_constraints(surface: &WlSurface) -> Constraints {
    with_states(surface, |states| {
        let mut cached = states.cached_state.get::<SurfaceCachedState>();
        let current = cached.current();
        Constraints {
            min: Size {
                w: current.min_size.w.max(0),
                h: current.min_size.h.max(0),
            },
            max: Size {
                w: current.max_size.w.max(0),
                h: current.max_size.h.max(0),
            },
        }
        .clamped()
    })
}

pub(crate) fn read_strings(surface: &WlSurface) -> (String, String) {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok())
            .map(|d| {
                (
                    d.app_id.clone().unwrap_or_default(),
                    d.title.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default()
    })
}

pub(crate) fn has_buffer(surface: &WlSurface) -> bool {
    smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
        s.buffer().is_some()
    })
    .unwrap_or(false)
}

fn initial_configure_sent(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok())
            .is_some_and(|d| d.initial_configure_sent)
    })
}

/// Whether the client still owes an ack for a configure we sent.
fn has_unacked(toplevel: &ToplevelSurface) -> bool {
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok())
            .is_some_and(|d| !d.pending_configures().is_empty())
    })
}

/// Sets size and state on the pending configure and sends it when something changed.
fn configure(toplevel: &ToplevelSurface, size: (i32, i32), flags: (bool, bool, bool)) {
    let (tiled, fullscreen, maximized) = flags;
    toplevel.with_pending_state(|s| {
        s.size = (size.0 > 0 && size.1 > 0).then(|| size.into());
        for state in [
            xdg_toplevel::State::TiledLeft,
            xdg_toplevel::State::TiledRight,
            xdg_toplevel::State::TiledTop,
            xdg_toplevel::State::TiledBottom,
        ] {
            if tiled {
                s.states.set(state);
            } else {
                s.states.unset(state);
            }
        }
        if fullscreen {
            s.states.set(xdg_toplevel::State::Fullscreen);
        } else {
            s.states.unset(xdg_toplevel::State::Fullscreen);
        }
        if maximized {
            s.states.set(xdg_toplevel::State::Maximized);
        } else {
            s.states.unset(xdg_toplevel::State::Maximized);
        }
    });
    toplevel.send_pending_configure();
}

impl Aurora {
    /// The output's usable area (bars excluded) and its full area, in global coordinates.
    pub(crate) fn work_area(&self, output: &Output) -> Option<(Rect, Rect)> {
        let geo = self.space.output_geometry(output)?;
        let full = rect_of(geo);
        let zone = layer_map_for_output(output).non_exclusive_zone();
        if zone.is_empty() {
            return Some((full, full));
        }
        let work = Rect::new(
            geo.loc.x + zone.loc.x,
            geo.loc.y + zone.loc.y,
            zone.size.w,
            zone.size.h,
        );
        Some((work, full))
    }

    pub fn relayout_all(&mut self) {
        self.normalize();
        let visible: Vec<u32> = self.wm.active_ws.values().copied().collect();
        for ws in visible {
            self.relayout_ws(ws);
        }
    }

    /// The workspace's placements on the output that shows it, or `None` when it is hidden.
    pub fn ws_placements(&mut self, ws: u32) -> Option<(Output, Vec<Placement>)> {
        let output = self.wm.output_for_ws(ws)?;
        let (work, full) = self.work_area(&output)?;
        let params = layout_params(&self.config);
        let mut placed = Vec::new();
        let old_full = self.wm.last_full.insert(ws, full);
        if let Some(workspace) = self.wm.workspaces.get_mut(&ws) {
            if let Some(old) = old_full {
                workspace.rebase(old, full);
            }
            workspace.placements(work, full, &params, &mut placed);
        }
        Some((output, placed))
    }

    pub fn relayout_ws(&mut self, ws: u32) {
        if let Some((output, placed)) = self.ws_placements(ws) {
            self.apply(&output, ws, &placed);
        }
    }

    /// Diffs `placed` onto the Space: map or relocate, unmap what left, configure only
    /// windows whose size or state changed, and refresh the borders.
    fn apply(&mut self, output: &Output, ws: u32, placed: &[Placement]) {
        let general = &self.config.general;
        let colors = [general.border_focused.0, general.border_unfocused.0];
        let border = general.border_width;
        let dragging = self.wm.drag.is_some();

        let mut line = String::new();
        for p in placed {
            let Some(win) = self.wm.windows.get_mut(&p.id) else {
                continue;
            };
            let mut content = p.content;
            let full = matches!(p.kind, Kind::Fullscreen | Kind::Maximized);
            let bw = if full { 0 } else { border };
            // A floating resize from the left or top keeps the far edge where it was, whatever
            // size the client has actually committed so far.
            if let (Some(a), Kind::Floating) = (win.resize_anchor, p.kind) {
                let actual = win.element.geometry().size;
                if actual.w > 0 && actual.h > 0 {
                    if a.edges.contains(Edges::LEFT) {
                        content.x = a.right - actual.w;
                    }
                    if a.edges.contains(Edges::TOP) {
                        content.y = a.bottom - actual.h;
                    }
                    if content != p.content
                        && let Some(workspace) = self.wm.workspaces.get_mut(&ws)
                    {
                        workspace.set_floating_rect(p.id, content.shrink(-bw));
                    }
                }
            }
            win.target = content;
            win.current = content;
            win.floating = p.kind == Kind::Floating;
            win.fs = p.kind == Kind::Fullscreen;
            win.ws = ws;
            if win.floating {
                win.float_rect = Some(content.shrink(-bw));
            }

            let deco = win.element.deco();
            deco.set_z(match p.kind {
                Kind::Fullscreen => Z_FULLSCREEN,
                Kind::Floating | Kind::Maximized => Z_FLOATING,
                Kind::Tiled => Z_TILED,
            });
            deco.set_border(bw, colors);

            let size = (content.w, content.h);
            let flags = (
                p.kind == Kind::Tiled,
                p.kind == Kind::Fullscreen,
                p.kind == Kind::Maximized,
            );
            if (win.sent_size != Some(size) || win.sent_flags != flags)
                && let Some(toplevel) = win.element.toplevel()
                // While dragging, wait for the ack: the commit that follows re-runs this.
                && !(dragging && has_unacked(toplevel))
            {
                win.sent_size = Some(size);
                win.sent_flags = flags;
                configure(toplevel, size, flags);
            } else if let Some(x11) = win.element.x11_surface() {
                // X11 has no acks: position and size go out at once, and the client's
                // position is part of the truth (menus open relative to it).
                let geo =
                    Rectangle::new((content.x, content.y).into(), (content.w, content.h).into());
                if win.sent_flags != flags {
                    win.sent_flags = flags;
                    if let Err(err) = x11.set_fullscreen(flags.1).and(x11.set_maximized(flags.2)) {
                        tracing::debug!("x11: cannot set the window state: {err}");
                    }
                }
                if x11.last_configure() != geo {
                    win.sent_size = Some(size);
                    if let Err(err) = x11.configure(geo) {
                        tracing::debug!("x11: cannot configure the window: {err}");
                    }
                }
            }

            if win.phase == Phase::Mapped {
                let at = (content.x, content.y).into();
                if self.space.element_location(&win.element) != Some(at) {
                    self.space.map_element(win.element.clone(), at, false);
                }
            }

            use std::fmt::Write;
            let _ = write!(
                line,
                " {}:{}:{},{} {}x{}{}{}",
                p.id.0,
                win.app_id,
                p.outer.x,
                p.outer.y,
                p.outer.w,
                p.outer.h,
                if win.floating { " float" } else { "" },
                match p.kind {
                    Kind::Fullscreen => " fs",
                    Kind::Maximized => " max",
                    _ => "",
                },
            );
        }

        let gone: Vec<WindowElement> = self
            .wm
            .windows
            .values()
            .filter(|w| w.ws == ws && w.phase == Phase::Mapped)
            .filter(|w| !placed.iter().any(|p| p.id == w.id))
            .map(|w| w.element.clone())
            .collect();
        for element in gone {
            self.space.unmap_elem(&element);
        }

        let line = format!(
            "layout: ws={ws} out={} [{}]",
            output.name(),
            line.trim_start()
        );
        if !dragging && self.wm.last_layout.get(&ws) != Some(&line) {
            tracing::info!("{line}");
            self.wm.last_layout.insert(ws, line);
            // Windows moved under a still pointer: focus and constraints must follow.
            self.resend_pointer_focus();
        }
        self.queue_redraw_output(output);
        // Fullscreen decides whether Top layers show and who may hold the keyboard.
        self.refresh_layer_focus();
    }

    pub fn new_wm_window(&mut self, toplevel: ToplevelSurface) {
        use smithay::reexports::wayland_server::Resource;
        let id = self.wm.alloc_id();
        let surface = toplevel.wl_surface().clone();
        let element =
            WindowElement::new(id, smithay::desktop::Window::new_wayland_window(toplevel));
        self.wm.by_surface.insert(surface.id(), id);
        self.wm.windows.insert(id, WinData::new(id, element));
    }

    /// Handles a toplevel's commit: the initial one runs the rules and places the window,
    /// the first buffer maps it, later ones track size hints.
    pub fn toplevel_commit(&mut self, surface: &WlSurface) {
        let Some(id) = self.wm.id_of(surface) else {
            return;
        };
        let Some((phase, placed, x11)) = self
            .wm
            .windows
            .get(&id)
            .map(|w| (w.phase, w.placed, w.element.x11_surface().is_some()))
        else {
            return;
        };
        // A client that destroys its role and commits resets the initial-configure flag, so
        // the placed check keeps a dying window from being mapped again.
        match phase {
            Phase::Mapped => {
                // X11 hints arrive as property changes, not through the surface.
                if !x11 {
                    self.refresh_constraints(id, surface);
                }
                // A drag waits for the client's ack and size, which arrive with this commit.
                if self.wm.drag.is_some()
                    && let Some(ws) = self.wm.windows.get(&id).map(|w| w.ws)
                {
                    self.relayout_ws(ws);
                }
            }
            // An X11 window is in the layout from its map request; the first buffer shows it.
            Phase::Pending if x11 => {
                if has_buffer(surface) {
                    self.window_mapped(id);
                }
            }
            Phase::Pending if !placed && !initial_configure_sent(surface) => {
                self.map_request(id, surface)
            }
            Phase::Pending => {
                if has_buffer(surface) {
                    self.window_mapped(id);
                }
            }
        }
    }

    fn map_request(&mut self, id: WinId, surface: &WlSurface) {
        let Some(win) = self.wm.windows.get(&id) else {
            return;
        };
        let Some(toplevel) = win.element.toplevel().cloned() else {
            return;
        };
        let (app_id, title) = read_strings(surface);
        let constraints = read_constraints(surface);
        let parent_surface = toplevel.parent();
        let parent = parent_surface.as_ref().and_then(|p| self.wm.id_of(p));
        if let Some(win) = self.wm.windows.get_mut(&id) {
            win.app_id = app_id;
            win.constraints = constraints;
            win.parent = parent;
        }
        self.place(id, parent_surface.is_some(), &title);
        // Nothing to lay out against (no output yet): the client still needs its configure.
        if !initial_configure_sent(surface) {
            toplevel.send_configure();
        }
    }

    /// Inserts the window into the layout of the focused output's workspace.
    pub(super) fn place(&mut self, id: WinId, has_parent: bool, title: &str) {
        let Some(active) = self.wm.active_output.clone() else {
            return;
        };
        let Some(active_ws) = self.wm.active_ws.get(&active).copied() else {
            return;
        };
        let Some(win) = self.wm.windows.get(&id) else {
            return;
        };
        let (constraints, parent) = (win.constraints, win.parent);
        let decision = rules::evaluate(
            &rules::Attrs {
                has_parent,
                constraints,
                app_id: &win.app_id,
                title,
                x11: win.element.x11_surface().is_some(),
            },
            &self.config.window_rules,
        );
        // A rule may name a workspace that is hidden or shown on another output.
        let ws = decision
            .workspace
            .filter(|w| (1..=self.config.general.workspaces).contains(w))
            .unwrap_or(active_ws);
        let output = self.wm.output_for_ws(ws).unwrap_or(active);
        let Some((work, _)) = self.work_area(&output) else {
            return;
        };
        let parent_rect = parent
            .and_then(|p| self.wm.windows.get(&p))
            .filter(|p| p.ws == ws)
            .map(|p| p.target);
        let params = layout_params(&self.config);
        let pointer = self.pointer.current_location();

        let workspace = self.wm.workspaces.entry(ws).or_default();
        if decision.floating {
            let rect = floating_rect(work, parent_rect, constraints, decision.size, params.border);
            workspace.add_floating(id, rect);
        } else {
            let after = self
                .wm
                .focused
                .filter(|f| workspace.contains(*f))
                .or_else(|| workspace.mru().first().copied());
            let hint = InsertHint {
                after,
                side: params.new_window_side,
                pointer: Some(Point {
                    x: pointer.x as i32,
                    y: pointer.y as i32,
                }),
            };
            workspace.add_tiled(id, hint, constraints);
        }
        workspace.set_parent(id, parent);
        let mode = if decision.fullscreen {
            Some(FsMode::Fullscreen)
        } else {
            self.wm.windows.get(&id).and_then(|w| w.want_mode)
        };
        if mode.is_some() {
            workspace.set_fullscreen(id, mode);
        }
        if let Some(win) = self.wm.windows.get_mut(&id) {
            win.ws = ws;
            win.placed = true;
            win.want_mode = None;
        }
        self.relayout_ws(ws);
    }

    /// First buffer: show and focus the window.
    fn window_mapped(&mut self, id: WinId) {
        let Some(win) = self.wm.windows.get_mut(&id) else {
            return;
        };
        win.phase = Phase::Mapped;
        if win.placed {
            let ws = win.ws;
            self.relayout_ws(ws);
        } else {
            let has_parent = win.parent.is_some();
            let title = win
                .element
                .toplevel()
                .map(|t| read_strings(t.wl_surface()).1);
            self.place(id, has_parent, &title.unwrap_or_default());
        }
        // A window opened on a workspace that is not the focused one stays in the background.
        let ws = self.wm.windows.get(&id).map_or(0, |w| w.ws);
        let shown_here = self
            .wm
            .active_output
            .as_ref()
            .and_then(|o| self.wm.active_ws.get(o))
            == Some(&ws);
        // Behind a fullscreen window only its transient children take focus.
        let blocked = self
            .wm
            .workspaces
            .get(&ws)
            .and_then(|w| w.fullscreen())
            .is_some_and(|(f, _)| {
                f != id
                    && self
                        .wm
                        .windows
                        .get(&id)
                        .is_some_and(|w| w.parent != Some(f))
            });
        if shown_here && !blocked {
            self.focus_window(Some(id), true);
        }
    }

    fn refresh_constraints(&mut self, id: WinId, surface: &WlSurface) {
        self.set_constraints(id, read_constraints(surface));
    }

    pub(super) fn set_constraints(&mut self, id: WinId, constraints: Constraints) {
        let Some(win) = self.wm.windows.get_mut(&id) else {
            return;
        };
        if win.constraints == constraints {
            return;
        }
        win.constraints = constraints;
        let ws = win.ws;
        if let Some(workspace) = self.wm.workspaces.get_mut(&ws) {
            workspace.tiling.set_constraints(id, constraints);
        }
        self.relayout_ws(ws);
    }

    /// The client destroyed its toplevel (or disconnected).
    pub fn wm_window_destroyed(&mut self, surface: &WlSurface) {
        use smithay::reexports::wayland_server::Resource;
        if let Some(id) = self.wm.by_surface.get(&surface.id()).copied() {
            self.wm_remove_window(id);
        }
    }

    /// Forgets a window whatever its kind: xdg toplevel destroyed, X11 window unmapped.
    pub(super) fn wm_remove_window(&mut self, id: WinId) {
        self.wm.by_surface.retain(|_, v| *v != id);
        self.wm.by_x11.retain(|_, v| *v != id);
        let Some(win) = self.wm.windows.remove(&id) else {
            return;
        };
        self.space.unmap_elem(&win.element);
        if self.wm.hover == Some(id) {
            self.wm.hover = None;
        }
        let was_focused = self.wm.focused == Some(id);
        let mut next = None;
        if let Some(workspace) = self.wm.workspaces.get_mut(&win.ws) {
            next = workspace.focus_after_close(id);
            workspace.remove(id);
        }
        self.relayout_ws(win.ws);
        if was_focused {
            self.focus_window(next, true);
        }
    }
}

/// Where a new floating window goes: centred on its parent or on the work area, sized
/// from its hints (or two thirds of the area). `border` grows the result to an outer rect.
pub(super) fn floating_rect(
    work: Rect,
    parent: Option<Rect>,
    c: Constraints,
    size: Option<(i32, i32)>,
    border: i32,
) -> Rect {
    let pick = |min: i32, max: i32, area: i32, want: Option<i32>| {
        let mut v = want.unwrap_or(if min > 0 && min == max {
            min
        } else {
            area * 2 / 3
        });
        v = v.max(min);
        if max > 0 {
            v = v.min(max);
        }
        v.min(area - 2 * border).max(1)
    };
    let w = pick(c.min.w, c.max.w, work.w, size.map(|s| s.0)) + 2 * border;
    let h = pick(c.min.h, c.max.h, work.h, size.map(|s| s.1)) + 2 * border;
    let anchor = parent.unwrap_or(work).center();
    let x = (anchor.x - w / 2).clamp(work.x, (work.right() - w).max(work.x));
    let y = (anchor.y - h / 2).clamp(work.y, (work.bottom() - h).max(work.y));
    Rect::new(x, y, w, h)
}
