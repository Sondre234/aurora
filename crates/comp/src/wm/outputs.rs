//! Output lifecycle on the window manager side: where outputs sit, rescuing windows when one
//! goes away, handing them back when it returns, and moving focus between outputs. Backends
//! only create and destroy `Output`s and call `add_output` / `wm_output_removed`.
use std::time::Duration;

use aurora_layout::{Dir, Rect, Size};
use smithay::{
    backend::input::InputTime,
    desktop::layer_map_for_output,
    input::pointer::MotionEvent,
    output::{Output, Scale},
    utils::{Logical, Point, SERIAL_COUNTER, Transform},
    wayland::{
        compositor::{SurfaceData, send_surface_state},
        fractional_scale::with_fractional_scale,
    },
};
use smithay::{
    desktop::utils::surface_primary_scanout_output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
};

use super::Rescue;
use crate::{
    Aurora,
    action::Dir as ActionDir,
    config::OutputRule,
    outputs::{output_in_dir, resolve_with},
};

fn layout_dir(dir: ActionDir) -> Dir {
    match dir {
        ActionDir::Left => Dir::Left,
        ActionDir::Right => Dir::Right,
        ActionDir::Up => Dir::Up,
        ActionDir::Down => Dir::Down,
    }
}

/// The output's size in logical pixels, rounded up as the Space does.
pub fn logical_size(output: &Output) -> Size {
    let Some(mode) = output.current_mode() else {
        return Size::default();
    };
    let size = output
        .current_transform()
        .transform_size(mode.size)
        .to_f64()
        .to_logical(output.current_scale().fractional_scale())
        .to_i32_ceil::<i32>();
    Size {
        w: size.w,
        h: size.h,
    }
}

/// The scale a config rule asks for; anything unusable means 1.
pub fn rule_scale(rule: Option<&OutputRule>) -> Scale {
    match rule.and_then(|r| r.scale) {
        Some(s) if s.is_finite() && (0.25..=8.0).contains(&s) => Scale::Fractional(s),
        _ => Scale::Integer(1),
    }
}

impl Aurora {
    pub fn output_rule(&self, name: &str) -> Option<&OutputRule> {
        self.config.outputs.iter().find(|r| r.name == name)
    }

    /// The `primary = true` output if one is connected, else the first.
    pub fn primary_output(&self) -> Option<Output> {
        self.wm
            .outputs
            .iter()
            .find(|o| self.output_rule(&o.name()).is_some_and(|r| r.primary))
            .or(self.wm.outputs.first())
            .cloned()
    }

    /// Registers a new output whose mode and scale are already set: positions it among the
    /// others, gives it a workspace and takes back the windows it lost earlier.
    pub fn add_output(&mut self, output: &Output) {
        self.space.map_output(output, (0, 0));
        self.xwayland.unmanaged.map_output(output, (0, 0));
        self.wm.output_added(output, &self.config);
        let primary = self.output_rule(&output.name()).is_some_and(|r| r.primary);
        if primary && self.wm.focused.is_none() {
            self.wm.active_output = Some(output.clone());
        }
        let returned = self.return_rescued(output);
        self.arrange_outputs();
        let geo = self.space.output_geometry(output).unwrap_or_default();
        tracing::info!(
            "output: added name={} geo={},{} {}x{} scale={} ws={} returned={returned}",
            output.name(),
            geo.loc.x,
            geo.loc.y,
            geo.size.w,
            geo.size.h,
            output.current_scale().fractional_scale(),
            self.wm.active_ws.get(output).copied().unwrap_or(0),
        );
    }

    /// The one place an output is placed: the Space, the wl_output state and its layer map
    /// change together. Returns whether anything moved; the caller runs the geometry hook.
    pub fn set_output_position(&mut self, output: &Output, pos: aurora_layout::Point) -> bool {
        let at = (pos.x, pos.y);
        let moved = output.current_location() != at.into()
            || self.space.output_geometry(output).map(|g| g.loc) != Some(at.into());
        self.space.map_output(output, at);
        self.xwayland.unmanaged.map_output(output, at);
        output.change_current_state(None, None, None, Some(at.into()));
        layer_map_for_output(output).arrange();
        moved
    }

