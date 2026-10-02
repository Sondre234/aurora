//! Input. `process_input_event` only decodes backend events; the `on_*` primitives in
//! `keyboard` and `pointer` are backend independent, so the virtual keyboard and the QA
//! hooks drive exactly the same paths as real devices.
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, Event, InputBackend, InputEvent, KeyboardKeyEvent,
        PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::keyboard::KeyboardSource,
};

use crate::state::Aurora;
use keyboard::Repeat;
use pointer::{AxisInput, AxisValue};

pub mod constraints;
pub mod keyboard;
pub mod pointer;

/// Bind bookkeeping that outlives a single event.
#[derive(Default)]
pub struct InputState {
    /// The held repeat bind, if any.
    pub repeat: Option<Repeat>,
    /// Buttons whose press was taken by a mouse bind; their release is swallowed too.
    pub suppressed_buttons: Vec<u32>,
    /// Vertical wheel movement not yet worth a whole notch, in v120 units.
    pub wheel_v120: f64,
}

impl Aurora {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        self.notify_activity();
        self.wake_on_input(&event);
        match event {
            InputEvent::Keyboard { event, .. } => {
                self.on_key(
                    KeyboardSource::MAIN,
                    event.key_code(),
                    event.state(),
                    Event::time(&event),
                );
            }
            InputEvent::PointerMotion { event, .. } => {
                self.on_pointer_motion_relative(
                    event.delta(),
                    event.delta_unaccel(),
                    Event::time(&event),
                );
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(geo) = self
                    .space
                    .outputs()
                    .find(|o| o.name() == "winit")
                    .and_then(|o| self.space.output_geometry(o))
                else {
                    return;
                };
                let pos = event.position_transformed(geo.size) + geo.loc.to_f64();
                self.on_pointer_motion_absolute(pos, Event::time(&event));
            }
            InputEvent::PointerButton { event, .. } => {
                self.on_pointer_button(
                    event.button_code(),
                    event.state(),
                    Event::time(&event),
                    true,
                );
            }
            InputEvent::PointerAxis { event, .. } => {
                let value = |axis| AxisValue {
                    amount: event.amount(axis),
                    v120: event.amount_v120(axis),
                };
                self.on_axis(AxisInput {
                    time: Event::time(&event),
                    source: event.source(),
                    horizontal: value(Axis::Horizontal),
                    vertical: value(Axis::Vertical),
                });
            }
            _ => {}
        }
    }
}
