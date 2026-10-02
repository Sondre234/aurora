//! Touchpad gestures (zwp_pointer_gestures_v1): libinput's swipe, pinch and hold go to the
//! client under the pointer, through whatever grab holds the pointer. Aurora binds none.
use smithay::{
    backend::input::{
        Event, GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent as _,
        GestureSwipeUpdateEvent as _, InputBackend,
    },
    input::pointer::{
        GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
        GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
        GestureSwipeUpdateEvent,
    },
    utils::SERIAL_COUNTER,
};

use crate::state::Aurora;

impl Aurora {
    pub(super) fn on_swipe_begin<B: InputBackend>(&mut self, event: B::GestureSwipeBeginEvent) {
        let pointer = self.pointer.clone();
        pointer.gesture_swipe_begin(
            self,
            &GestureSwipeBeginEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: Event::time(&event),
                fingers: event.fingers(),
            },
        );
    }

    pub(super) fn on_swipe_update<B: InputBackend>(&mut self, event: B::GestureSwipeUpdateEvent) {
        let pointer = self.pointer.clone();
        pointer.gesture_swipe_update(
            self,
            &GestureSwipeUpdateEvent {
                time: Event::time(&event),
                delta: event.delta(),
            },
        );
    }

    pub(super) fn on_swipe_end<B: InputBackend>(&mut self, event: B::GestureSwipeEndEvent) {
        let pointer = self.pointer.clone();
        pointer.gesture_swipe_end(
            self,
            &GestureSwipeEndEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: Event::time(&event),
                cancelled: event.cancelled(),
            },
        );
    }

    pub(super) fn on_pinch_begin<B: InputBackend>(&mut self, event: B::GesturePinchBeginEvent) {
        let pointer = self.pointer.clone();
        pointer.gesture_pinch_begin(
            self,
            &GesturePinchBeginEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: Event::time(&event),
                fingers: event.fingers(),
            },
        );
    }

    pub(super) fn on_pinch_update<B: InputBackend>(&mut self, event: B::GesturePinchUpdateEvent) {
        let pointer = self.pointer.clone();
        pointer.gesture_pinch_update(
            self,
            &GesturePinchUpdateEvent {
                time: Event::time(&event),
                delta: event.delta(),
                scale: event.scale(),
                rotation: event.rotation(),
            },
        );
    }

    pub(super) fn on_pinch_end<B: InputBackend>(&mut self, event: B::GesturePinchEndEvent) {
        let pointer = self.pointer.clone();
        pointer.gesture_pinch_end(
            self,
            &GesturePinchEndEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: Event::time(&event),
                cancelled: event.cancelled(),
            },
        );
    }

    pub(super) fn on_hold_begin<B: InputBackend>(&mut self, event: B::GestureHoldBeginEvent) {
        let pointer = self.pointer.clone();
        pointer.gesture_hold_begin(
            self,
            &GestureHoldBeginEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: Event::time(&event),
                fingers: event.fingers(),
            },
        );
    }

    pub(super) fn on_hold_end<B: InputBackend>(&mut self, event: B::GestureHoldEndEvent) {
        let pointer = self.pointer.clone();
        pointer.gesture_hold_end(
            self,
            &GestureHoldEndEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: Event::time(&event),
                cancelled: event.cancelled(),
            },
        );
    }
}
