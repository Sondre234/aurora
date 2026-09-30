//! The bar as an `aurora_ui` app: one layer surface per output, IPC and clock sources.

use std::time::Duration;

use aurora_ipc::{
    Body, Event as IpcEvent, Request, Response, Snapshot, ThemeSnapshot, socket_path,
};
use aurora_theme::Theme;
use aurora_ui::runtime::calloop::generic::Generic;
use aurora_ui::runtime::calloop::timer::{TimeoutAction, Timer};
use aurora_ui::runtime::calloop::{Interest, Mode, PostAction};
use aurora_ui::runtime::{
    Anchor, App, Event, KeyboardInteractivity, Layer, LayerConfig, Output, Runtime, State,
    SurfaceId,
};
use aurora_ui::{Ui, UiEvent};

use crate::clock::{epoch_ms, format_clock, local_now, ms_until_next_minute};
use crate::ipc_client::{Backoff, Conn};
use crate::model::{OutputView, output_view};
use crate::view::{bar_height, build, ws_from_id};

/// What was last built for a bar, to skip rebuilds when nothing it shows changed.
type Key = (OutputView, String, u64);

struct BarSurface {
    output_id: u32,
    name: Option<String>,
    surface: SurfaceId,
    last: Option<Key>,
}

pub struct Bar {
    theme: Theme,
    /// Revision of the last applied `Event::Theme`, `None` until one arrives.
    theme_rev: Option<u64>,
    /// Bumped on every applied theme so bars rebuild.
    theme_gen: u64,
    snapshot: Snapshot,
    clock: String,
    bars: Vec<BarSurface>,
    conn: Option<Conn>,
    backoff: Backoff,
    applied_height: u32,
    warned_no_socket: bool,
    /// Set when the bar cannot work at all (no layer-shell); `main` exits nonzero.
    pub fatal: bool,
}

impl Bar {
    pub fn new() -> Self {
        let theme = Theme::default();
        Self {
            applied_height: bar_height(&theme),
            theme,
            theme_rev: None,
            theme_gen: 0,
            snapshot: Snapshot::default(),
            clock: format_clock(&local_now()),
            bars: Vec::new(),
            conn: None,
            backoff: Backoff::default(),
            warned_no_socket: false,
            fatal: false,
        }
    }

    pub fn bar_count(&self) -> usize {
        self.bars.len()
    }

    fn view_of(&self, name: &Option<String>) -> OutputView {
        name.as_deref()
            .map(|n| output_view(&self.snapshot, n))
            .unwrap_or_default()
    }

    /// Creates the bar of `o` unless it has one (then only refreshes its name).
    fn ensure_bar(&mut self, rt: &mut Runtime<Bar>, o: &Output) {
        if let Some(b) = self.bars.iter_mut().find(|b| b.output_id == o.id) {
            if b.name != o.name {
                b.name = o.name.clone();
                b.last = None;
            }
            return;
        }
        let h = bar_height(&self.theme);
        let cfg = LayerConfig {
            layer: Layer::Top,
            namespace: "aurora-bar".into(),
            anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            size: (0, h),
            exclusive_zone: h as i32,
            keyboard: KeyboardInteractivity::None,
            output: Some(o.clone()),
            ..LayerConfig::default()
        };
        let view = self.view_of(&o.name);
        let ui = Ui::new(rt.text().clone(), build(&view, &self.clock, &self.theme));
        match rt.create_layer(cfg, ui) {
            Ok(surface) => self.bars.push(BarSurface {
                output_id: o.id,
                name: o.name.clone(),
                surface,
                last: None,
            }),
            Err(e) => {
                tracing::error!("shell: cannot create a bar on {:?}: {e}", o.name);
                self.fatal = true;
                rt.quit();
            }
        }
    }

    fn remove_output(&mut self, rt: &mut Runtime<Bar>, id: u32) {
        if let Some(i) = self.bars.iter().position(|b| b.output_id == id) {
            let b = self.bars.remove(i);
            rt.destroy(b.surface);
        }
    }

    /// Rebuilds every bar whose content changed and applies a new bar height.
    fn refresh(&mut self, rt: &mut Runtime<Bar>) {
        let h = bar_height(&self.theme);
        if h != self.applied_height {
            self.applied_height = h;
            for b in &self.bars {
                rt.update_layer_config(b.surface, |c| {
                    c.size = (0, h);
                    c.exclusive_zone = h as i32;
                });
            }
        }
        for i in 0..self.bars.len() {
            let view = self.view_of(&self.bars[i].name);
            let key = (view, self.clock.clone(), self.theme_gen);
            if self.bars[i].last.as_ref() == Some(&key) {
                continue;
            }
            if let Some(ui) = rt.ui(self.bars[i].surface) {
                *ui.root_mut() = build(&key.0, &key.1, &self.theme);
            }
            self.bars[i].last = Some(key);
        }
    }

    fn set_theme(&mut self, ts: ThemeSnapshot) {
        if self.theme_rev.is_none_or(|r| ts.rev > r) {
            self.theme_rev = Some(ts.rev);
            self.theme = ts.theme;
            self.theme_gen += 1;
        }
    }

