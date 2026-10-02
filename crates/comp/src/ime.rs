//! Input methods: text-input-v3 for applications, input-method-v2 for the IME (fcitx5), the
//! virtual keyboard it already has (`virtual_input.rs`).
//!
//! Smithay moves the text-input focus with the keyboard focus and only talks to text inputs
//! while an IME is bound, so without one all of this costs nothing. Aurora adds:
//!
//! - the IME's candidate popup, placed under the text cursor (above it when there is no room)
//!   and kept on the output, drawn and hit-tested with the window or layer it belongs to;
//! - the keyboard grab: physical keys pass the bind filter first and only what would reach
//!   the client goes to the IME. What the IME sends back through the virtual keyboard while
//!   it holds the grab goes straight to the focused client, past the grab, or it would loop
//!   back into the IME. While the session is locked nothing goes to the IME at all.
use smithay::{
    backend::input::{InputTime, KeyState},
    desktop::{PopupKind, PopupManager, space::SpaceElement, utils::bbox_from_surface_tree},
    input::keyboard::Keycode,
    reexports::wayland_server::{
        Resource,
        protocol::{wl_keyboard, wl_surface::WlSurface},
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        input_method::{InputMethodHandler, InputMethodManagerState, PopupSurface},
        seat::WaylandFocus,
        text_input::TextInputManagerState,
    },
};

use crate::{
    sandbox::{Privileged, can_view},
    state::Aurora,
};

/// Keeps both globals alive.
pub struct Ime {
    _text_input: TextInputManagerState,
    _input_method: InputMethodManagerState,
}

impl Ime {
    pub fn new(dh: &smithay::reexports::wayland_server::DisplayHandle) -> Self {
        Self {
            _text_input: TextInputManagerState::new::<Aurora>(dh),
            _input_method: InputMethodManagerState::new::<Aurora, _>(dh, |c| {
                can_view(Privileged::InputMethod, c)
            }),
        }
    }
}

/// Where the popup goes relative to the parent surface: below the text cursor `cursor`,
/// above it when it would leave `bounds` at the bottom, pushed left or right to stay inside.
/// `origin` is the parent surface's origin in the same space as `bounds`.
pub fn popup_location(
    cursor: Rectangle<i32, Logical>,
    size: (i32, i32),
    origin: Point<i32, Logical>,
    bounds: Rectangle<i32, Logical>,
) -> Point<i32, Logical> {
    let (w, h) = size;
    let mut x = origin.x + cursor.loc.x;
    let mut y = origin.y + cursor.loc.y + cursor.size.h;
    if y + h > bounds.loc.y + bounds.size.h {
        let above = origin.y + cursor.loc.y - h;
        if above >= bounds.loc.y {
            y = above;
        }
    }
    x = x.min(bounds.loc.x + bounds.size.w - w).max(bounds.loc.x);
    Point::from((x - origin.x, y - origin.y))
}

impl InputMethodHandler for Aurora {
    fn new_popup(&mut self, surface: PopupSurface) {
        self.place_ime_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            tracing::warn!(%err, "input method: cannot track the popup");
        }
    }

    fn popup_repositioned(&mut self, surface: PopupSurface) {
        self.place_ime_popup(&surface);
        self.queue_redraw_all();
    }

    fn dismiss_popup(&mut self, surface: PopupSurface) {
        if let Some(parent) = surface.get_parent().map(|p| p.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::from(surface));
        }
        self.queue_redraw_all();
    }

    /// The popup's offset is taken relative to this rectangle's corner: the window geometry
    /// for a window (whose render places popups against it), the surface origin otherwise.
    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, Logical> {
        if let Some(win) = self.wm.window_of(parent) {
            return SpaceElement::geometry(&win.element);
        }
        Rectangle::from_size(bbox_from_surface_tree(parent, (0, 0)).size)
    }
}

