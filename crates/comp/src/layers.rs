//! Layer-shell policy: the Smithay layer maps hold geometry and exclusive zones, this holds
//! what they do not: keyboard focus, hit testing order and the fullscreen rule.
//!
//! Every output's layer map sits behind a mutex. Guards here are short-lived and never held
//! across anything that can commit or change focus.
use smithay::{
    desktop::{LayerMap, LayerSurface, Window, WindowSurfaceType, layer_map_for_output},
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
            },
            xdg::PopupSurface,
        },
    },
};

use crate::{focus::FocusTarget, state::Aurora, wm::window::WindowElement};

/// Who holds the keyboard on behalf of layer surfaces.
#[derive(Default)]
pub struct LayerFocus {
    /// The Exclusive layer that owns the keyboard; window focus changes are parked in
    /// `restore` meanwhile.
    pub exclusive: Option<LayerSurface>,
    /// Where the keyboard goes back to when the exclusive layer goes away.
    pub restore: Option<FocusTarget>,
    /// The OnDemand layer that took the keyboard by click; dropped once it dies, unmaps or
    /// stops asking for the keyboard.
    pub on_demand: Option<LayerSurface>,
    /// Exclusive layers the user took the keyboard away from (`revoke-inhibit`); they are
    /// ignored until they unmap or die.
    pub demoted: Vec<LayerSurface>,
}

pub struct LayerHit {
    pub layer: LayerSurface,
    pub surface: WlSurface,
    /// Global position of `surface`.
    pub loc: Point<f64, Logical>,
}

pub enum Hit {
    Layer(LayerHit),
    Window(WindowElement, Point<i32, Logical>),
    /// An override-redirect X11 window (menu, tooltip): it takes the pointer, never focus.
    Unmanaged(Window, Point<i32, Logical>),
    Nothing,
}

/// Where the layer's surface tree is drawn, in output-local logical coordinates.
fn layer_origin(map: &LayerMap, layer: &LayerSurface) -> Option<Point<i32, Logical>> {
    Some(map.layer_geometry(layer)?.loc - layer.geometry().loc)
}

/// The layers on `kind`, topmost first, with their output-local origin.
pub fn layers_front_to_back(
    output: &Output,
    kind: Layer,
) -> Vec<(LayerSurface, Point<i32, Logical>)> {
    let map = layer_map_for_output(output);
    map.layers_on(kind)
        .rev()
        .filter_map(|l| Some((l.clone(), layer_origin(&map, l)?)))
        .collect()
}

/// What a layer asks of the keyboard, with Exclusive below the Top layer downgraded: only
/// Top and Overlay may take the keyboard away from windows.
fn interactivity(layer: &LayerSurface) -> KeyboardInteractivity {
    match (layer.cached_state().keyboard_interactivity, layer.layer()) {
        (KeyboardInteractivity::Exclusive, Layer::Overlay | Layer::Top) => {
            KeyboardInteractivity::Exclusive
        }
        (KeyboardInteractivity::Exclusive, _) => KeyboardInteractivity::OnDemand,
        (other, _) => other,
    }
}

fn initial_configure_sent(layer: &LayerSurface) -> bool {
    with_states(layer.wl_surface(), |states| {
        states
            .data_map
            .get::<LayerSurfaceData>()
            .and_then(|d| d.lock().ok())
            .is_some_and(|d| d.initial_configure_sent)
    })
}

