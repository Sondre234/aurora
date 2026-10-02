//! QA client for the display protocols, driven by `scripts/qa-nested.sh display` against a
//! nested Aurora. Not shipped; prints one parsable line per fact.
//!
//!   qa_display heads                        every head: name, enabled, mode, pos, scale
//!   qa_display place NAME X Y SCALE         apply: NAME at X,Y with SCALE, the rest as is
//!   qa_display test-scale NAME SCALE        test only
//!   qa_display stale                        apply with an outdated serial
//!   qa_display power NAME on|off            wlr-output-power set_mode, prints mode events
//!   qa_display gamma NAME                   gamma control: size or failed
//!   qa_display tearing-twice                second tearing control on one surface
use std::{collections::HashMap, time::Duration};

use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    protocol::{wl_compositor, wl_output, wl_registry, wl_surface},
};
use wayland_protocols::wp::tearing_control::v1::client::{
    wp_tearing_control_manager_v1, wp_tearing_control_v1,
};
use wayland_protocols_wlr::{
    gamma_control::v1::client::{zwlr_gamma_control_manager_v1, zwlr_gamma_control_v1},
    output_management::v1::client::{
        zwlr_output_configuration_head_v1, zwlr_output_configuration_v1, zwlr_output_head_v1,
        zwlr_output_manager_v1, zwlr_output_mode_v1,
    },
    output_power_management::v1::client::{zwlr_output_power_manager_v1, zwlr_output_power_v1},
};

#[derive(Default, Debug, Clone)]
struct Mode {
    size: (i32, i32),
    refresh: i32,
}

#[derive(Default)]
struct Head {
    name: String,
    enabled: bool,
    modes: Vec<zwlr_output_mode_v1::ZwlrOutputModeV1>,
    current: Option<zwlr_output_mode_v1::ZwlrOutputModeV1>,
    pos: (i32, i32),
    scale: f64,
    transform: i32,
    adaptive_sync: Option<u32>,
}

#[derive(Default)]
struct State {
    globals: HashMap<String, (u32, u32)>,
    heads: Vec<(zwlr_output_head_v1::ZwlrOutputHeadV1, Head)>,
    modes: HashMap<u32, Mode>,
    serial: Option<u32>,
    result: Option<&'static str>,
    outputs: Vec<(wl_output::WlOutput, String)>,
    power_modes: Vec<u32>,
    power_failed: bool,
    gamma_size: Option<u32>,
    gamma_failed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.insert(interface, (name, version));
        }
    }
}

impl Dispatch<zwlr_output_manager_v1::ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_output_manager_v1::ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => {
                state.heads.push((head, Head::default()));
            }
            zwlr_output_manager_v1::Event::Done { serial } => state.serial = Some(serial),
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, zwlr_output_manager_v1::ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (zwlr_output_head_v1::ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<zwlr_output_head_v1::ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &zwlr_output_head_v1::ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some((_, head)) = state.heads.iter_mut().find(|(h, _)| h == proxy) else {
            return;
        };
        use zwlr_output_head_v1::Event;
        match event {
            Event::Name { name } => head.name = name,
            Event::Mode { mode } => head.modes.push(mode),
            Event::Enabled { enabled } => head.enabled = enabled != 0,
            Event::CurrentMode { mode } => head.current = Some(mode),
            Event::Position { x, y } => head.pos = (x, y),
            Event::Scale { scale } => head.scale = scale,
            Event::Transform { transform } => {
                head.transform = match transform {
                    WEnum::Value(t) => u32::from(t) as i32,
                    WEnum::Unknown(t) => t as i32,
                }
            }
            Event::AdaptiveSync { state } => {
                head.adaptive_sync = Some(match state {
                    WEnum::Value(s) => u32::from(s),
                    WEnum::Unknown(s) => s,
                })
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, zwlr_output_head_v1::ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (zwlr_output_mode_v1::ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<zwlr_output_mode_v1::ZwlrOutputModeV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &zwlr_output_mode_v1::ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mode = state.modes.entry(proxy.id().protocol_id()).or_default();
        match event {
            zwlr_output_mode_v1::Event::Size { width, height } => mode.size = (width, height),
            zwlr_output_mode_v1::Event::Refresh { refresh } => mode.refresh = refresh,
            _ => {}
        }
    }
}

impl Dispatch<zwlr_output_configuration_v1::ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_output_configuration_v1::ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.result = Some(match event {
            zwlr_output_configuration_v1::Event::Succeeded => "succeeded",
            zwlr_output_configuration_v1::Event::Failed => "failed",
            zwlr_output_configuration_v1::Event::Cancelled => "cancelled",
            _ => return,
        });
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs.push((proxy.clone(), name));
        }
    }
}

