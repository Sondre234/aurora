//! Values that move toward a target over time, and the timeline that drives them.
use std::time::Duration;

use super::Curve;

pub trait Lerp: Copy {
    fn lerp(self, to: Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(self, to: f32, t: f32) -> f32 {
        self + (to - self) * t
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PointF {
    pub x: f32,
    pub y: f32,
}

impl PointF {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

impl Lerp for PointF {
    fn lerp(self, to: Self, t: f32) -> Self {
        Self::new(self.x.lerp(to.x, t), self.y.lerp(to.y, t))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectF {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

impl Lerp for RectF {
    fn lerp(self, to: Self, t: f32) -> Self {
        Self::new(
            self.x.lerp(to.x, t),
            self.y.lerp(to.y, t),
            self.w.lerp(to.w, t),
            self.h.lerp(to.h, t),
        )
    }
}

/// A value that is at rest or moving from `from` to `to` between `start` and `start + dur`.
#[derive(Clone, Copy, Debug)]
pub struct Animated<T: Lerp> {
    from: T,
    to: T,
    start: Duration,
    dur: Duration,
    curve: Curve,
}

impl<T: Lerp + PartialEq> Animated<T> {
    /// At rest at `value`.
    pub fn new(value: T) -> Self {
        Self {
            from: value,
            to: value,
            start: Duration::ZERO,
            dur: Duration::ZERO,
            curve: Curve::Linear,
        }
    }

    /// Jumps to `value`, cancelling any motion.
    pub fn set(&mut self, value: T) {
        *self = Self::new(value);
    }

    /// Starts moving from wherever the value is at `now` toward `to`, so a retarget in flight
    /// has no jump. A zero duration snaps; the same target as the running one is ignored.
    pub fn retarget(&mut self, to: T, now: Duration, dur: Duration, curve: Curve) {
        if to == self.to && self.is_active(now) {
            return;
        }
        if dur.is_zero() {
            self.set(to);
            return;
        }
        *self = Self {
            from: self.value(now),
            to,
            start: now,
            dur,
            curve,
        };
    }

    pub fn target(&self) -> T {
        self.to
    }

    pub fn is_active(&self, now: Duration) -> bool {
        now < self.start + self.dur
    }

    /// Normalized progress in 0..=1 (curve not applied).
    pub fn progress(&self, now: Duration) -> f32 {
        if self.dur.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_sub(self.start);
        (elapsed.as_secs_f32() / self.dur.as_secs_f32()).clamp(0.0, 1.0)
    }

    pub fn value(&self, now: Duration) -> T {
        if !self.is_active(now) {
            return self.to;
        }
        self.from
            .lerp(self.to, self.curve.value(self.progress(now)))
    }
}

/// One frame's view of time. `begin` at the start of a frame, read animated values through
/// `sample` (which notes whether any is still moving), and ask `busy` at the end.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timeline {
    now: Duration,
    busy: bool,
}

impl Timeline {
    pub fn begin(&mut self, now: Duration) {
        self.now = now;
        self.busy = false;
    }

    pub fn now(&self) -> Duration {
        self.now
    }

    pub fn sample<T: Lerp + PartialEq>(&mut self, a: &Animated<T>) -> T {
        self.busy |= a.is_active(self.now);
        a.value(self.now)
    }

    /// Whether any sampled value is still moving, so another frame is needed.
    pub fn busy(&self) -> bool {
        self.busy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn at_rest_is_inactive() {
        let a = Animated::new(3.0f32);
        assert!(!a.is_active(ms(0)));
        assert_eq!(a.value(ms(500)), 3.0);
    }

    #[test]
    fn linear_halfway_and_end() {
        let mut a = Animated::new(0.0f32);
        a.retarget(10.0, ms(100), ms(200), Curve::Linear);
        assert!(a.is_active(ms(100)));
        assert_eq!(a.value(ms(100)), 0.0);
        assert!((a.value(ms(200)) - 5.0).abs() < 1e-4);
        assert!(!a.is_active(ms(300)));
        assert_eq!(a.value(ms(300)), 10.0);
    }

    #[test]
    fn retarget_continues_from_current_value() {
        let mut a = Animated::new(0.0f32);
        a.retarget(10.0, ms(0), ms(100), Curve::Linear);
        a.retarget(0.0, ms(50), ms(100), Curve::Linear);
        assert!((a.value(ms(50)) - 5.0).abs() < 1e-4);
        assert!((a.value(ms(100)) - 2.5).abs() < 1e-4);
        assert_eq!(a.value(ms(150)), 0.0);
    }

    #[test]
    fn same_target_in_flight_keeps_the_running_animation() {
        let mut a = Animated::new(0.0f32);
        a.retarget(10.0, ms(0), ms(100), Curve::Linear);
        a.retarget(10.0, ms(50), ms(100), Curve::Linear);
        assert_eq!(a.value(ms(100)), 10.0);
        assert!(!a.is_active(ms(100)));
    }

    #[test]
    fn zero_duration_snaps() {
        let mut a = Animated::new(PointF::new(1.0, 2.0));
        a.retarget(
            PointF::new(5.0, 6.0),
            ms(10),
            Duration::ZERO,
            Curve::EASE_OUT,
        );
        assert!(!a.is_active(ms(10)));
        assert_eq!(a.value(ms(10)), PointF::new(5.0, 6.0));
    }

    #[test]
    fn rect_interpolates_every_field() {
        let mut a = Animated::new(RectF::new(0.0, 0.0, 100.0, 100.0));
        a.retarget(
            RectF::new(10.0, 20.0, 200.0, 300.0),
            ms(0),
            ms(100),
            Curve::Linear,
        );
        let r = a.value(ms(50));
        assert!((r.x - 5.0).abs() < 1e-4 && (r.y - 10.0).abs() < 1e-4);
        assert!((r.w - 150.0).abs() < 1e-4 && (r.h - 200.0).abs() < 1e-4);
    }

    #[test]
    fn timeline_reports_busy_only_while_moving() {
        let mut a = Animated::new(0.0f32);
        let rest = Animated::new(1.0f32);
        a.retarget(1.0, ms(0), ms(100), Curve::Linear);
        let mut tl = Timeline::default();
        tl.begin(ms(50));
        tl.sample(&rest);
        assert!(!tl.busy());
        tl.sample(&a);
        assert!(tl.busy());
        tl.begin(ms(100));
        tl.sample(&a);
        assert!(!tl.busy());
    }
}
