//! Pure maths of the window animations: what a window looks like at one instant. No Smithay,
//! no `Wm`; the render path and `Wm::tick` both lean on it, and the tests live here.
use crate::anim::RectF;

/// Scale an opening window starts from and a closing one ends at, about its centre.
pub const POP_SCALE: f32 = 0.92;

/// How one window is drawn this frame, when that differs from "at `target`, fully opaque".
/// `drawn` is the content rectangle in logical coordinates, `target` where the Space (and so
/// input) has it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Visual {
    pub drawn: RectF,
    pub target: RectF,
    pub alpha: f32,
}

impl Visual {
    /// `None` when the window is drawn exactly as the Space has it, so the render path can
    /// take the untouched route.
    pub fn new(drawn: RectF, target: RectF, alpha: f32) -> Option<Self> {
        (drawn != target || alpha != 1.0).then_some(Self {
            drawn,
            target,
            alpha,
        })
    }

    /// Offset of the content origin and scale factors relative to the target, or `None` for a
    /// degenerate target that cannot be scaled.
    pub fn transform(&self) -> Option<((f32, f32), (f32, f32))> {
        if self.target.w < 1.0 || self.target.h < 1.0 {
            return None;
        }
        Some((
            (self.drawn.x - self.target.x, self.drawn.y - self.target.y),
            (self.drawn.w / self.target.w, self.drawn.h / self.target.h),
        ))
    }
}

/// `r` scaled by `s` about its centre.
pub fn scale_about_center(r: RectF, s: f32) -> RectF {
    let (w, h) = (r.w * s, r.h * s);
    RectF::new(r.x + (r.w - w) / 2.0, r.y + (r.h - h) / 2.0, w, h)
}

/// `r` moved by `dx` horizontally.
pub fn shifted(r: RectF, dx: f32) -> RectF {
    RectF::new(r.x + dx, r.y, r.w, r.h)
}

/// Scale and opacity of an opening window at eased progress `p` (0 = just mapped, 1 = done).
pub fn open_state(p: f32) -> (f32, f32) {
    let p = p.clamp(0.0, 1.0);
    (POP_SCALE + (1.0 - POP_SCALE) * p, p)
}

/// Scale and opacity of a closing window at eased progress `p` (0 = just closed, 1 = gone).
pub fn close_state(p: f32, start_alpha: f32) -> (f32, f32) {
    let p = p.clamp(0.0, 1.0);
    (1.0 - (1.0 - POP_SCALE) * p, start_alpha * (1.0 - p))
}

/// Horizontal offsets of the two workspaces of a slide at eased progress `e`, as
/// `(incoming, outgoing)`. `dir` is +1 when the new workspace comes in from the right.
pub fn slide_offsets(e: f32, dir: f32, width: f32) -> (f32, f32) {
    let e = e.clamp(0.0, 1.0);
    (dir * width * (1.0 - e), -dir * width * e)
}

/// Which way a switch from workspace `from` to `to` slides: +1 (new one enters from the
/// right) when the number goes up.
pub fn slide_direction(from: u32, to: u32) -> f32 {
    if to >= from { 1.0 } else { -1.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_state_is_not_a_visual() {
        let r = RectF::new(10.0, 20.0, 300.0, 200.0);
        assert!(Visual::new(r, r, 1.0).is_none());
        assert!(Visual::new(r, r, 0.5).is_some());
        assert!(Visual::new(shifted(r, 1.0), r, 1.0).is_some());
    }

    #[test]
    fn transform_reports_offset_and_scale() {
        let target = RectF::new(100.0, 100.0, 400.0, 200.0);
        let drawn = RectF::new(110.0, 90.0, 200.0, 100.0);
        let v = Visual::new(drawn, target, 1.0).unwrap();
        assert_eq!(v.transform(), Some(((10.0, -10.0), (0.5, 0.5))));
        let degenerate = Visual::new(drawn, RectF::new(0.0, 0.0, 0.0, 0.0), 1.0).unwrap();
        assert!(degenerate.transform().is_none());
    }

    #[test]
    fn scaling_keeps_the_centre() {
        let r = RectF::new(0.0, 0.0, 100.0, 50.0);
        let s = scale_about_center(r, 0.5);
        assert_eq!(s, RectF::new(25.0, 12.5, 50.0, 25.0));
        assert_eq!(scale_about_center(r, 1.0), r);
    }

    #[test]
    fn open_runs_from_pop_to_full() {
        assert_eq!(open_state(0.0), (POP_SCALE, 0.0));
        assert_eq!(open_state(1.0), (1.0, 1.0));
        assert_eq!(open_state(7.0), (1.0, 1.0));
        let (s, a) = open_state(0.5);
        assert!(s > POP_SCALE && s < 1.0 && (a - 0.5).abs() < 1e-6);
    }

    #[test]
    fn close_fades_from_the_starting_opacity() {
        assert_eq!(close_state(0.0, 0.8), (1.0, 0.8));
        let (s, a) = close_state(1.0, 0.8);
        assert!((s - POP_SCALE).abs() < 1e-6 && a == 0.0);
    }

    #[test]
    fn slide_halves_meet_the_ends() {
        assert_eq!(slide_offsets(0.0, 1.0, 1000.0), (1000.0, 0.0));
        assert_eq!(slide_offsets(1.0, 1.0, 1000.0), (0.0, -1000.0));
        assert_eq!(slide_offsets(0.0, -1.0, 500.0), (-500.0, 0.0));
        let (inc, out) = slide_offsets(0.25, 1.0, 100.0);
        assert!((inc - 75.0).abs() < 1e-4 && (out + 25.0).abs() < 1e-4);
    }

    #[test]
    fn direction_follows_number_order() {
        assert_eq!(slide_direction(2, 5), 1.0);
        assert_eq!(slide_direction(5, 2), -1.0);
    }
}
