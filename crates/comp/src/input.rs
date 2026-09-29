use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
};

use crate::{
    config::keybind::Mods,
    emergency::{self, Emergency},
    state::Aurora,
};

/// Compositor-level key actions, decided before clients see the key.
#[derive(Clone, Copy)]
enum KeyAction {
    Emergency(Emergency),
    None,
}

fn key_action(
    modifiers: &smithay::input::keyboard::ModifiersState,
    handle: &smithay::input::keyboard::KeysymHandle<'_>,
) -> Option<KeyAction> {
    let mods = Mods::from_state(modifiers);
    let raw = handle.raw_syms();
    let raw = &raw[..];
    if let Some(action) = emergency::classify(mods, handle.raw_code(), raw, handle.modified_sym()) {
        return Some(KeyAction::Emergency(action));
    }
    if emergency::near_miss(mods, raw) {
        // Leaves a trace when a VT chord is pressed with the wrong modifiers.
        tracing::info!(
            alt = modifiers.alt,
            altgr = modifiers.iso_level3_shift,
            logo = modifiers.logo,
            "Ctrl+F-key pressed without Alt, not a VT switch"
        );
    }
    None
}

/// Clamps to the nearest output rectangle; free movement when there are none.
fn clamp_to_outputs(
    pos: Point<f64, Logical>,
    outputs: &[Rectangle<i32, Logical>],
) -> Point<f64, Logical> {
    let clamp_to = |r: &Rectangle<i32, Logical>| {
        // The far edge is exclusive, so stop one pixel short of it.
        let x = pos
            .x
            .clamp(r.loc.x as f64, (r.loc.x + r.size.w - 1).max(r.loc.x) as f64);
        let y = pos
            .y
            .clamp(r.loc.y as f64, (r.loc.y + r.size.h - 1).max(r.loc.y) as f64);
        Point::<f64, Logical>::from((x, y))
    };
    let dist = |p: Point<f64, Logical>| (p.x - pos.x).powi(2) + (p.y - pos.y).powi(2);
    outputs
        .iter()
        .map(clamp_to)
        .min_by(|a, b| dist(*a).total_cmp(&dist(*b)))
        .unwrap_or(pos)
}

impl Aurora {
    pub(crate) fn clamp_pointer(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        let geos: Vec<_> = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .collect();
        clamp_to_outputs(pos, &geos)
    }

    /// Pointer motion only changes what is drawn on outputs that hold the old or new
    /// position (cursor plane, hover state). During a grab a dragged window can span
    /// other outputs, so everything is repainted.
    fn queue_redraw_pointer(&mut self, old: Point<f64, Logical>, new: Point<f64, Logical>) {
        if self.pointer.is_grabbed() {
            self.queue_redraw_all();
            return;
        }
        let touched: Vec<_> = self
            .space
            .outputs()
            .filter(|o| {
                self.space
                    .output_geometry(o)
                    .is_some_and(|g| g.to_f64().contains(old) || g.to_f64().contains(new))
            })
            .cloned()
            .collect();
        for output in &touched {
            self.queue_redraw_output(output);
        }
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time(&event);
                let key_state = event.state();
                let keycode = event.key_code();

                // Runs before clients and ignores shortcut inhibitors on purpose: the quit
                // chord and VT switch must always work.
                let action = self.keyboard.clone().input::<KeyAction, _>(
                    self,
                    keycode,
                    key_state,
                    serial,
                    time,
                    |state, modifiers, handle| {
                        if key_state == KeyState::Pressed {
                            match key_action(modifiers, &handle) {
                                Some(action) => {
                                    state.suppressed_keys.push(keycode);
                                    FilterResult::Intercept(action)
                                }
                                None => FilterResult::Forward,
                            }
                        } else if state.suppressed_keys.contains(&keycode) {
                            state.suppressed_keys.retain(|k| *k != keycode);
                            FilterResult::Intercept(KeyAction::None)
                        } else {
                            FilterResult::Forward
                        }
                    },
                );

                match action {
                    Some(KeyAction::Emergency(Emergency::Quit)) => {
                        tracing::warn!("quitting: quit chord");
                        crate::safety::arm_exit_deadline();
                        self.loop_signal.stop();
                    }
                    Some(KeyAction::Emergency(Emergency::VtSwitch(vt))) => {
                        tracing::info!(vt, "VT switch requested");
                        self.backend.change_vt(vt);
                    }
                    Some(KeyAction::None) | None => {}
                }
                if matches!(action, Some(KeyAction::Emergency(_))) {
                    let _ = self.display_handle.flush_clients();
                }
            }
            InputEvent::PointerMotion { event, .. } => {
                let pointer = self.pointer.clone();
                let serial = SERIAL_COUNTER.next_serial();

                let old_pos = pointer.current_location();
                let pos = self.clamp_pointer(old_pos + event.delta());
                let under = self.surface_under(pos);

                pointer.motion(
                    self,
                    under.clone(),
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time(),
                    },
                );
                pointer.relative_motion(
                    self,
                    under,
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        time: event.time(),
                    },
                );
                pointer.frame(self);
                self.queue_redraw_pointer(old_pos, pos);
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else {
                    return;
                };

                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

                let serial = SERIAL_COUNTER.next_serial();

                let pointer = self.pointer.clone();
                let old_pos = pointer.current_location();

                let under = self.surface_under(pos);

                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time(),
                    },
                );
                pointer.frame(self);
                self.queue_redraw_pointer(old_pos, pos);
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.pointer.clone();
                let keyboard = self.keyboard.clone();

                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();

                let button_state = event.state();

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    if let Some((window, _loc)) = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(w, l)| (w.clone(), l))
                    {
                        self.space.raise_element(&window, true);
                        self.queue_redraw_all();
                        keyboard.set_focus(
                            self,
                            Some(window.toplevel().unwrap().wl_surface().clone()),
                            serial,
                        );
                        self.space.elements().for_each(|window| {
                            window.toplevel().unwrap().send_pending_configure();
                        });
                    } else {
                        self.space.elements().for_each(|window| {
                            window.set_activated(false);
                            window.toplevel().unwrap().send_pending_configure();
                        });
                        keyboard.set_focus(self, Option::<WlSurface>::None, serial);
                    }
                };

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();

                let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.
                });
                let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.
                });
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }

                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                let pointer = self.pointer.clone();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            _ => {}
        }
    }
}
