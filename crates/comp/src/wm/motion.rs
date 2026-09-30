//! Window motion: `WinData.current` is the drawn rectangle, advanced toward `target` by
//! `Wm::tick`. With no `Motion` given, `current` equals `target` at once, which is what
//! happens whenever animations are disabled.
use std::{collections::HashMap, time::Duration};

use aurora_layout::Rect;
use smithay::output::Output;

use super::{
    Phase, WinData, Wm, ghost,
    visual::{self, Visual},
};
use crate::{
    Aurora,
    anim::{Animated, Curve, RectF},
    config::AnimKind,
};

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
    /// from wherever it is now. Returns whether a new animation started; asking for the
    /// target it already has (or is heading to) never restarts anything.
    pub fn set_target(&mut self, target: Rect, motion: Option<Motion>) -> bool {
        let same = self.motion.target() == rect_f(target);
        self.target = target;
        match motion {
            Some(_) if same => false,
            Some(m) => {
                self.motion.retarget(rect_f(target), m.now, m.dur, m.curve);
                self.current = rect_i(self.motion.value(m.now));
                self.motion.is_active(m.now)
            }
            None => {
                self.motion.set(rect_f(target));
                self.current = target;
                false
            }
        }
    }

    /// Starts the open animation (scale up and fade in) at `now`.
    pub fn start_open(&mut self, now: Duration, dur: Duration, curve: Curve) {
        self.open.set(0.0);
        self.open.retarget(1.0, now, dur, curve);
    }

    /// Whether `current` is still on its way to `target` at the last tick.
    pub fn is_animating(&self, now: Duration) -> bool {
        self.motion.is_active(now) || self.open.is_active(now) || self.opacity.is_active(now)
    }
}

/// Frame clock of the window manager: what `tick` last sampled at and whether anything was
/// still moving then.
#[derive(Default)]
pub struct Clock {
    now: Duration,
    busy: bool,
}

/// A workspace switch in flight on one output: the incoming workspace comes in from the
/// side `dir` points to while the outgoing one leaves the other way. Both stay mapped in the
/// Space until it ends so that both keep getting frame callbacks.
pub struct Slide {
    pub output: Output,
    pub from: u32,
    pub to: u32,
    pub dir: f32,
    /// Logical width of the output, how far a workspace travels.
    pub width: f32,
    pub anim: Animated<f32>,
}

impl Wm {
    /// Advances every animation to `now` and reports whether any is still running, so the
    /// backend knows to render another frame. Time never goes backwards: outputs render at
    /// slightly different moments and the later call wins.
    pub fn tick(&mut self, now: Duration) -> bool {
        let now = now.max(self.clock.now);
        self.clock.now = now;
        let mut busy = false;

        // Slides first: a finished one must not offset anything in this very frame.
        let before = self.slides.len();
        self.slides.retain(|s| s.anim.is_active(now));
        if self.slides.len() != before {
            self.slides_done = true;
        }
        let mut slide_dx: HashMap<u32, (f32, bool)> = HashMap::new();
        for s in &self.slides {
            busy = true;
            let (incoming, outgoing) = visual::slide_offsets(s.anim.value(now), s.dir, s.width);
            slide_dx.insert(s.to, (incoming, false));
            slide_dx.insert(s.from, (outgoing, true));
        }

        for win in self.windows.values_mut() {
            let moving = win.motion.is_active(now);
            busy |= moving | win.open.is_active(now) | win.opacity.is_active(now);
            let m = win.motion.value(now);
            if moving {
                win.current = rect_i(m);
            } else if win.current != win.target {
                win.current = win.target;
            }
            let (scale, fade) = visual::open_state(win.open.value(now));
            let (dx, outgoing) = slide_dx.get(&win.ws).copied().unwrap_or((0.0, false));
            let drawn = visual::shifted(visual::scale_about_center(m, scale), dx);
            let alpha = (fade * win.opacity.value(now)).clamp(0.0, 1.0);
            let deco = win.element.deco();
            deco.set_visual(Visual::new(drawn, rect_f(win.target), alpha));
            deco.set_inert(outgoing);
        }

        for output in &self.outputs {
            busy |= ghost::prune(output, now);
        }

        if self.clock.busy && !busy {
            tracing::info!("anim: idle");
        }
        self.clock.busy = busy;
        busy
    }