impl Aurora {
    /// Global origin of the surface an IME popup belongs to, and the output it is on.
    fn ime_parent_origin(
        &self,
        parent: &WlSurface,
    ) -> Option<(Point<i32, Logical>, Rectangle<i32, Logical>)> {
        let output_geo = |at: Point<i32, Logical>| {
            self.space
                .outputs()
                .filter_map(|o| self.space.output_geometry(o))
                .find(|g| g.contains(at))
                .or_else(|| {
                    let first = self.space.outputs().next()?;
                    self.space.output_geometry(first)
                })
        };
        if let Some(win) = self.wm.window_of(parent) {
            let loc = self.space.element_location(&win.element)?;
            let origin = loc - SpaceElement::geometry(&win.element).loc;
            return Some((origin, output_geo(loc)?));
        }
        let (output, layer) = self.layer_of(parent)?;
        let map = smithay::desktop::layer_map_for_output(&output);
        let geo = map.layer_geometry(&layer)?;
        let out = self.space.output_geometry(&output)?;
        Some((out.loc + geo.loc, out))
    }

    /// Puts the popup under the text cursor of its parent, on screen.
    fn place_ime_popup(&self, popup: &PopupSurface) {
        let Some(parent) = popup.get_parent().map(|p| p.surface.clone()) else {
            return;
        };
        let Some((origin, bounds)) = self.ime_parent_origin(&parent) else {
            return;
        };
        let size = bbox_from_surface_tree(popup.wl_surface(), (0, 0)).size;
        let loc = popup_location(
            popup.text_input_rectangle(),
            (size.w, size.h),
            origin,
            bounds,
        );
        popup.set_location(loc);
    }

    /// A commit of an IME popup: its size may have changed, so it is placed again.
    pub fn ime_popup_commit(&mut self, surface: &WlSurface) {
        if let Some(PopupKind::InputMethod(popup)) = self.popups.find_popup(surface) {
            self.place_ime_popup(&popup);
        }
    }

    /// Whether a key that passed the bind filter must skip the IME's keyboard grab: keys the
    /// IME (or any virtual keyboard) injects while it holds the grab, and every key while
    /// the session is locked.
    pub fn bypasses_ime(&self, virtual_key: bool) -> bool {
        use smithay::wayland::input_method::InputMethodSeat;
        (virtual_key || self.is_locked()) && self.seat.input_method().keyboard_grabbed()
    }

    /// Sends a key straight to the focused client's keyboards, past any grab.
    pub fn forward_past_ime(&mut self, keycode: Keycode, state: KeyState, time: InputTime) {
        let Some(focus) = self.keyboard.current_focus() else {
            return;
        };
        let Some(client) = focus.wl_surface().and_then(|s| s.client()) else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let mods = self.keyboard.modifier_state().serialized;
        let state = match state {
            KeyState::Pressed => wl_keyboard::KeyState::Pressed,
            KeyState::Released => wl_keyboard::KeyState::Released,
        };
        let keyboards: Vec<_> = self.keyboard.client_keyboards(&client).collect();
        for kbd in keyboards {
            kbd.modifiers(
                serial.into(),
                mods.depressed,
                mods.latched,
                mods.locked,
                mods.layout_effective,
            );
            kbd.key(
                serial.into(),
                time.millis(),
                keycode.raw().saturating_sub(8),
                state,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    const SCREEN: (i32, i32, i32, i32) = (0, 0, 1000, 800);

    fn screen() -> Rectangle<i32, Logical> {
        let (x, y, w, h) = SCREEN;
        rect(x, y, w, h)
    }

    #[test]
    fn popup_goes_under_the_cursor() {
        let at = popup_location(rect(50, 100, 2, 20), (200, 100), (10, 10).into(), screen());
        assert_eq!(at, Point::from((50, 120)));
    }

    #[test]
    fn popup_flips_above_at_the_bottom() {
        let at = popup_location(rect(50, 700, 2, 20), (200, 100), (0, 0).into(), screen());
        assert_eq!(at, Point::from((50, 600)));
    }

    #[test]
    fn popup_stays_inside_horizontally() {
        let at = popup_location(rect(950, 100, 2, 20), (200, 100), (0, 0).into(), screen());
        assert_eq!(at.x, 800);
        let at = popup_location(rect(5, 100, 2, 20), (200, 100), (-50, 0).into(), screen());
        assert_eq!(at.x, 50);
    }
}
