//! Toast stack layout and time-based animation. Pure: positions and tweens are functions
//! of `now` (ms), so nothing ticks unless the caller asks for a frame, and
//! [`ToastMotion::active`] tells it when to stop asking.
//!
//! Each toast is its own layer surface anchored top-right. Its place is expressed as
//! layer margins: `top` is the animated stack offset, `right` slides the toast in from
//! off-screen and back out again.

/// Easing, the subset of the theme's curve names a slide needs. Unknown curves (`spring`,
/// `bezier ...`) fall back to `ease-out`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Curve {
    Linear,
    EaseOut,
    EaseInOut,
}

impl Curve {
    pub fn parse(text: &str) -> Self {
        match text.trim() {
            "linear" => Curve::Linear,
            "ease-in-out" => Curve::EaseInOut,
            _ => Curve::EaseOut,
        }
    }

    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Curve::Linear => t,
            Curve::EaseOut => 1.0 - (1.0 - t).powi(3),
            Curve::EaseInOut => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
        }
    }
}

/// A value moving from `from` to `to` over `[start, start + dur]` ms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tween {
    from: f32,
    to: f32,
    start: u64,
    dur: u64,
    curve: Curve,
}

impl Tween {
    pub fn new(from: f32, to: f32, start: u64, dur: u64, curve: Curve) -> Self {
        Self {
            from,
            to,
            start,
            dur,
            curve,
        }
    }

    /// A tween that is already at `v`.
    pub fn at(v: f32) -> Self {
        Self::new(v, v, 0, 0, Curve::Linear)
    }

    pub fn value(&self, now: u64) -> f32 {
        if self.dur == 0 || now >= self.start + self.dur {
            return self.to;
        }
        let t = now.saturating_sub(self.start) as f32 / self.dur as f32;
        self.from + (self.to - self.from) * self.curve.apply(t)
    }

    pub fn target(&self) -> f32 {
        self.to
    }

    pub fn done(&self, now: u64) -> bool {
        self.dur == 0 || now >= self.start + self.dur
    }
}

/// Y positions (top margins) of a stack, top to bottom, from the top margin and gap.
pub fn stack_positions(heights: &[f32], top: f32, gap: f32) -> Vec<f32> {
    let mut y = top;
    heights
        .iter()
        .map(|h| {
            let at = y;
            y += h + gap;
            at
        })
        .collect()
}

/// Right margin of a toast for slide progress `p` (0 = fully off-screen, 1 = in place).
pub fn right_margin(p: f32, width: f32, margin: f32) -> f32 {
    margin - (1.0 - p.clamp(0.0, 1.0)) * (width + margin)
}

/// Motion of one toast: its animated stack offset and its slide progress.
#[derive(Clone, Copy, Debug)]
pub struct ToastMotion {
    y: Tween,
    slide: Tween,
    dur: u64,
    curve: Curve,
    leaving: bool,
}

impl ToastMotion {
    /// A toast appearing at stack offset `y`, sliding in.
    pub fn enter(now: u64, y: f32, dur: u64, curve: Curve) -> Self {
        Self {
            y: Tween::at(y),
            slide: Tween::new(0.0, 1.0, now, dur, curve),
            dur,
            curve,
            leaving: false,
        }
    }

    /// Glide to a new stack offset (no-op when already heading there).
    pub fn move_to(&mut self, now: u64, y: f32) {
        if (self.y.target() - y).abs() > 0.01 {
            self.y = Tween::new(self.y.value(now), y, now, self.dur, self.curve);
        }
    }

    /// Slide out from wherever the slide currently is.
    pub fn leave(&mut self, now: u64) {
        if !self.leaving {
            self.leaving = true;
            self.slide = Tween::new(self.slide.value(now), 0.0, now, self.dur, self.curve);
        }
    }

    pub fn leaving(&self) -> bool {
        self.leaving
    }

    pub fn top(&self, now: u64) -> f32 {
        self.y.value(now)
    }

    pub fn progress(&self, now: u64) -> f32 {
        self.slide.value(now)
    }

    /// Anything still moving (the caller keeps a frame timer only while true).
    pub fn active(&self, now: u64) -> bool {
        !self.y.done(now) || !self.slide.done(now)
    }