    /// Recomputes every output's position (config first, the rest packed left to right) and
    /// runs the geometry hook.
    pub fn arrange_outputs(&mut self) {
        let outputs = self.wm.outputs.clone();
        let sizes: Vec<_> = outputs
            .iter()
            .map(|o| (o.name(), logical_size(o)))
            .collect();
        let positions = resolve_with(&sizes, |name| {
            self.wm
                .debug_positions
                .get(name)
                .copied()
                .or_else(|| self.output_rule(name).and_then(|r| r.position))
        });
        for (output, pos) in outputs.iter().zip(positions) {
            self.set_output_position(output, pos);
        }
        self.wm_output_geometry_changed();
    }

    /// Something about the arrangement changed (position, size, scale, an output came or
    /// went): lay everything out again, keep the pointer on an output and tell clients.
    pub fn wm_output_geometry_changed(&mut self) {
        self.space.refresh();
        // Layout rebases floating windows whose output rectangle moved.
        self.relayout_all();
        let old = self.pointer.current_location();
        let new = self.clamp_pointer(old);
        if new != old {
            self.warp_pointer(new);
        }
        let outputs = self.wm.outputs.clone();
        for output in &outputs {
            self.send_output_scale(output);
            self.captures.output_changed(output);
        }
        self.queue_redraw_all();
    }

    /// Applies the config's scale and positions to the running outputs.
    pub fn reapply_output_config(&mut self) {
        let outputs = self.wm.outputs.clone();
        for output in &outputs {
            let scale = rule_scale(self.output_rule(&output.name()));
            if output.current_scale().fractional_scale() != scale.fractional_scale() {
                output.change_current_state(None, None, Some(scale), None);
            }
        }
        self.arrange_outputs();
    }

    /// Sends the preferred scale of `output` to everything it presents.
    pub fn send_output_scale(&self, output: &Output) {
        let scale = output.current_scale();
        let (fractional, integer) = (scale.fractional_scale(), scale.integer_scale());
        let update = |surface: &WlSurface, data: &SurfaceData| {
            let primary = surface_primary_scanout_output(surface, data);
            if primary.is_none_or(|p| p == *output) {
                with_fractional_scale(data, |fs| fs.set_preferred_scale(fractional));
                send_surface_state(surface, data, integer, Transform::Normal);
            }
        };
        for window in self.space.elements() {
            if self.space.outputs_for_element(window).contains(output) {
                window.with_surfaces(update);
            }
        }
        for layer in layer_map_for_output(output).layers() {
            layer.with_surfaces(update);
        }
    }

