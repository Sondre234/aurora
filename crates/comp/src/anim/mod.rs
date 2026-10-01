//! Pure animation engine: curves, animated values and a timeline. No Smithay types, so all of
//! it is unit tested. Time is a `Duration` on one monotonic clock (the presentation clock in
//! DRM, `state.clock` elsewhere); nothing here reads a clock itself.

mod animated;
mod curve;

pub use animated::{Animated, RectF};
pub use curve::Curve;