    /// Ends every running animation where it is headed: used when animations get disabled.
    /// Slides end too, but their outgoing workspace leaves the Space in `finish_slides`.
    pub fn snap_animations(&mut self) {
        for win in self.windows.values_mut() {
            win.set_target(win.target, None);
            win.open.set(1.0);
            let rest = win.opacity.target();
            win.opacity.set(rest);
        }
        if !self.slides.is_empty() {
            self.slides.clear();
            self.slides_done = true;
        }
        for output in &self.outputs {
            ghost::clear(output);
        }
        self.tick(self.clock.now);
        self.clock.busy = false;
    }

    /// Time of the last `tick`, the moment new animations should start from.
    pub fn frame_time(&self) -> Duration {
        self.clock.now
    }

    /// Whether `ws` is the outgoing half of a slide.
    pub fn sliding_out(&self, ws: u32) -> bool {
        self.slides.iter().any(|s| s.from == ws)
    }
}

impl Aurora {
    /// The animation clock now. Animations start here, `Wm::tick` samples them later.
    pub fn anim_now(&self) -> Duration {
        Duration::from(self.clock.now()).max(self.wm.frame_time())
    }

    /// How an animation of `kind` starting now should move, `None` to snap.
    pub fn motion_for(&self, kind: AnimKind) -> Option<Motion> {
        let spec = self.config.animations.spec(kind)?;
        Some(Motion {
            now: self.anim_now(),
            dur: spec.duration(),
            curve: spec.curve,
        })
    }

    /// Sets each window's opacity target from focus: the focused window (and any fullscreen
    /// one, which must stay opaque to be scanned out) is opaque, the rest take
    /// `inactive_opacity`.
    pub fn sync_opacity(&mut self) {
        let inactive = self.config.decoration.inactive_opacity.clamp(0.0, 1.0);
        let motion = self.motion_for(AnimKind::Fade);
        let focused = self.wm.focused;
        let mut changed = Vec::new();
        for win in self.wm.windows.values_mut() {
            let want = if Some(win.id) == focused || win.fs {
                1.0
            } else {
                inactive
            };
            if win.opacity.target() == want {
                continue;
            }
            match motion {
                Some(m) => win.opacity.retarget(want, m.now, m.dur, m.curve),
                None => win.opacity.set(want),
            }
            changed.push(win.ws);
        }
        for ws in changed {
            if let Some(output) = self.wm.output_for_ws(ws) {
                self.queue_redraw_output(&output);
            }
        }
    }

    /// Starts sliding `output` from workspace `from` to `to`. Call after the switch is
    /// recorded in `active_ws`, before `normalize` would hide the old workspace. A slide
    /// already running on the output ends at once.
    pub(super) fn start_slide(&mut self, output: &Output, from: u32, to: u32) {
        let Some(m) = self.motion_for(AnimKind::Workspace) else {
            return;
        };
        let Some(geo) = self.space.output_geometry(output) else {
            return;
        };
        let occupied = |wm: &Wm, ws: u32| {
            wm.windows
                .values()
                .any(|w| w.ws == ws && w.phase == Phase::Mapped)
        };
        // Two empty workspaces look the same either way.
        if !occupied(&self.wm, from) && !occupied(&self.wm, to) {
            return;
        }
        if self.wm.slides.iter().any(|s| &s.output == output) {
            self.wm.slides.retain(|s| &s.output != output);
            self.wm.slides_done = true;
        }
        let mut anim = Animated::new(0.0f32);
        anim.retarget(1.0, m.now, m.dur, m.curve);
        self.wm.slides.push(Slide {
            output: output.clone(),
            from,
            to,
            dir: visual::slide_direction(from, to),
            width: geo.size.w as f32,
            anim,
        });
        tracing::info!("anim: start kind=workspace win=- from={from} to={to}");
    }

    /// Takes the workspaces that slid out of the Space. Runs from the event loop, where the
    /// Space is at hand; the tick only notes that a slide ended.
    pub fn finish_slides(&mut self) {
        if !std::mem::take(&mut self.wm.slides_done) {
            return;
        }
        self.hide_invisible();
        self.sync_covering();
        let outputs = self.wm.outputs.clone();
        for output in outputs {
            self.queue_redraw_output(&output);
        }
    }
}