impl Dispatch<zwlr_output_power_v1::ZwlrOutputPowerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_output_power_v1::ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_power_v1::Event::Mode { mode } => state.power_modes.push(match mode {
                WEnum::Value(m) => u32::from(m),
                WEnum::Unknown(m) => m,
            }),
            zwlr_output_power_v1::Event::Failed => state.power_failed = true,
            _ => {}
        }
    }
}

impl Dispatch<zwlr_gamma_control_v1::ZwlrGammaControlV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_gamma_control_v1::ZwlrGammaControlV1,
        event: zwlr_gamma_control_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => state.gamma_size = Some(size),
            zwlr_gamma_control_v1::Event::Failed => state.gamma_failed = true,
            _ => {}
        }
    }
}

macro_rules! no_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
no_events!(
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    wp_tearing_control_manager_v1::WpTearingControlManagerV1,
    wp_tearing_control_v1::WpTearingControlV1,
    wl_compositor::WlCompositor,
    wl_surface::WlSurface
);

struct Qa {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    registry: wl_registry::WlRegistry,
}

impl Qa {
    fn new() -> Self {
        let conn = Connection::connect_to_env().expect("connect");
        let queue = conn.new_event_queue();
        let registry = conn.display().get_registry(&queue.handle(), ());
        let mut qa = Self {
            conn,
            queue,
            state: State::default(),
            registry,
        };
        qa.roundtrip();
        qa
    }

    fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.state).expect("roundtrip");
    }

    fn bind<I: Proxy + 'static>(&mut self, max: u32) -> I
    where
        State: Dispatch<I, ()>,
    {
        let iface = I::interface().name;
        let Some(&(name, version)) = self.state.globals.get(iface) else {
            println!("missing global={iface}");
            std::process::exit(1);
        };
        println!("global={iface} version={version}");
        self.registry
            .bind::<I, _, _>(name, version.min(max), &self.queue.handle(), ())
    }

    fn manager(&mut self) -> zwlr_output_manager_v1::ZwlrOutputManagerV1 {
        let m = self.bind::<zwlr_output_manager_v1::ZwlrOutputManagerV1>(4);
        self.roundtrip();
        self.roundtrip();
        m
    }

    fn print_heads(&self) {
        for (_, h) in &self.state.heads {
            let mode = h
                .current
                .as_ref()
                .and_then(|m| self.state.modes.get(&m.id().protocol_id()))
                .map_or("none".to_string(), |m| {
                    format!("{}x{}@{}", m.size.0, m.size.1, m.refresh)
                });
            println!(
                "head name={} enabled={} mode={mode} pos={},{} scale={:.2} transform={} modes={} adaptive_sync={:?}",
                h.name,
                u8::from(h.enabled),
                h.pos.0,
                h.pos.1,
                h.scale,
                h.transform,
                h.modes.len(),
                h.adaptive_sync
            );
        }
        println!("serial={:?}", self.state.serial);
    }

    /// Configuration with every head enabled as it is, `edit` tweaking the named one.
    fn configure(
        &mut self,
        manager: &zwlr_output_manager_v1::ZwlrOutputManagerV1,
        serial: u32,
        target: &str,
        edit: impl Fn(&zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1),
        apply: bool,
    ) -> &'static str {
        let qh = self.queue.handle();
        let config = manager.create_configuration(serial, &qh, ());
        for (head, h) in &self.state.heads {
            let ch = config.enable_head(head, &qh, ());
            if h.name == target {
                edit(&ch);
            }
        }
        if apply {
            config.apply();
        } else {
            config.test();
        }
        self.state.result = None;
        for _ in 0..20 {
            self.roundtrip();
            if self.state.result.is_some() {
                break;
            }
        }
        config.destroy();
        self.state.result.unwrap_or("none")
    }

    fn output(&mut self, name: &str) -> wl_output::WlOutput {
        let Some(&(global, _)) = self.state.globals.get("wl_output") else {
            println!("missing global=wl_output");
            std::process::exit(1);
        };
        // Only one output in the nested case; bind it and check its name.
        let output = self
            .registry
            .bind::<wl_output::WlOutput, _, _>(global, 4, &self.queue.handle(), ());
        self.roundtrip();
        if !self.state.outputs.iter().any(|(_, n)| n == name) {
            println!("output {name} not found");
            std::process::exit(1);
        }
        output
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    let mut qa = Qa::new();
    match arg(0).as_str() {
        "heads" => {
            qa.manager();
            qa.print_heads();
        }
        "place" => {
            let manager = qa.manager();
            let serial = qa.state.serial.unwrap_or(0);
            let (x, y): (i32, i32) = (arg(2).parse().unwrap(), arg(3).parse().unwrap());
            let scale: f64 = arg(4).parse().unwrap();
            let result = qa.configure(
                &manager,
                serial,
                &arg(1),
                |ch| {
                    ch.set_position(x, y);
                    ch.set_scale(scale);
                },
                true,
            );
            println!("result={result}");
            std::thread::sleep(Duration::from_millis(200));
            qa.roundtrip();
            qa.print_heads();
        }
        "test-scale" => {
            let manager = qa.manager();
            let serial = qa.state.serial.unwrap_or(0);
            let scale: f64 = arg(2).parse().unwrap();
            let result = qa.configure(&manager, serial, &arg(1), |ch| ch.set_scale(scale), false);
            println!("result={result}");
        }
        "stale" => {
            let manager = qa.manager();
            let serial = qa.state.serial.unwrap_or(0).wrapping_sub(1);
            let result = qa.configure(&manager, serial, "", |_| {}, true);
            println!("result={result}");
        }
        "power" => {
            let output = qa.output(&arg(1));
            let manager = qa.bind::<zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1>(1);
            let control = manager.get_output_power(&output, &qa.queue.handle(), ());
            qa.roundtrip();
            let mode = if arg(2) == "off" {
                zwlr_output_power_v1::Mode::Off
            } else {
                zwlr_output_power_v1::Mode::On
            };
            control.set_mode(mode);
            qa.roundtrip();
            println!(
                "power modes={:?} failed={}",
                qa.state.power_modes, qa.state.power_failed
            );
        }
        "gamma" => {
            let output = qa.output(&arg(1));
            let manager =
                qa.bind::<zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1>(1);
            manager.get_gamma_control(&output, &qa.queue.handle(), ());
            qa.roundtrip();
            println!(
                "gamma size={:?} failed={}",
                qa.state.gamma_size, qa.state.gamma_failed
            );
        }
        "tearing-twice" => {
            let compositor = qa.bind::<wl_compositor::WlCompositor>(4);
            let manager = qa.bind::<wp_tearing_control_manager_v1::WpTearingControlManagerV1>(1);
            let qh = qa.queue.handle();
            let surface = compositor.create_surface(&qh, ());
            let first = manager.get_tearing_control(&surface, &qh, ());
            first.set_presentation_hint(wp_tearing_control_v1::PresentationHint::Async);
            surface.commit();
            qa.roundtrip();
            println!("tearing first=ok");
            manager.get_tearing_control(&surface, &qh, ());
            match qa.queue.roundtrip(&mut qa.state) {
                Ok(_) => println!("tearing second=accepted"),
                Err(err) => println!("tearing second=protocol-error {err}"),
            }
        }
        other => {
            eprintln!("unknown command {other:?}");
            std::process::exit(2);
        }
    }
    let _ = qa.conn.flush();
}