impl Aurora {
    /// The output and layer whose root surface is `surface`.
    pub fn layer_of(&self, surface: &WlSurface) -> Option<(Output, LayerSurface)> {
        self.wm.outputs.iter().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer = map
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)?
                .clone();
            Some((o.clone(), layer))
        })
    }

    pub(crate) fn output_at(&self, pos: Point<f64, Logical>) -> Option<Output> {
        self.space
            .outputs()
            .find(|o| {
                self.space
                    .output_geometry(o)
                    .is_some_and(|g| g.to_f64().contains(pos))
            })
            .cloned()
    }

    fn layer_hit(&self, output: &Output, kind: Layer, pos: Point<f64, Logical>) -> Option<Hit> {
        let geo = self.space.output_geometry(output)?;
        let local = pos - geo.loc.to_f64();
        let map = layer_map_for_output(output);
        map.layers_on(kind).rev().find_map(|layer| {
            let origin = layer_origin(&map, layer)?;
            let (surface, at) =
                layer.surface_under(local - origin.to_f64(), WindowSurfaceType::ALL)?;
            Some(Hit::Layer(LayerHit {
                layer: layer.clone(),
                surface,
                loc: (at + origin + geo.loc).to_f64(),
            }))
        })
    }

    /// The topmost thing at `pos`: Overlay, Top (not over a fullscreen window), windows,
    /// Bottom, Background. Nothing while the overview owns the pointer, which keeps hover,
    /// click-to-focus and every pointer focus path away from the windows behind it.
    pub fn hit_test(&self, pos: Point<f64, Logical>) -> Hit {
        if self.overview_grabs_input() {
            return Hit::Nothing;
        }
        let output = self.output_at(pos);
        let hide_top = output
            .as_ref()
            .is_some_and(|o| self.wm.output_fullscreen(o));
        let probe = |kind| output.as_ref().and_then(|o| self.layer_hit(o, kind, pos));
        probe(Layer::Overlay)
            .or_else(|| if hide_top { None } else { probe(Layer::Top) })
            .or_else(|| {
                let (window, at) = self.xwayland.unmanaged.element_under(pos)?;
                Some(Hit::Unmanaged(window.clone(), at))
            })
            .or_else(|| {
                let (window, at) = self.space.element_under(pos)?;
                Some(Hit::Window(window.clone(), at))
            })
            .or_else(|| probe(Layer::Bottom))
            .or_else(|| probe(Layer::Background))
            .unwrap_or(Hit::Nothing)
    }

    /// Commit of a surface that may be a layer's root: arranges the output's layers, sends
    /// the initial configure and relayouts when the usable area moved. Returns the output
    /// so the caller repaints just that one.
    pub fn layer_commit(&mut self, surface: &WlSurface) -> Option<Output> {
        let (output, layer) = self.layer_of(surface)?;
        let before = layer_map_for_output(&output).non_exclusive_zone();
        // Arrange first so the initial configure already carries the client's requested size.
        let after = {
            let mut map = layer_map_for_output(&output);
            map.arrange();
            map.non_exclusive_zone()
        };
        if !initial_configure_sent(&layer) {
            layer.layer_surface().send_configure();
        }
        self.layer_zone_changed(&output, before, after);
        self.refresh_layer_focus();
        Some(output)
    }

    /// The layer's client destroyed it (or went away).
    pub fn layer_removed(&mut self, surface: &WlrLayerSurface) {
        let found = self.wm.outputs.iter().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer = map.layers().find(|l| l.layer_surface() == surface)?.clone();
            Some((o.clone(), layer))
        });
        let Some((output, layer)) = found else {
            return;
        };
        let before = layer_map_for_output(&output).non_exclusive_zone();
        let after = {
            let mut map = layer_map_for_output(&output);
            map.unmap_layer(&layer);
            map.non_exclusive_zone()
        };
        tracing::info!(
            "layer: gone out={} ns={:?}",
            output.name(),
            layer.namespace()
        );
        self.layer_zone_changed(&output, before, after);
        self.refresh_layer_focus();
        self.resend_pointer_focus();
        self.queue_redraw_output(&output);
    }

    /// The Wm work-area hook: a layer changed how much of the output windows may use.
    fn layer_zone_changed(
        &mut self,
        output: &Output,
        before: Rectangle<i32, Logical>,
        after: Rectangle<i32, Logical>,
    ) {
        if before == after {
            return;
        }
        tracing::info!(
            "layer: out={} usable={},{} {}x{}",
            output.name(),
            after.loc.x,
            after.loc.y,
            after.size.w,
            after.size.h
        );
        if let Some(ws) = self.wm.active_ws.get(output).copied() {
            self.relayout_ws(ws);
        }
    }

    /// Recomputes the fullscreen rule and the exclusive keyboard owner. Call after anything
    /// that can change either: layer commit, unmap, new layer, fullscreen or workspace change.
    pub fn refresh_layer_focus(&mut self) {
        self.sync_top_hidden();
        self.layer_focus
            .demoted
            .retain(|l| l.alive() && crate::wm::apply::has_buffer(l.wl_surface()));
        let want = self.exclusive_candidate();
        if want.is_none() {
            self.release_on_demand();
        }
        if want == self.layer_focus.exclusive {
            return;
        }
        let had = std::mem::replace(&mut self.layer_focus.exclusive, want.clone());
        match want {
            Some(layer) => {
                if had.is_none() {
                    self.layer_focus.restore = self.keyboard.current_focus();
                }
                tracing::info!("focus: layer:{}", layer.namespace());
                self.set_keyboard_focus(Some(FocusTarget::Wl(layer.wl_surface().clone())));
            }
            None => {
                let restore = self
                    .layer_focus
                    .restore
                    .take()
                    .filter(|t| t.alive())
                    .or_else(|| self.window_focus_target());
                self.layer_focus.restore = None;
                tracing::info!("focus: layer released");
                self.set_keyboard_focus(restore);
            }
        }
    }

    /// The window that should hold the keyboard when no layer does.
    fn window_focus_target(&self) -> Option<FocusTarget> {
        self.wm
            .focused
            .and_then(|id| self.wm.windows.get(&id))
            .and_then(|w| w.element.focus_target())
    }

    /// Gives the keyboard back to the focused window when the on-demand holder is gone.
    fn release_on_demand(&mut self) {
        let Some(layer) = self.layer_focus.on_demand.as_ref() else {
            return;
        };
        let valid = layer.alive()
            && interactivity(layer) == KeyboardInteractivity::OnDemand
            && crate::wm::apply::has_buffer(layer.wl_surface());
        if valid {
            return;
        }
        let surface = layer.wl_surface().clone();
        self.layer_focus.on_demand = None;
        let holds = self
            .keyboard
            .current_focus()
            .is_none_or(|f| !f.alive() || f == FocusTarget::Wl(surface));
        if holds {
            tracing::info!("focus: on-demand layer released");
            let target = self.window_focus_target();
            self.set_keyboard_focus(target);
        }
    }

    fn set_keyboard_focus(&mut self, target: Option<FocusTarget>) {
        let keyboard = self.keyboard.clone();
        self.end_popup_grab_for(target.as_ref());
        keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
    }

    /// The topmost mapped Exclusive layer: Overlay before Top, the active output first.
    fn exclusive_candidate(&self) -> Option<LayerSurface> {
        let active = self.wm.active_output.iter();
        let outputs = active.chain(
            self.wm
                .outputs
                .iter()
                .filter(|o| self.wm.active_output.as_ref() != Some(*o)),
        );
        for output in outputs {
            let hide_top = self.wm.output_fullscreen(output);
            for kind in [Layer::Overlay, Layer::Top] {
                if kind == Layer::Top && hide_top {
                    continue;
                }
                let map = layer_map_for_output(output);
                let found = map.layers_on(kind).rev().find(|l| {
                    l.alive()
                        && !self.layer_focus.demoted.contains(l)
                        && interactivity(l) == KeyboardInteractivity::Exclusive
                        && crate::wm::apply::has_buffer(l.wl_surface())
                });
                if let Some(layer) = found {
                    return Some(layer.clone());
                }
            }
        }
        None
    }

    /// Records per output whether the scene skips its Top layers, repainting on change.
    fn sync_top_hidden(&mut self) {
        let outputs = self.wm.outputs.clone();
        let mut changed = false;
        for output in &outputs {
            let hidden = self.wm.output_fullscreen(output);
            if crate::scene::set_top_hidden(output, hidden) {
                changed = true;
                self.queue_redraw_output(output);
            }
        }
        if changed {
            self.resend_pointer_focus();
        }
    }

    /// Pointer focus follows what is under the cursor, which layout and layer changes can
    /// alter without any motion. A running grab owns the pointer, so it is left alone.
    pub fn resend_pointer_focus(&mut self) {
        if self.pointer.is_grabbed() {
            return;
        }
        let pointer = self.pointer.clone();
        let pos = pointer.current_location();
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time: smithay::backend::input::InputTime::from_millis(
                    std::time::Duration::from(self.clock.now()).as_millis() as u32,
                ),
            },
        );
        pointer.frame(self);
        self.activate_constraint_at(pos);
    }

    /// A click on a layer that accepts keyboard focus on demand gives it the keyboard.
    pub fn focus_layer_on_click(&mut self, layer: &LayerSurface) {
        if self.layer_focus.exclusive.is_some()
            || interactivity(layer) != KeyboardInteractivity::OnDemand
        {
            return;
        }
        let target = FocusTarget::Wl(layer.wl_surface().clone());
        if self.keyboard.current_focus().as_ref() == Some(&target) {
            return;
        }
        tracing::info!("focus: layer:{}", layer.namespace());
        self.layer_focus.on_demand = Some(layer.clone());
        self.set_keyboard_focus(Some(target));
    }

    /// Keeps a popup of a layer surface inside its output. `root` is the layer's surface.
    pub fn unconstrain_layer_popup(&self, popup: &PopupSurface, root: &WlSurface) {
        use smithay::desktop::{PopupKind, get_popup_toplevel_coords};
        let Some((output, layer)) = self.layer_of(root) else {
            return;
        };
        let Some(geo) = self.space.output_geometry(&output) else {
            return;
        };
        let origin = {
            let map = layer_map_for_output(&output);
            layer_origin(&map, &layer)
        };
        let Some(origin) = origin else { return };
        let mut target = Rectangle::from_size(geo.size);
        target.loc -= origin;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
