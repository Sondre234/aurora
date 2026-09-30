//! Introspection for QA runs: the SIGUSR2 state dump and the `--qa` input actions. The
//! dump is a fixed line format (see docs/m2-plan.md) that scripts grep.
use std::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use smithay::{
    backend::input::{AxisSource, ButtonState, InputTime},
    desktop::{WindowSurface, layer_map_for_output},
    reexports::wayland_server::Resource,
};

use crate::{
    action::DebugPointer,
    config::keybind::WheelDir,
    focus::FocusTarget,
    input::pointer::{AxisInput, AxisValue},
    state::Aurora,
    wm::apply::read_strings,
};

fn quote(s: &str) -> String {
    format!("{s:?}")
}

impl Aurora {
    pub fn dump_state(&mut self) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!("dump: begin {seq}");

        let mods = self.keyboard.modifier_state();
        let names: Vec<_> = [
            (mods.shift, "shift"),
            (mods.ctrl, "ctrl"),
            (mods.alt, "alt"),
            (mods.logo, "logo"),
            (mods.iso_level3_shift, "altgr"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, n)| *n)
        .collect();
        let mut pressed: Vec<u32> = self
            .keyboard
            .pressed_keys()
            .iter()
            .map(|k| k.raw())
            .collect();
        pressed.sort_unstable();
        let mut suppressed: Vec<u32> = self.suppressed_keys.iter().map(|k| k.raw()).collect();
        suppressed.sort_unstable();
        let repeat = self.input.repeat.as_ref().map(|r| r.keycode().raw());
        tracing::info!(
            "dump: mods={} pressed={pressed:?} suppressed={suppressed:?} binds={} repeat={repeat:?}",
            names.join("+"),
            self.config.binds.len(),
        );

        let outputs = self.wm.outputs.clone();
        for output in &outputs {
            let geo = self.space.output_geometry(output);
            let usable = self.work_area(output).map(|(w, _)| w);
            tracing::info!(
                "dump: out {} geo={} scale={} ws={} usable={}",
                output.name(),
                geo.map_or("none".into(), |g| format!(
                    "{},{} {}x{}",
                    g.loc.x, g.loc.y, g.size.w, g.size.h
                )),
                output.current_scale().fractional_scale(),
                self.wm.active_ws.get(output).copied().unwrap_or(0),
                usable.map_or("none".into(), |r| format!(
                    "{},{} {}x{}",
                    r.x, r.y, r.w, r.h
                )),
            );
        }

        let mut ws_ids: Vec<u32> = self.wm.workspaces.keys().copied().collect();
        ws_ids.sort_unstable();
        for ws in ws_ids {
            let out = self
                .wm
                .output_for_ws(ws)
                .map_or_else(|| "none".to_string(), |o| o.name());
            let mut ids: Vec<u64> = self
                .wm
                .windows
                .values()
                .filter(|w| w.ws == ws)
                .map(|w| w.id.0)
                .collect();
            ids.sort_unstable();
            let focus = self
                .wm
                .workspaces
                .get(&ws)
                .and_then(|w| w.mru().first().copied())
                .map(|id| id.0);
            tracing::info!(
                "dump: ws {ws} out={out} layout=dwindle windows={ids:?} focus={focus:?}"
            );
        }

        let mut wins: Vec<_> = self.wm.windows.values().collect();
        wins.sort_by_key(|w| w.id.0);
        for win in wins {
            let (kind, title) = match win.element.underlying_surface() {
                WindowSurface::Wayland(t) => ("wl", read_strings(t.wl_surface()).1),
                WindowSurface::X11(x) => (
                    if x.is_override_redirect() {
                        "or"
                    } else {
                        "x11"
                    },
                    x.title(),
                ),
            };
            let mut line = String::new();
            let r = win.target;
            let c = win.current;
            let anim = win.is_animating(self.wm.frame_time());
            let _ = write!(
                line,
                "dump: win {} app_id={} title={} kind={kind} ws={} rect={},{} {}x{} current={},{} {}x{} anim={} float={} fs={} frames_sent={} urgent={} mapped={}",
                win.id.0,
                quote(&win.app_id),
                quote(&title),
                win.ws,
                r.x,
                r.y,
                r.w,
                r.h,
                c.x,
                c.y,
                c.w,
                c.h,
                anim as u8,
                win.floating as u8,
                win.fs as u8,
                win.frames_sent,
                win.urgent as u8,
                (self.space.element_location(&win.element).is_some()) as u8,
            );
            if let Some(x) = win.element.x11_surface() {
                let _ = write!(line, " xid={}", x.window_id());
            }
            tracing::info!("{line}");
        }