    fn on_frame(&mut self, body: Body) {
        match body {
            Body::Hello(h) => tracing::info!("shell: ipc connected server={}", h.client),
            Body::Event(IpcEvent::Theme(ts)) | Body::Response(Response::Theme(ts)) => {
                self.set_theme(ts);
            }
            Body::Event(e) => {
                if matches!(e, IpcEvent::Snapshot(_)) {
                    // Served a full state: the link is healthy, so restart the backoff.
                    self.backoff.reset();
                }
                self.snapshot.apply(&e);
            }
            Body::Error(e) => tracing::warn!("shell: ipc error {:?}: {}", e.code, e.message),
            _ => {}
        }
    }

    fn switch_workspace(&mut self, name: String, index: u32) {
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        let req = Request::SwitchWorkspace {
            output: Some(name),
            index,
        };
        if let Err(e) = conn.send(req) {
            tracing::warn!("shell: ipc send failed: {e}");
        }
    }
}

impl App for Bar {
    fn event(&mut self, rt: &mut Runtime<Self>, event: Event) {
        match event {
            Event::OutputAdded(o) | Event::OutputChanged(o) => {
                self.ensure_bar(rt, &o);
                self.refresh(rt);
            }
            Event::OutputRemoved(o) => self.remove_output(rt, o.id),
            Event::Closed { surface } => {
                self.bars.retain(|b| b.surface != surface);
                tracing::warn!("shell: the compositor closed a bar surface");
            }
            Event::Ui {
                surface,
                event: UiEvent::Clicked(id),
            } => {
                let name = self
                    .bars
                    .iter()
                    .find(|b| b.surface == surface)
                    .and_then(|b| b.name.clone());
                if let (Some(name), Some(index)) = (name, ws_from_id(id)) {
                    self.switch_workspace(name, index);
                }
            }
            _ => {}
        }
    }
}

// ---- event loop glue ----

/// First loop iteration: bars for the outputs already known, the `ready` line, the IPC
/// link and the clock.
pub fn start(state: &mut State<Bar>) {
    let outputs: Vec<Output> = state.rt.outputs().to_vec();
    for o in &outputs {
        state.app.ensure_bar(&mut state.rt, o);
    }
    state.app.refresh(&mut state.rt);
    tracing::info!("shell: ready outputs={}", state.app.bar_count());
    if !try_connect(state) {
        schedule_reconnect(state);
    }
    schedule_clock(state);
}

fn try_connect(state: &mut State<Bar>) -> bool {
    let Some(path) = socket_path() else {
        if !std::mem::replace(&mut state.app.warned_no_socket, true) {
            tracing::warn!("shell: no ipc socket path (XDG_RUNTIME_DIR unset); bar stays empty");
        }
        return false;
    };
    let conn = match Conn::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("shell: ipc connect {}: {e}", path.display());
            return false;
        }
    };
    let stream = match conn.stream() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("shell: ipc socket clone: {e}");
            return false;
        }
    };
    let source = Generic::new(stream, Interest::READ, Mode::Level);
    let handle = state.rt.loop_handle().clone();
    if let Err(e) = handle.insert_source(source, |_, _, state| Ok(on_readable(state))) {
        tracing::warn!("shell: cannot watch the ipc socket: {e}");
        return false;
    }
    state.app.conn = Some(conn);
    true
}

fn on_readable(state: &mut State<Bar>) -> PostAction {
    let Some(conn) = state.app.conn.as_mut() else {
        return PostAction::Remove;
    };
    match conn.read() {
        Ok(frames) => {
            for f in frames {
                state.app.on_frame(f.body);
            }
            state.app.refresh(&mut state.rt);
            PostAction::Continue
        }
        Err(e) => {
            tracing::warn!("shell: ipc connection lost: {e}; reconnecting");
            state.app.conn = None;
            state.app.snapshot = Snapshot::default();
            // A restarted compositor counts revisions from scratch again.
            state.app.theme_rev = None;
            state.app.refresh(&mut state.rt);
            schedule_reconnect(state);
            PostAction::Remove
        }
    }
}

fn schedule_reconnect(state: &mut State<Bar>) {
    let delay = state.app.backoff.next_delay();
    let handle = state.rt.loop_handle().clone();
    let inserted = handle.insert_source(Timer::from_duration(delay), |_, _, state| {
        if try_connect(state) {
            TimeoutAction::Drop
        } else {
            TimeoutAction::ToDuration(state.app.backoff.next_delay())
        }
    });
    if let Err(e) = inserted {
        tracing::error!("shell: cannot schedule an ipc reconnect: {e}");
    }
}

fn clock_delay() -> Duration {
    // A little past the boundary so the wall clock has certainly rolled over.
    Duration::from_millis(ms_until_next_minute(epoch_ms()) + 20)
}

fn schedule_clock(state: &mut State<Bar>) {
    let handle = state.rt.loop_handle().clone();
    let inserted = handle.insert_source(Timer::from_duration(clock_delay()), |_, _, state| {
        state.app.clock = format_clock(&local_now());
        state.app.refresh(&mut state.rt);
        TimeoutAction::ToDuration(clock_delay())
    });
    if let Err(e) = inserted {
        tracing::error!("shell: cannot schedule the clock: {e}");
    }
}