    /// The exit slide finished: the surface can go.
    pub fn gone(&self, now: u64) -> bool {
        self.leaving && self.slide.done(now)
    }

    /// Layer margins `(top, right)` in whole pixels.
    pub fn margins(&self, now: u64, width: f32, margin: f32) -> (i32, i32) {
        (
            self.top(now).round() as i32,
            right_margin(self.progress(now), width, margin).round() as i32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacking_uses_heights_and_gaps() {
        assert_eq!(stack_positions(&[], 10.0, 8.0), Vec::<f32>::new());
        assert_eq!(
            stack_positions(&[50.0, 70.0, 30.0], 10.0, 8.0),
            vec![10.0, 68.0, 146.0]
        );
    }

    #[test]
    fn curves_are_anchored() {
        for c in [Curve::Linear, Curve::EaseOut, Curve::EaseInOut] {
            assert_eq!(c.apply(0.0), 0.0);
            assert!((c.apply(1.0) - 1.0).abs() < 1e-6);
            assert_eq!(c.apply(-3.0), 0.0);
            assert!((c.apply(9.0) - 1.0).abs() < 1e-6);
        }
        assert!(Curve::EaseOut.apply(0.5) > 0.5);
        assert_eq!(Curve::parse("linear"), Curve::Linear);
        assert_eq!(Curve::parse(" ease-in-out "), Curve::EaseInOut);
        assert_eq!(Curve::parse("spring 0.7"), Curve::EaseOut);
    }

    #[test]
    fn tween_runs_then_rests() {
        let t = Tween::new(0.0, 100.0, 1000, 200, Curve::Linear);
        assert_eq!(t.value(900), 0.0);
        assert_eq!(t.value(1100), 50.0);
        assert_eq!(t.value(1200), 100.0);
        assert!(!t.done(1199) && t.done(1200));
        let snap = Tween::new(0.0, 5.0, 0, 0, Curve::EaseOut);
        assert_eq!(snap.value(0), 5.0);
        assert!(snap.done(0));
    }

    #[test]
    fn slide_margin_runs_from_offscreen_to_resting() {
        assert_eq!(right_margin(1.0, 300.0, 12.0), 12.0);
        assert_eq!(right_margin(0.0, 300.0, 12.0), -300.0);
        assert!(right_margin(0.5, 300.0, 12.0) < 12.0);
    }

    #[test]
    fn enter_then_idle() {
        let m = ToastMotion::enter(0, 12.0, 200, Curve::Linear);
        assert!(m.active(0) && m.active(199));
        assert!(!m.active(200));
        assert_eq!(m.margins(0, 300.0, 12.0), (12, -300));
        assert_eq!(m.margins(200, 300.0, 12.0), (12, 12));
        assert!(!m.gone(10_000));
    }

    #[test]
    fn restack_glides_from_the_current_position() {
        let mut m = ToastMotion::enter(0, 100.0, 0, Curve::Linear);
        assert!(!m.active(0));
        // Nothing to do for the same target.
        m.move_to(10, 100.0);
        assert!(!m.active(10));
        m.dur = 100;
        m.move_to(10, 50.0);
        assert!(m.active(10));
        assert_eq!(m.top(10), 100.0);
        assert_eq!(m.top(1000), 50.0);
    }

    #[test]
    fn leave_slides_out_once_then_is_gone() {
        let mut m = ToastMotion::enter(0, 12.0, 100, Curve::Linear);
        m.leave(50); // interrupted mid-enter at progress 0.5
        assert!((m.progress(50) - 0.5).abs() < 1e-6);
        assert!(m.leaving() && !m.gone(60));
        assert!(m.gone(150));
        // A second leave does not restart the slide.
        m.leave(140);
        assert!(m.gone(150));
        assert_eq!(m.progress(150), 0.0);
    }

    #[test]
    fn zero_duration_snaps() {
        let mut m = ToastMotion::enter(0, 12.0, 0, Curve::EaseOut);
        assert!(!m.active(0));
        assert_eq!(m.progress(0), 1.0);
        m.leave(5);
        assert!(m.gone(5));
    }
}