    /// Frame callbacks for the backends that have no vblank of their own (nested, headless).
    pub fn send_nested_frames(&mut self, output: &Output) {
        let time = Duration::from(self.clock.now());
        self.send_output_scale(output);
        let mut sent = Vec::new();
        for window in self.space.elements() {
            if self.space.outputs_for_element(window).contains(output) {
                window.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
                sent.push(window.id());
            }
        }
        for id in sent {
            if let Some(win) = self.wm.windows.get_mut(&id) {
                win.frames_sent += 1;
            }
        }
        for window in self.xwayland.unmanaged.elements() {
            if self
                .xwayland
                .unmanaged
                .outputs_for_element(window)
                .contains(output)
            {
                window.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
            }
        }
        for layer in layer_map_for_output(output).layers() {
            layer.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        self.space.refresh();
        self.xwayland.unmanaged.refresh();
        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
    }

    /// Pulls windows back to an output that returned, unless they were moved since.
    fn return_rescued(&mut self, output: &Output) -> usize {
        let Some(ws) = self.wm.active_ws.get(output).copied() else {
            return 0;
        };
        let name = output.name();
        let back: Vec<_> = self
            .wm
            .windows
            .values()
            .filter(|w| w.rescued_from.as_ref().is_some_and(|r| r.output == name))
            .map(|w| (w.id, w.ws, w.rescued_from.as_ref().map_or(0, |r| r.to_ws)))
            .collect();
        let mut returned = 0;
        for (id, at, to_ws) in back {
            if at == to_ws && at != ws {
                self.relocate(id, ws);
                returned += 1;
            }
            if let Some(win) = self.wm.windows.get_mut(&id) {
                win.rescued_from = None;
            }
        }
        returned
    }

    /// An output is going away and is still alive here. Its windows move to the workspace the
    /// first remaining output shows, then the bookkeeping forgets it. With no output left the
    /// windows keep their workspace and return with the next output. Returns the window count.
    pub fn wm_output_removed(&mut self, output: &Output) -> usize {
        self.captures.output_removed(output);
        let name = output.name();
        let shown = self.wm.active_ws.get(output).copied();
        let target = self
            .wm
            .outputs
            .iter()
            .filter(|o| *o != output)
            .find_map(|o| self.wm.active_ws.get(o).copied());
        let mut rescued = 0;
        if let (Some(from), Some(to)) = (shown, target) {
            let mut order = Vec::new();
            if let Some(workspace) = self.wm.workspaces.get(&from) {
                workspace.tiling.windows(&mut order);
                order.extend(workspace.floating().iter().map(|(id, _)| *id));
            }
            // Parents first, so transients find theirs in the new workspace.
            order.sort_by_key(|id| self.wm.windows.get(id).is_some_and(|w| w.parent.is_some()));
            for id in order {
                self.relocate(id, to);
                if let Some(win) = self.wm.windows.get_mut(&id) {
                    win.rescued_from = Some(Rescue {
                        output: name.clone(),
                        to_ws: to,
                    });
                    rescued += 1;
                }
            }
            self.relayout_ws(to);
        }

        // Layers cannot follow: their clients get closed, and re-create them on another output.
        let layers: Vec<_> = layer_map_for_output(output).layers().cloned().collect();
        for layer in layers {
            layer.layer_surface().send_close();
            layer_map_for_output(output).unmap_layer(&layer);
        }
        self.wm.output_removed(output);
        self.space.unmap_output(output);
        self.xwayland.unmanaged.unmap_output(output);
        // The focused window may have moved with the rescue.
        if let Some(ws) = self
            .wm
            .focused
            .and_then(|f| self.wm.windows.get(&f))
            .map(|w| w.ws)
            && let Some(o) = self.wm.output_for_ws(ws)
        {
            self.wm.active_output = Some(o);
        }
        self.arrange_outputs();
        self.refresh_layer_focus();
        tracing::info!("output: removed name={name} rescued={rescued}");
        rescued
    }

    /// Moves the pointer without a device event and updates who is under it.
    pub fn warp_pointer(&mut self, pos: Point<f64, Logical>) {
        let pointer = self.pointer.clone();
        let pos = self.clamp_pointer(pos);
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        pointer.frame(self);
        // Focus keeps following the keyboard until the pointer moves onto another window.
        self.wm.hover = self.space.element_under(pos).map(|(w, _)| w.id());
        self.queue_redraw_all();
    }

    fn output_rects(&self) -> Vec<(Output, Rect)> {
        self.wm
            .outputs
            .iter()
            .filter_map(|o| {
                let g = self.space.output_geometry(o)?;
                Some((o.clone(), Rect::new(g.loc.x, g.loc.y, g.size.w, g.size.h)))
            })
            .collect()
    }

    pub fn output_in_direction(&self, from: &Output, dir: ActionDir) -> Option<Output> {
        let outputs = self.output_rects();
        let index = outputs.iter().position(|(o, _)| o == from)?;
        let rects: Vec<_> = outputs.iter().map(|(_, r)| *r).collect();
        let target = output_in_dir(&rects, index, layout_dir(dir))?;
        outputs.get(target).map(|(o, _)| o.clone())
    }

    /// `focus-output`: the pointer jumps to the middle of the neighbour, which becomes the
    /// active output and focuses its top window.
    pub fn focus_output_dir(&mut self, dir: ActionDir) {
        let Some(from) = self.wm.active_output.clone() else {
            return;
        };
        let Some(target) = self.output_in_direction(&from, dir) else {
            return;
        };
        if let Some(geo) = self.space.output_geometry(&target) {
            let centre = geo.loc.to_f64() + geo.size.to_f64().downscale(2.0).to_point();
            self.warp_pointer(centre);
        }
        self.focus_output_ws(&target);
    }

    /// `move-to-output`: the focused window and its transients go to the workspace the
    /// neighbour shows, and focus goes with them.
    pub fn move_to_output_dir(&mut self, dir: ActionDir) {
        let Some((id, src)) = self.focused_with_ws() else {
            return;
        };
        let Some(from) = self.wm.output_for_ws(src) else {
            return;
        };
        let Some(target) = self.output_in_direction(&from, dir) else {
            return;
        };
        let Some(dst) = self.wm.active_ws.get(&target).copied() else {
            return;
        };
        self.relocate_with_children(id, dst);
        self.relayout_ws(src);
        self.relayout_ws(dst);
        self.focus_window(Some(id), true);
        self.normalize();
    }
}
