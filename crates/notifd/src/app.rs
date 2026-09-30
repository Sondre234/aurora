//! The daemon proper: glues the state machine ([`Center`]) to the ui runtime.
//!
//! One layer-shell surface per visible notification (namespace `aurora-notify`, Overlay,
//! anchored top-right). Stack position and slide are layer margins, so moving a toast
//! commits the surface without repainting it; only a content change repaints.
//!
//! Idle behaviour (docs/performance.md): with no toast on screen there are no timers, no
//! frames and no wakeups besides the D-Bus executor thread and the IPC thread sleeping in
//! `epoll`/`read`. While toasts live there is exactly one expiry timer armed for the
//! earliest deadline, plus a 16 ms animation timer that exists only while a slide or
//! restack is in flight.

use std::time::{Duration, Instant};

use aurora_theme::Theme;
use aurora_ui::runtime::calloop::RegistrationToken;
use aurora_ui::runtime::calloop::timer::{TimeoutAction, Timer};
use aurora_ui::runtime::{
    Anchor, App, Event, Layer, LayerConfig, Output, Runtime, State, SurfaceId,
};
use aurora_ui::{Id, Image, Ui, UiEvent};

use crate::dbus::{Command, Server};
use crate::ipc_client::IpcMsg;
use crate::layout::{Curve, ToastMotion, stack_positions};
use crate::state::{Center, CloseReason, Config, Effect, Notification};
use crate::toast::{self, ToastStyle};

/// Everything the main loop reacts to besides Wayland.
#[derive(Debug)]
pub enum Msg {
    Dbus(Command),
    Ipc(IpcMsg),
}

/// Toast geometry in logical px.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub width: f32,
    /// Distance from the output's top and right edges.
    pub margin: f32,
    pub gap: f32,
    pub min_height: f32,
    pub max_height: f32,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            width: 360.0,
            margin: 12.0,
            gap: 8.0,
            min_height: 44.0,
            max_height: 320.0,
        }
    }
}

const FRAME: Duration = Duration::from_millis(16);

struct View {
    id: u32,
    surface: SurfaceId,
    revision: u64,
    height: f32,
    motion: ToastMotion,
    /// Keys of the action buttons, in button order.
    keys: Vec<String>,
    /// Margins last sent to the compositor.
    applied: (i32, i32),
}

pub struct Notifd {
    center: Center,
    views: Vec<View>,
    metrics: Metrics,
    theme: Theme,
    active_output: Option<String>,
    /// Output the current stack lives on; fixed while the stack is non-empty.
    stack_output: Option<Output>,
    pub server: Option<Server>,
    start: Instant,
    expiry: Option<RegistrationToken>,
    anim: Option<RegistrationToken>,
}

