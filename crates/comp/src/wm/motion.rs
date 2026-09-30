//! Window motion: `WinData.current` is the drawn rectangle, advanced toward `target` by
//! `Wm::tick`. With no `Motion` given, `current` equals `target` at once, which is what
//! happens whenever animations are disabled.
use std::time::Duration;

use aurora_layout::Rect;

use super::{WinData, Wm};
use crate::anim::{Curve, RectF};

/// How a retarget should move: started at `now`, over `dur`, along `curve`.
#[derive(Clone, Copy, Debug)]
pub struct Motion {
    pub now: Duration,
    pub dur: Duration,
    pub curve: Curve,
}

pub fn rect_f(r: Rect) -> RectF {
    RectF::new(r.x as f32, r.y as f32, r.w as f32, r.h as f32)
}

fn rect_i(r: RectF) -> Rect {
    Rect::new(
        r.x.round() as i32,
        r.y.round() as i32,
        r.w.round() as i32,
        r.h.round() as i32,
    )
}

impl WinData {
    /// Sets the layout rectangle. `None` snaps `current` to it; `Some` animates `current`
    /// from wherever it is now.
    pub fn set_target(&mut self, target: Rect, motion: Option<Motion>) {
        self.target = target;
        self.current = match motion {
            Some(m) => {
                self.motion.retarget(rect_f(target), m.now, m.dur, m.curve);
                rect_i(self.motion.value(m.now))
            }
            None => {
                self.motion.set(rect_f(target));
                target
            }
        };
    }

    /// Whether `current` is still on its way to `target` at the last tick.
    pub fn is_animating(&self, now: Duration) -> bool {
        self.motion.is_active(now)
    }
}

/// Frame clock of the window manager: what `tick` last sampled at and whether anything was
/// still moving then.
#[derive(Default)]
pub struct Clock {
    now: Duration,
    busy: bool,
}

impl Wm {
    /// Advances every animation to `now` and reports whether any is still running, so the
    /// backend knows to render another frame. Time never goes backwards: outputs render at
    /// slightly different moments and the later call wins.
    pub fn tick(&mut self, now: Duration) -> bool {
        let now = now.max(self.clock.now);
        self.clock.now = now;
        let mut busy = false;
        for win in self.windows.values_mut() {
            if win.motion.is_active(now) {
                busy = true;
                win.current = rect_i(win.motion.value(now));
            } else if win.current != win.target {
                win.current = win.target;
            }
        }
        if self.clock.busy && !busy {
            tracing::info!("anim: idle");
        }
        self.clock.busy = busy;
        busy
    }

    /// Time of the last `tick`, the moment new animations should start from.
    pub fn frame_time(&self) -> Duration {
        self.clock.now
    }
}
