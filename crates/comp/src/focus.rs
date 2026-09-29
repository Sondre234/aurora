//! One focus type for keyboard and pointer: a Wayland surface, or an X11 window that
//! needs X11-side focus handling on top of its wl_surface.
use std::{borrow::Cow, sync::Arc};

use smithay::{
    backend::input::{InputTime, KeyState},
    desktop::PopupKind,
    input::{
        Seat,
        dnd::{DndFocus, OfferData, Source},
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent,
            PointerTarget, RelativeMotionEvent,
        },
    },
    reexports::wayland_server::{
        DisplayHandle, backend::ObjectId, protocol::wl_surface::WlSurface,
    },
    utils::{IsAlive, Logical, Point, Serial},
    wayland::{seat::WaylandFocus, selection::data_device::WlOfferData},
    xwayland::{X11Surface, xwm::XwmOfferData},
};

use crate::state::Aurora;

#[derive(Debug, Clone, PartialEq)]
// X11Surface is big, but a focus target is a short-lived handle, never stored in bulk.
#[allow(clippy::large_enum_variant)]
pub enum FocusTarget {
    Wl(WlSurface),
    X11(X11Surface),
}

impl IsAlive for FocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Wl(s) => s.alive(),
            Self::X11(s) => s.alive(),
        }
    }
}

impl WaylandFocus for FocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wl(s) => Some(Cow::Borrowed(s)),
            Self::X11(s) => s.wl_surface().map(Cow::Owned),
        }
    }
    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        match self {
            Self::Wl(s) => s.same_client_as(object_id),
            Self::X11(s) => s.same_client_as(object_id),
        }
    }
}

impl From<WlSurface> for FocusTarget {
    fn from(s: WlSurface) -> Self {
        Self::Wl(s)
    }
}
impl From<X11Surface> for FocusTarget {
    fn from(s: X11Surface) -> Self {
        Self::X11(s)
    }
}
impl From<PopupKind> for FocusTarget {
    fn from(p: PopupKind) -> Self {
        Self::Wl(p.wl_surface().clone())
    }
}

impl FocusTarget {
    fn kb(&self) -> &dyn KeyboardTarget<Aurora> {
        match self {
            Self::Wl(s) => s,
            Self::X11(s) => s,
        }
    }
    fn ptr(&self) -> &dyn PointerTarget<Aurora> {
        match self {
            Self::Wl(s) => s,
            Self::X11(s) => s,
        }
    }
}

impl KeyboardTarget<Aurora> for FocusTarget {
    fn enter(
        &self,
        seat: &Seat<Aurora>,
        data: &mut Aurora,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        self.kb().enter(seat, data, keys, serial)
    }
    fn leave(&self, seat: &Seat<Aurora>, data: &mut Aurora, serial: Serial) {
        self.kb().leave(seat, data, serial)
    }
    fn key(
        &self,
        seat: &Seat<Aurora>,
        data: &mut Aurora,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: InputTime,
    ) {
        self.kb().key(seat, data, key, state, serial, time)
    }
    fn modifiers(
        &self,
        seat: &Seat<Aurora>,
        data: &mut Aurora,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        self.kb().modifiers(seat, data, modifiers, serial)
    }
}

impl PointerTarget<Aurora> for FocusTarget {
    fn enter(&self, seat: &Seat<Aurora>, data: &mut Aurora, e: &MotionEvent) {
        self.ptr().enter(seat, data, e)
    }
    fn motion(&self, seat: &Seat<Aurora>, data: &mut Aurora, e: &MotionEvent) {
        self.ptr().motion(seat, data, e)
    }
    fn relative_motion(&self, seat: &Seat<Aurora>, data: &mut Aurora, e: &RelativeMotionEvent) {
        self.ptr().relative_motion(seat, data, e)
    }
    fn button(&self, seat: &Seat<Aurora>, data: &mut Aurora, e: &ButtonEvent) {
        self.ptr().button(seat, data, e)
    }
    fn axis(&self, seat: &Seat<Aurora>, data: &mut Aurora, f: AxisFrame) {
        self.ptr().axis(seat, data, f)
    }
    fn frame(&self, seat: &Seat<Aurora>, data: &mut Aurora) {
        self.ptr().frame(seat, data)
    }
    fn leave(&self, seat: &Seat<Aurora>, data: &mut Aurora, serial: Serial, time: InputTime) {
        self.ptr().leave(seat, data, serial, time)
    }
    fn gesture_swipe_begin(
        &self,
        seat: &Seat<Aurora>,
        data: &mut Aurora,
        e: &GestureSwipeBeginEvent,
    ) {
        self.ptr().gesture_swipe_begin(seat, data, e)
    }
    fn gesture_swipe_update(
        &self,
        seat: &Seat<Aurora>,
        data: &mut Aurora,
        e: &GestureSwipeUpdateEvent,
    ) {
        self.ptr().gesture_swipe_update(seat, data, e)
    }
    fn gesture_swipe_end(&self, seat: &Seat<Aurora>, data: &mut Aurora, e: &GestureSwipeEndEvent) {
        self.ptr().gesture_swipe_end(seat, data, e)
    }
    fn gesture_pinch_begin(
        &self,
        seat: &Seat<Aurora>,
        data: &mut Aurora,
        e: &GesturePinchBeginEvent,
    ) {
        self.ptr().gesture_pinch_begin(seat, data, e)
    }
    fn gesture_pinch_update(
        &self,
        seat: &Seat<Aurora>,
        data: &mut Aurora,
        e: &GesturePinchUpdateEvent,
    ) {
        self.ptr().gesture_pinch_update(seat, data, e)
    }
    fn gesture_pinch_end(&self, seat: &Seat<Aurora>, data: &mut Aurora, e: &GesturePinchEndEvent) {
        self.ptr().gesture_pinch_end(seat, data, e)
    }
    fn gesture_hold_begin(
        &self,
        seat: &Seat<Aurora>,
        data: &mut Aurora,
        e: &GestureHoldBeginEvent,
    ) {
        self.ptr().gesture_hold_begin(seat, data, e)
    }
    fn gesture_hold_end(&self, seat: &Seat<Aurora>, data: &mut Aurora, e: &GestureHoldEndEvent) {
        self.ptr().gesture_hold_end(seat, data, e)
    }
}

