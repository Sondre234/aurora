//! Drawing tablets (zwp_tablet_v2): libinput tablet tools go to the surface under them.
//!
//! The tablet area maps onto the output the pointer is on. The tool also moves the pointer,
//! through the normal absolute-motion path, so focus, hover, the cursor and pointer-only
//! clients follow it; a tip press focuses what is under it like a click. No pad support
//! (buttons and rings on the tablet itself) and no per-tool cursor.
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Device, DeviceCapability, Event, InputBackend, ProximityState,
        TabletToolButtonEvent, TabletToolEvent, TabletToolProximityEvent, TabletToolTipEvent,
        TabletToolTipState,
    },
    input::tablet::{
        TabletDescriptor, TabletSeatTrait,
        tool::{
            AxisFrame, ButtonEvent, DownEvent, MotionEvent, ProximityInEvent, ProximityOutEvent,
            UpEvent,
        },
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::seat::WaylandFocus,
};

use crate::state::Aurora;

/// The axes that changed in `event`.
fn axis_frame<E: TabletToolEvent<B>, B: InputBackend>(event: &E) -> AxisFrame {
    AxisFrame {
        pressure: event.pressure_has_changed().then(|| event.pressure()),
        distance: event.distance_has_changed().then(|| event.distance()),
        tilt: event.tilt_has_changed().then(|| event.tilt()),
        rotation: event.rotation_has_changed().then(|| event.rotation()),
        slider: event.slider_has_changed().then(|| event.slider_position()),
        wheel: event
            .wheel_has_changed()
            .then(|| (event.wheel_delta(), event.wheel_delta_discrete())),
    }
}

impl Aurora {
    pub(super) fn tablet_device_added<B: InputBackend>(&mut self, device: &B::Device) {
        if device.has_capability(DeviceCapability::TabletTool) {
            let dh = self.display_handle.clone();
            self.seat
                .tablet_seat()
                .add_wp_tablet(&dh, &TabletDescriptor::from(device));
        }
    }

    pub(super) fn tablet_device_removed<B: InputBackend>(&mut self, device: &B::Device) {
        if device.has_capability(DeviceCapability::TabletTool) {
            let seat = self.seat.tablet_seat();
            seat.remove_tablet(&TabletDescriptor::from(device));
            if seat.count_tablets() == 0 {
                seat.clear_tools();
            }
        }
    }

    /// Where the tool is, in global coordinates: the tablet spans the pointer's output.
    fn tablet_position<B: InputBackend>(
        &self,
        event: &impl AbsolutePositionEvent<B>,
    ) -> Option<Point<f64, Logical>> {
        let pointer = self.pointer.current_location();
        let geo = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .find(|g| g.to_f64().contains(pointer))
            .or_else(|| {
                let first = self.space.outputs().next()?;
                self.space.output_geometry(first)
            })?;
        Some(event.position_transformed(geo.size) + geo.loc.to_f64())
    }

    /// Moves the pointer along and returns the tool's focus at `pos`.
    fn tablet_focus(
        &mut self,
        pos: Point<f64, Logical>,
        time: smithay::backend::input::InputTime,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.on_pointer_motion_absolute(pos, time);
        let (target, loc) = self.surface_under(pos)?;
        let surface = target.wl_surface()?.into_owned();
        Some((surface, loc))
    }

    pub(super) fn on_tablet_axis<B: InputBackend>(&mut self, event: B::TabletToolAxisEvent) {
        let Some(pos) = self.tablet_position(&event) else {
            return;
        };
        let time = Event::time(&event);
        let focus = self.tablet_focus(pos, time);
        let Some(tool) = self.seat.tablet_seat().get_tool(&event.tool()) else {
            return;
        };
        tool.axis(self, axis_frame(&event));
        tool.motion(
            self,
            focus,
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        tool.frame(self, time);
    }

    pub(super) fn on_tablet_proximity<B: InputBackend>(
        &mut self,
        event: B::TabletToolProximityEvent,
    ) {
        let Some(pos) = self.tablet_position(&event) else {
            return;
        };
        let time = Event::time(&event);
        let focus = self.tablet_focus(pos, time);
        let seat = self.seat.tablet_seat();
        let Some(tablet) = seat.get_tablet(&TabletDescriptor::from(&event.device())) else {
            return;
        };
        let descriptor = event.tool();
        let dh = self.display_handle.clone();
        let tool = seat
            .get_tool(&descriptor)
            .unwrap_or_else(|| seat.add_wp_tool(self, &dh, &descriptor));
        match event.state() {
            ProximityState::In => tool.proximity_in(
                self,
                focus,
                tablet,
                &ProximityInEvent {
                    location: pos,
                    axis: Some(axis_frame(&event)),
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            ),
            ProximityState::Out => tool.proximity_out(
                self,
                &ProximityOutEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            ),
        }
        tool.frame(self, time);
    }

    pub(super) fn on_tablet_tip<B: InputBackend>(&mut self, event: B::TabletToolTipEvent) {
        let Some(tool) = self.seat.tablet_seat().get_tool(&event.tool()) else {
            return;
        };
        let time = Event::time(&event);
        let serial = SERIAL_COUNTER.next_serial();
        match event.tip_state() {
            TabletToolTipState::Down => {
                tool.down(self, &DownEvent { serial, time });
                if !self.pointer.is_grabbed() && !self.overview_grabs_input() {
                    self.focus_under_pointer();
                }
            }
            TabletToolTipState::Up => tool.up(self, &UpEvent { serial, time }),
        }
        tool.frame(self, time);
    }

    pub(super) fn on_tablet_button<B: InputBackend>(&mut self, event: B::TabletToolButtonEvent) {
        let Some(tool) = self.seat.tablet_seat().get_tool(&event.tool()) else {
            return;
        };
        let time = Event::time(&event);
        tool.button(
            self,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                button: event.button(),
                state: event.button_state(),
                time,
            },
        );
        tool.frame(self, time);
    }
}
