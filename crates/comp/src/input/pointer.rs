use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, InputTime},
    input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
};

use crate::{
    action::Action,
    config::keybind::{Chord, Mods, Trigger, WheelDir},
    state::Aurora,
    wm::grabs::DragKind,
};

/// One axis of a scroll event, as the backend reported it.
#[derive(Clone, Copy, Default)]
pub struct AxisValue {
    pub amount: Option<f64>,
    pub v120: Option<f64>,
}

pub struct AxisInput {
    pub time: InputTime,
    pub source: AxisSource,
    pub horizontal: AxisValue,
    pub vertical: AxisValue,
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

/// At most this many binds fire for one wheel frame, so a runaway device cannot flood actions.
const MAX_WHEEL_STEPS: usize = 8;

impl Aurora {
    /// Starts the drag a mouse bind asked for on the window under the pointer.
    fn start_bound_drag(&mut self, kind: DragKind, button: u32) -> bool {
        let pos = self.pointer.current_location();
        let Some(id) = self.space.element_under(pos).map(|(e, _)| e.id()) else {
            return false;
        };
        self.start_drag(id, kind, None, button, SERIAL_COUNTER.next_serial())
    }

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

    pub fn on_pointer_motion_relative(
        &mut self,
        delta: Point<f64, Logical>,
        delta_unaccel: Point<f64, Logical>,
        time: InputTime,
    ) {
        let pointer = self.pointer.clone();
        let old = pointer.current_location();
        let constraint = self.current_constraint().filter(|c| c.active);
        let mut pos = self.clamp_pointer(old + delta);
        let locked = constraint.as_ref().is_some_and(|c| c.locked);
        if let Some(c) = constraint.as_ref().filter(|c| !c.locked) {
            pos = self.confine(c, old, pos);
        }
        let under = self.surface_under(if locked { old } else { pos });
        // A locked pointer stays put; the client only hears the relative movement.
        if !locked {
            pointer.motion(
                self,
                under.clone(),
                &MotionEvent {
                    location: pos,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
        }
        pointer.relative_motion(
            self,
            under,
            &RelativeMotionEvent {
                delta,
                delta_unaccel,
                time,
            },
        );
        pointer.frame(self);
        if !locked {
            self.activate_constraint_at(pos);
            self.queue_redraw_pointer(old, pos);
            self.update_hover();
        }
    }

    /// `pos` is in global logical coordinates.
    pub fn on_pointer_motion_absolute(&mut self, pos: Point<f64, Logical>, time: InputTime) {
        let pointer = self.pointer.clone();
        let old = pointer.current_location();
        let constraint = self.current_constraint().filter(|c| c.active);
        if constraint.as_ref().is_some_and(|c| c.locked) {
            return;
        }
        let mut pos = self.clamp_pointer(pos);
        if let Some(c) = &constraint {
            pos = self.confine(c, old, pos);
        }
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        self.activate_constraint_at(pos);
        self.queue_redraw_pointer(old, pos);
        self.update_hover();
    }

    /// `honour_binds` is false for callers that must reach the client unconditionally.
    pub fn on_pointer_button(
        &mut self,
        button: u32,
        state: ButtonState,
        time: InputTime,
        honour_binds: bool,
    ) {
        let pointer = self.pointer.clone();

        if state == ButtonState::Released {
            let held = &mut self.input.suppressed_buttons;
            if let Some(i) = held.iter().position(|b| *b == button) {
                held.swap_remove(i);
                return;
            }
        } else {
            if !pointer.is_grabbed() {
                self.focus_under_pointer();
            }
            if honour_binds && self.binds_allowed() {
                let chord = Chord {
                    mods: Mods::from_state(&self.keyboard.modifier_state()),
                    trigger: Trigger::Button(button),
                };
                if let Some(bind) = self.config.binds.get(&chord).cloned() {
                    let kind = match bind.action {
                        Action::DragMove => Some(DragKind::Move),
                        Action::DragResize => Some(DragKind::Resize),
                        _ => None,
                    };
                    match kind {
                        // The grab goes in first and then takes the press below, so the
                        // client under the pointer never sees it; its release ends the grab.
                        Some(kind) if self.start_bound_drag(kind, button) => {
                            tracing::info!("action: {}", bind.action);
                        }
                        _ => {
                            self.input.suppressed_buttons.push(button);
                            self.dispatch(bind.action);
                            return;
                        }
                    }
                }
            }
        }

        pointer.button(
            self,
            &ButtonEvent {
                button,
                state,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
    }

    pub fn on_axis(&mut self, input: AxisInput) {
        if input.source == AxisSource::Wheel
            && let Some(v120) = input.vertical.v120.filter(|v| *v != 0.0)
            && self.wheel_bind(v120)
        {
            return;
        }

        let amount = |a: AxisValue| {
            a.amount
                .unwrap_or_else(|| a.v120.unwrap_or(0.0) * 15.0 / 120.)
        };
        let mut frame = AxisFrame::new(input.time).source(input.source);
        for (axis, value) in [
            (Axis::Horizontal, input.horizontal),
            (Axis::Vertical, input.vertical),
        ] {
            let amount = amount(value);
            if amount != 0.0 {
                frame = frame.value(axis, amount);
                if let Some(discrete) = value.v120 {
                    frame = frame.v120(axis, discrete as i32);
                }
            }
            if input.source == AxisSource::Finger && value.amount == Some(0.0) {
                frame = frame.stop(axis);
            }
        }

        let pointer = self.pointer.clone();
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Accumulates wheel movement and fires the wheel bind for the current modifiers once
    /// per whole notch. True when a bind exists, which consumes the frame either way.
    fn wheel_bind(&mut self, v120: f64) -> bool {
        let dir = if v120 > 0.0 {
            WheelDir::Down
        } else {
            WheelDir::Up
        };
        let chord = Chord {
            mods: Mods::from_state(&self.keyboard.modifier_state()),
            trigger: Trigger::Wheel(dir),
        };
        let bind = if self.binds_allowed() {
            self.config.binds.get(&chord).cloned()
        } else {
            None
        };
        let Some(bind) = bind else {
            self.input.wheel_v120 = 0.0;
            return false;
        };
        self.input.wheel_v120 += v120;
        let notches = (self.input.wheel_v120 / 120.0).trunc();
        self.input.wheel_v120 -= notches * 120.0;
        for _ in 0..(notches.abs() as usize).min(MAX_WHEEL_STEPS) {
            self.dispatch(bind.action.clone());
        }
        true
    }
}
