//! Focus model: which window has keyboard focus and how it is shown (activated state,
//! border colour, stacking).
use aurora_layout::WinId;
use smithay::{desktop::space::SpaceElement, utils::SERIAL_COUNTER};

use super::Phase;
use crate::{Aurora, layers::Hit};

impl Aurora {
    /// Focuses `id` (or nothing). `raise` also lifts it above other windows of its layer.
    pub fn focus_window(&mut self, id: Option<WinId>, raise: bool) {
        let id = id.filter(|i| {
            self.wm
                .windows
                .get(i)
                .is_some_and(|w| w.phase == Phase::Mapped)
        });
        let prev = self.wm.focused;
        // An exclusive layer owns the keyboard: window focus then only updates what the
        // keyboard returns to.
        let locked = self.layer_focus.exclusive.is_some();
        // The keyboard may sit on a layer while `wm.focused` still names the window.
        let target = id
            .and_then(|i| self.wm.windows.get(&i))
            .and_then(|w| w.element.focus_target());
        let keyboard_there = locked || id.is_none() || self.keyboard.current_focus() == target;
        if prev == id && !raise && keyboard_there {
            return;
        }

        if let Some(p) = prev.filter(|p| Some(*p) != id)
            && let Some(win) = self.wm.windows.get(&p)
        {
            SpaceElement::set_activate(&win.element, false);
            win.element.send_pending_configure();
            let ws = win.ws;
            self.redraw_ws(ws);
        }

        self.wm.focused = id;
        let keyboard = self.keyboard.clone();
        let serial = SERIAL_COUNTER.next_serial();
        match id.and_then(|i| self.wm.windows.get(&i)) {
            Some(win) => {
                let (ws, element) = (win.ws, win.element.clone());
                if let Some(workspace) = self.wm.workspaces.get_mut(&ws) {
                    workspace.note_focus(element.id());
                }
                if let Some(output) = self.wm.output_for_ws(ws) {
                    self.wm.active_output = Some(output);
                }
                SpaceElement::set_activate(&element, true);
                element.send_pending_configure();
                if raise && self.space.element_location(&element).is_some() {
                    self.space.raise_element(&element, false);
                }
                if locked {
                    self.layer_focus.restore = element.focus_target();
                } else {
                    keyboard.set_focus(self, element.focus_target(), serial);
                }
                let out = self
                    .wm
                    .output_for_ws(ws)
                    .map_or_else(String::new, |o| o.name());
                tracing::info!(
                    "focus: {}:{}@ws{ws}/{out}",
                    element.id().0,
                    self.app_id(element.id())
                );
                self.redraw_ws(ws);
            }
            None => {
                if locked {
                    self.layer_focus.restore = None;
                } else {
                    keyboard.set_focus(self, None, serial);
                }
                if prev.is_some() {
                    tracing::info!("focus: none");
                }
            }
        }
    }

    fn app_id(&self, id: WinId) -> &str {
        self.wm.windows.get(&id).map_or("", |w| w.app_id.as_str())
    }

    fn redraw_ws(&mut self, ws: u32) {
        if let Some(output) = self.wm.output_for_ws(ws) {
            self.queue_redraw_output(&output);
        }
    }

    /// The window under the pointer changed: focus follows it when configured. Called on
    /// motion, but only acts on a change, so a stationary hover never steals focus back.
    pub fn update_hover(&mut self) {
        let pos = self.pointer.current_location();
        let hover = match self.hit_test(pos) {
            Hit::Window(window, _) => Some(window.id()),
            _ => None,
        };
        if hover == self.wm.hover {
            return;
        }
        self.wm.hover = hover;
        if self.config.general.focus_follows_mouse
            && !self.pointer.is_grabbed()
            && let Some(id) = hover
            && self.wm.focused != Some(id)
        {
            self.focus_window(Some(id), false);
        }
    }

    /// Click-to-focus: the window under the pointer, or a layer that takes the keyboard on
    /// demand. A click on a bar never focuses the window behind it.
    pub fn focus_under_pointer(&mut self) {
        let pos = self.pointer.current_location();
        match self.hit_test(pos) {
            Hit::Window(window, _) => {
                let id = window.id();
                // Only floating windows change stacking on a click; tiles never overlap.
                let raise = self
                    .wm
                    .windows
                    .get(&id)
                    .and_then(|w| self.wm.workspaces.get(&w.ws))
                    .is_some_and(|ws| ws.is_floating(id));
                self.focus_window(Some(id), raise);
            }
            Hit::Layer(hit) => self.focus_layer_on_click(&hit.layer),
            Hit::Nothing => {}
        }
    }
}
