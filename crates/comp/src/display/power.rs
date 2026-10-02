//! Monitor power: `zwlr_output_power_manager_v1` (wlopm, swayidle's DPMS hooks) and the
//! `power-off-monitors` / `power-on-monitors` actions. Power is per output; the actions and
//! input touch all of them. Any input that a person makes (a key or button press, pointer
//! motion, scrolling, touch, tablet) powers every output back on, like niri. Key and button
//! releases do not, so the release of the key that ran `power-off-monitors` cannot undo it.
//!
//! The flag lives in the output's user data, so the render paths read it without borrowing
//! the state. The session lock is independent: locking while off works (the lock surface is
//! what shows on wake), and the first wake-up key goes to the lock surface as usual.
use std::sync::atomic::{AtomicBool, Ordering};

use smithay::{
    backend::input::{
        ButtonState, InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerButtonEvent,
    },
    output::Output,
    reexports::{
        wayland_protocols_wlr::output_power_management::v1::server::{
            zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
            zwlr_output_power_v1::{self, Mode, ZwlrOutputPowerV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, WEnum,
            backend::{ClientId, GlobalId},
        },
    },
    wayland::{Dispatch2, GlobalDispatch2},
};

use crate::{backend::Backend, state::Aurora};

#[derive(Default)]
struct PoweredOff(AtomicBool);

/// Whether `output` is powered off (DPMS off): nothing is rendered or scanned out.
pub fn is_off(output: &Output) -> bool {
    output
        .user_data()
        .get::<PoweredOff>()
        .is_some_and(|f| f.0.load(Ordering::Relaxed))
}

/// Sets the flag and says whether it changed.
fn set_off(output: &Output, off: bool) -> bool {
    let flag = output
        .user_data()
        .get_or_insert_threadsafe(PoweredOff::default);
    flag.0.swap(off, Ordering::Relaxed) != off
}

/// Input a person makes, as opposed to device hotplug, releases or the end of a gesture.
fn wakes<I: InputBackend>(event: &InputEvent<I>) -> bool {
    match event {
        InputEvent::Keyboard { event } => event.state() == KeyState::Pressed,
        InputEvent::PointerButton { event } => event.state() == ButtonState::Pressed,
        InputEvent::PointerMotion { .. }
        | InputEvent::PointerMotionAbsolute { .. }
        | InputEvent::PointerAxis { .. }
        | InputEvent::GestureSwipeBegin { .. }
        | InputEvent::GesturePinchBegin { .. }
        | InputEvent::GestureHoldBegin { .. }
        | InputEvent::TouchDown { .. }
        | InputEvent::TouchMotion { .. }
        | InputEvent::TabletToolAxis { .. }
        | InputEvent::TabletToolProximity { .. }
        | InputEvent::TabletToolTip { .. }
        | InputEvent::TabletToolButton { .. } => true,
        _ => false,
    }
}

pub struct PowerState {
    _global: GlobalId,
    /// Live `zwlr_output_power_v1` objects and the output each controls.
    controls: Vec<(ZwlrOutputPowerV1, Output)>,
    /// Some output is off; checked on every input event, so it is kept cached.
    any_off: bool,
}

impl PowerState {
    pub fn new(dh: &DisplayHandle) -> Self {
        Self {
            _global: dh.create_global::<Aurora, ZwlrOutputPowerManagerV1, _>(1, ManagerGlobal),
            controls: Vec::new(),
            any_off: false,
        }
    }
}

fn mode(on: bool) -> Mode {
    if on { Mode::On } else { Mode::Off }
}

impl Aurora {
    /// Powers one output on or off. A no-op when it already is.
    pub fn set_output_power(&mut self, output: &Output, on: bool) {
        if !set_off(output, !on) {
            return;
        }
        self.display.power.any_off = self.wm.outputs.iter().any(is_off);
        let name = output.name();
        tracing::info!("power: output={name} {}", if on { "on" } else { "off" });
        match self.backend {
            Backend::Drm(_) => self.drm_set_power(output, on),
            Backend::Winit => {
                tracing::info!("power: nested backend, {name} is not really switched");
            }
        }
        for (control, _) in self
            .display
            .power
            .controls
            .iter()
            .filter(|(_, o)| o == output)
        {
            control.mode(mode(on));
        }
        if on {
            self.queue_redraw_output(output);
        }
    }

    /// `power-off-monitors` / `power-on-monitors`, and the IPC requests of the same name.
    pub fn power_all(&mut self, on: bool) {
        for output in self.wm.outputs.clone() {
            self.set_output_power(&output, on);
        }
    }

    /// Called for every backend input event before it is processed.
    pub fn wake_on_input<I: InputBackend>(&mut self, event: &InputEvent<I>) {
        if self.display.power.any_off && wakes(event) {
            tracing::info!("power: input wakes the outputs");
            self.power_all(true);
        }
    }

    /// The output disappears: its controls fail and the flag goes with the output.
    pub(super) fn power_output_removed(&mut self, output: &Output) {
        set_off(output, false);
        let power = &mut self.display.power;
        power.controls.retain(|(control, o)| {
            if o == output {
                control.failed();
                false
            } else {
                true
            }
        });
        power.any_off = self
            .wm
            .outputs
            .iter()
            .filter(|o| *o != output)
            .any(is_off);
    }
}

pub struct ManagerGlobal;
pub struct Manager;
pub struct Control;

impl GlobalDispatch2<ZwlrOutputPowerManagerV1, Aurora> for ManagerGlobal {
    fn bind(
        &self,
        _state: &mut Aurora,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        data_init.init(resource, Manager);
    }
}

impl Dispatch2<ZwlrOutputPowerManagerV1, Aurora> for Manager {
    fn request(
        &self,
        state: &mut Aurora,
        _client: &Client,
        _resource: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        if let zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } = request {
            let control = data_init.init(id, Control);
            match Output::from_resource(&output).filter(|o| state.wm.outputs.contains(o)) {
                Some(output) => {
                    control.mode(mode(!is_off(&output)));
                    state.display.power.controls.push((control, output));
                }
                None => control.failed(),
            }
        }
    }
}

impl Dispatch2<ZwlrOutputPowerV1, Aurora> for Control {
    fn request(
        &self,
        state: &mut Aurora,
        _client: &Client,
        resource: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Aurora>,
    ) {
        let zwlr_output_power_v1::Request::SetMode { mode } = request else {
            return;
        };
        let on = match mode {
            WEnum::Value(Mode::On) => true,
            WEnum::Value(Mode::Off) => false,
            _ => {
                resource.post_error(
                    zwlr_output_power_v1::Error::InvalidMode,
                    "unknown power mode",
                );
                return;
            }
        };
        // A control that failed (its output is gone) is inert.
        let Some(output) = state
            .display
            .power
            .controls
            .iter()
            .find(|(c, _)| c == resource)
            .map(|(_, o)| o.clone())
        else {
            return;
        };
        state.set_output_power(&output, on);
    }

    fn destroyed(&self, state: &mut Aurora, _client: ClientId, resource: &ZwlrOutputPowerV1) {
        state
            .display
            .power
            .controls
            .retain(|(control, _)| control != resource);
    }
}