        // Override-redirect windows are not in Wm; their rect is the position the client chose.
        for window in self.xwayland.unmanaged.elements() {
            let Some(x) = window.x11_surface() else {
                continue;
            };
            let r = self
                .xwayland
                .unmanaged
                .element_geometry(window)
                .unwrap_or_default();
            tracing::info!(
                "dump: win x{} app_id={} title={} kind=or ws=0 rect={},{} {}x{} float=0 fs=0 frames_sent=0 mapped=1 xid={}",
                x.window_id(),
                quote(&x.class()),
                quote(&x.title()),
                r.loc.x,
                r.loc.y,
                r.size.w,
                r.size.h,
                x.window_id(),
            );
        }

        for output in &outputs {
            let map = layer_map_for_output(output);
            for layer in map.layers() {
                let geo = map.layer_geometry(layer);
                let state = layer.cached_state();
                tracing::info!(
                    "dump: layer {} ns={} layer={:?} kbd={:?} excl={:?} exclusive_focus={} geo={}",
                    output.name(),
                    quote(layer.namespace()),
                    layer.layer(),
                    state.keyboard_interactivity,
                    state.exclusive_zone,
                    (self.layer_focus.exclusive.as_ref() == Some(layer)) as u8,
                    geo.map_or("none".into(), |g| format!(
                        "{},{} {}x{}",
                        g.loc.x, g.loc.y, g.size.w, g.size.h
                    )),
                );
            }
        }

        let describe = |target: Option<FocusTarget>| match target {
            None => "none".to_string(),
            Some(FocusTarget::Wl(s)) => match self.wm.id_of(&s) {
                Some(id) => id.0.to_string(),
                None => self.layer_of(&s).map_or_else(
                    || format!("surface:{}", s.id()),
                    |(_, layer)| format!("layer:{}", layer.namespace()),
                ),
            },
            Some(FocusTarget::X11(x)) => format!("x11:{}", x.window_id()),
        };
        let kbd = describe(self.keyboard.current_focus());
        let ptr = describe(self.pointer.current_focus());
        let grab = self.wm.drag.unwrap_or("none");
        tracing::info!("dump: focus kbd={kbd} pointer={ptr} grab={grab}");
        self.dump_overview();
        self.dump_lock();
        self.dump_services();
        self.dump_ipc();
        tracing::info!("dump: end {seq}");
    }

    fn now_input(&self) -> InputTime {
        InputTime::from_millis(Duration::from(self.clock.now()).as_millis() as u32)
    }

    /// `--qa` pointer injection: the same primitives as a real device, honouring mouse
    /// binds with the current keyboard modifiers.
    pub fn debug_pointer(&mut self, action: DebugPointer) {
        let time = self.now_input();
        match action {
            DebugPointer::Move(x, y) => {
                self.on_pointer_motion_absolute((x as f64, y as f64).into(), time)
            }
            DebugPointer::MoveBy(dx, dy) => {
                let delta = (dx as f64, dy as f64).into();
                self.on_pointer_motion_relative(delta, delta, time);
            }
            DebugPointer::Press(b) => {
                self.on_pointer_button(b.code(), ButtonState::Pressed, time, true)
            }
            DebugPointer::Release(b) => {
                self.on_pointer_button(b.code(), ButtonState::Released, time, true)
            }
            DebugPointer::Scroll(dir) => {
                let v120 = match dir {
                    WheelDir::Up => -120.0,
                    WheelDir::Down => 120.0,
                };
                self.on_axis(AxisInput {
                    time,
                    source: AxisSource::Wheel,
                    horizontal: AxisValue::default(),
                    vertical: AxisValue {
                        amount: None,
                        v120: Some(v120),
                    },
                });
            }
        }
    }
}