impl Notifd {
    pub fn new(cfg: Config, metrics: Metrics) -> Self {
        Self {
            center: Center::new(cfg),
            views: Vec::new(),
            metrics,
            theme: Theme::default(),
            active_output: None,
            stack_output: None,
            server: None,
            start: Instant::now(),
            expiry: None,
            anim: None,
        }
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn motion_params(&self) -> (u64, Curve) {
        (
            u64::from(self.theme.motion.duration_ms.min(1000)),
            Curve::parse(&self.theme.motion.curve),
        )
    }

    /// Entry point for D-Bus and IPC input, called from the loop's channel source.
    pub fn handle(&mut self, rt: &mut Runtime<Self>, msg: Msg) {
        let now = self.now();
        match msg {
            Msg::Dbus(Command::Notify(args)) => {
                self.center.notify(now, *args);
                self.apply(rt, Vec::new());
            }
            Msg::Dbus(Command::Close(id)) => {
                let fx = self.center.close(now, id, CloseReason::Closed);
                self.apply(rt, fx);
            }
            Msg::Dbus(Command::NameLost) => {
                tracing::warn!("notifd: lost org.freedesktop.Notifications to another daemon");
                rt.quit();
            }
            Msg::Ipc(IpcMsg::Theme(t)) => {
                if t.theme != self.theme {
                    tracing::info!("notifd: theme rev={}", t.rev);
                    self.theme = t.theme;
                    for i in 0..self.views.len() {
                        self.rebuild(rt, i);
                    }
                    self.apply(rt, Vec::new());
                }
            }
            Msg::Ipc(IpcMsg::ActiveOutput(o)) => self.active_output = o,
        }
    }

    /// Emits signals for `effects`, then brings views and timers in line with the state.
    fn apply(&mut self, rt: &mut Runtime<Self>, effects: Vec<Effect>) {
        for fx in effects {
            match fx {
                Effect::Closed { id, reason } => {
                    tracing::info!("notifd: closed id={id} reason={}", reason.code());
                    if let Some(s) = &self.server {
                        s.emit_closed(id, reason);
                    }
                }
                Effect::ActionInvoked { id, key } => {
                    tracing::info!("notifd: action id={id} key={key}");
                    if let Some(s) = &self.server {
                        s.emit_action(id, &key);
                    }
                }
            }
        }
        self.sync(rt);
        self.rearm_expiry(rt);
    }

    /// Views follow `center.visible()`: new ones get a surface, changed ones a rebuilt
    /// tree, vanished ones start their exit slide.
    fn sync(&mut self, rt: &mut Runtime<Self>) {
        let now = self.now();
        for v in &mut self.views {
            if self.center.get(v.id).is_none() {
                v.motion.leave(now);
            }
        }
        let wanted: Vec<(u32, u64)> = self
            .center
            .visible()
            .iter()
            .map(|n| (n.id, n.revision))
            .collect();
        for (id, rev) in wanted {
            match self
                .views
                .iter()
                .position(|v| v.id == id && !v.motion.leaving())
            {
                Some(i) if self.views[i].revision != rev => self.rebuild(rt, i),
                Some(_) => {}
                None => self.create(rt, id),
            }
        }
        self.restack(rt);
        self.ensure_anim(rt);
    }

    fn pick_output(&self, rt: &Runtime<Self>) -> Option<Output> {
        let want = self.active_output.as_deref()?;
        rt.outputs()
            .iter()
            .find(|o| o.name.as_deref() == Some(want))
            .cloned()
    }

    /// Builds the toast tree and measures it at the toast width.
    fn build_ui(&self, rt: &Runtime<Self>, n: &Notification) -> (Ui, f32, Option<Image>) {
        let st = ToastStyle::new(&self.theme, n.urgency);
        let image = crate::icon::resolve(&n.hints, &n.app_icon);
        let ui = Ui::new(
            rt.text().clone(),
            toast::build(n, &st, image.clone(), self.metrics.width),
        );
        let h = ui
            .natural_size(Some(self.metrics.width))
            .h
            .ceil()
            .clamp(self.metrics.min_height, self.metrics.max_height);
        (ui, h, image)
    }

    fn keys_of(n: &Notification) -> Vec<String> {
        n.actions
            .iter()
            .take(toast::MAX_ACTION_BUTTONS)
            .map(|a| a.key.clone())
            .collect()
    }

    fn create(&mut self, rt: &mut Runtime<Self>, id: u32) {
        let Some(n) = self.center.get(id).cloned() else {
            return;
        };
        let now = self.now();
        if self.views.is_empty() {
            self.stack_output = self.pick_output(rt);
        }
        let (ui, height, _) = self.build_ui(rt, &n);
        let heights: Vec<f32> = self
            .views
            .iter()
            .map(|v| v.height)
            .chain([height])
            .collect();
        let y = stack_positions(&heights, self.metrics.margin, self.metrics.gap)
            .last()
            .copied()
            .unwrap_or(self.metrics.margin);
        let (dur, curve) = self.motion_params();
        let motion = ToastMotion::enter(now, y, dur, curve);
        let margins = motion.margins(now, self.metrics.width, self.metrics.margin);
        let cfg = LayerConfig {
            layer: Layer::Overlay,
            namespace: "aurora-notify".into(),
            anchor: Anchor::TOP | Anchor::RIGHT,
            size: (self.metrics.width as u32, height as u32),
            exclusive_zone: 0,
            margin: (margins.0, margins.1, 0, 0),
            output: self.stack_output.clone(),
            ..LayerConfig::default()
        };
        match rt.create_layer(cfg, ui) {
            Ok(surface) => {
                tracing::info!("notifd: shown id={id}");
                self.views.push(View {
                    id,
                    surface,
                    revision: n.revision,
                    height,
                    motion,
                    keys: Self::keys_of(&n),
                    applied: margins,
                });
            }
            Err(e) => tracing::error!("notifd: cannot show id={id}: {e}"),
        }
    }

    /// Re-renders view `i` from the current notification and theme.
    fn rebuild(&mut self, rt: &mut Runtime<Self>, i: usize) {
        let v = &self.views[i];
        if v.motion.leaving() {
            return;
        }
        let Some(n) = self.center.get(v.id).cloned() else {
            return;
        };
        let (ui, height, _) = self.build_ui(rt, &n);
        let (surface, width) = (v.surface, self.metrics.width as u32);
        let mut ui = ui;
        if let Some(live) = rt.ui(surface) {
            // Swap the tree in place so the surface keeps its buffers and scale state.
            std::mem::swap(live.root_mut(), ui.root_mut());
        }
        rt.update_layer_config(surface, |c| c.size = (width, height as u32));
        let v = &mut self.views[i];
        v.revision = n.revision;
        v.height = height;
        v.keys = Self::keys_of(&n);
    }

    /// Recomputes every toast's stack offset and pushes changed margins to the compositor.
    fn restack(&mut self, rt: &mut Runtime<Self>) {
        let now = self.now();
        let heights: Vec<f32> = self.views.iter().map(|v| v.height).collect();
        let ys = stack_positions(&heights, self.metrics.margin, self.metrics.gap);
        for (v, y) in self.views.iter_mut().zip(ys) {
            v.motion.move_to(now, y);
        }
        self.push_margins(rt, now);
    }

    fn push_margins(&mut self, rt: &mut Runtime<Self>, now: u64) {
        let (w, m) = (self.metrics.width, self.metrics.margin);
        for v in &mut self.views {
            let margins = v.motion.margins(now, w, m);
            if margins != v.applied {
                v.applied = margins;
                rt.update_layer_config(v.surface, |c| c.margin = (margins.0, margins.1, 0, 0));
            }
        }
    }

    fn animating(&self, now: u64) -> bool {
        self.views
            .iter()
            .any(|v| v.motion.active(now) || v.motion.gone(now))
    }

    /// One animation frame: advance margins, drop finished exits, relayout the rest.
    /// Returns whether another frame is needed.
    fn tick(&mut self, rt: &mut Runtime<Self>) -> bool {
        let now = self.now();
        self.push_margins(rt, now);
        let before = self.views.len();
        let mut gone = Vec::new();
        self.views.retain(|v| {
            if v.motion.gone(now) {
                gone.push(v.surface);
                false
            } else {
                true
            }
        });
        for s in gone {
            rt.destroy(s);
        }
        if self.views.len() != before {
            if self.views.is_empty() {
                self.stack_output = None;
            }
            self.restack(rt);
        }
        self.animating(self.now())
    }

    fn ensure_anim(&mut self, rt: &mut Runtime<Self>) {
        if self.anim.is_some() || !self.animating(self.now()) {
            return;
        }
        let token = rt.loop_handle().insert_source(
            Timer::from_duration(FRAME),
            |_, _, state: &mut State<Notifd>| {
                if state.app.tick(&mut state.rt) {
                    TimeoutAction::ToDuration(FRAME)
                } else {
                    state.app.anim = None;
                    TimeoutAction::Drop
                }
            },
        );
        match token {
            Ok(t) => self.anim = Some(t),
            Err(e) => tracing::error!("notifd: cannot arm animation timer: {e}"),
        }
    }

    /// One timer for the earliest expiry; none when nothing can expire.
    fn rearm_expiry(&mut self, rt: &mut Runtime<Self>) {
        if let Some(t) = self.expiry.take() {
            rt.loop_handle().remove(t);
        }
        let Some(deadline) = self.center.next_deadline() else {
            return;
        };
        let at = self.start + Duration::from_millis(deadline);
        let token = rt.loop_handle().insert_source(
            Timer::from_deadline(at),
            |_, _, state: &mut State<Notifd>| {
                let app = &mut state.app;
                app.expiry = None;
                let fx = app.center.expire(app.now());
                app.apply(&mut state.rt, fx);
                TimeoutAction::Drop
            },
        );
        match token {
            Ok(t) => self.expiry = Some(t),
            Err(e) => tracing::error!("notifd: cannot arm expiry timer: {e}"),
        }
    }

    fn clicked(&mut self, rt: &mut Runtime<Self>, surface: SurfaceId, widget: Id) {
        let Some(v) = self
            .views
            .iter()
            .find(|v| v.surface == surface && !v.motion.leaving())
        else {
            return;
        };
        let (id, now) = (v.id, self.now());
        let fx = match widget.0 {
            toast::ROOT => self.center.activate(now, id),
            toast::CLOSE => self.center.close(now, id, CloseReason::Dismissed),
            w if w >= toast::ACTION_BASE => {
                let Some(key) = v.keys.get((w - toast::ACTION_BASE) as usize).cloned() else {
                    return;
                };
                self.center.invoke(now, id, &key)
            }
            _ => return,
        };
        self.apply(rt, fx);
    }
}

impl App for Notifd {
    fn event(&mut self, rt: &mut Runtime<Self>, event: Event) {
        match event {
            Event::Ui {
                surface,
                event: UiEvent::Clicked(widget),
            } => self.clicked(rt, surface, widget),
            Event::Closed { surface } => {
                // The compositor took the surface away (output unplugged, ...). The
                // notification stays; the next sync maps it again.
                self.views.retain(|v| v.surface != surface);
                self.stack_output = None;
                self.sync(rt);
            }
            Event::OutputRemoved(o) if self.stack_output.as_ref() == Some(&o) => {
                self.stack_output = None;
            }
            _ => {}
        }
    }
}