pub enum AuroraOfferData<S: Source> {
    Wayland(WlOfferData<S>),
    X11(XwmOfferData<S>),
}

impl<S: Source> OfferData for AuroraOfferData<S> {
    fn disable(&self) {
        match self {
            Self::Wayland(d) => d.disable(),
            Self::X11(d) => d.disable(),
        }
    }
    fn drop(&self) {
        match self {
            Self::Wayland(d) => d.drop(),
            Self::X11(d) => d.drop(),
        }
    }
    fn validated(&self) -> bool {
        match self {
            Self::Wayland(d) => d.validated(),
            Self::X11(d) => d.validated(),
        }
    }
}

impl DndFocus<Aurora> for FocusTarget {
    type OfferData<S>
        = AuroraOfferData<S>
    where
        S: Source;

    fn enter<S: Source>(
        &self,
        data: &mut Aurora,
        dh: &DisplayHandle,
        source: Arc<S>,
        seat: &Seat<Aurora>,
        location: Point<f64, Logical>,
        serial: &Serial,
    ) -> Option<AuroraOfferData<S>> {
        match self {
            Self::Wl(s) => DndFocus::enter(s, data, dh, source, seat, location, serial)
                .map(AuroraOfferData::Wayland),
            Self::X11(s) => DndFocus::enter(s, data, dh, source, seat, location, serial)
                .map(AuroraOfferData::X11),
        }
    }
    fn motion<S: Source>(
        &self,
        data: &mut Aurora,
        offer: Option<&mut AuroraOfferData<S>>,
        seat: &Seat<Aurora>,
        location: Point<f64, Logical>,
        time: InputTime,
    ) {
        match (self, offer) {
            (Self::Wl(s), Some(AuroraOfferData::Wayland(o))) => {
                DndFocus::motion(s, data, Some(o), seat, location, time)
            }
            (Self::Wl(s), None) => DndFocus::motion::<S>(s, data, None, seat, location, time),
            (Self::X11(s), Some(AuroraOfferData::X11(o))) => {
                DndFocus::motion(s, data, Some(o), seat, location, time)
            }
            (Self::X11(s), None) => DndFocus::motion::<S>(s, data, None, seat, location, time),
            _ => {}
        }
    }
    fn leave<S: Source>(
        &self,
        data: &mut Aurora,
        offer: Option<&mut AuroraOfferData<S>>,
        seat: &Seat<Aurora>,
    ) {
        match (self, offer) {
            (Self::Wl(s), Some(AuroraOfferData::Wayland(o))) => {
                DndFocus::leave(s, data, Some(o), seat)
            }
            (Self::Wl(s), None) => DndFocus::leave::<S>(s, data, None, seat),
            (Self::X11(s), Some(AuroraOfferData::X11(o))) => {
                DndFocus::leave(s, data, Some(o), seat)
            }
            (Self::X11(s), None) => DndFocus::leave::<S>(s, data, None, seat),
            _ => {}
        }
    }
    fn drop<S: Source>(
        &self,
        data: &mut Aurora,
        offer: Option<&mut AuroraOfferData<S>>,
        seat: &Seat<Aurora>,
    ) {
        match (self, offer) {
            (Self::Wl(s), Some(AuroraOfferData::Wayland(o))) => {
                DndFocus::drop(s, data, Some(o), seat)
            }
            (Self::Wl(s), None) => DndFocus::drop::<S>(s, data, None, seat),
            (Self::X11(s), Some(AuroraOfferData::X11(o))) => DndFocus::drop(s, data, Some(o), seat),
            (Self::X11(s), None) => DndFocus::drop::<S>(s, data, None, seat),
            _ => {}
        }
    }
}
