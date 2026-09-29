mod backend;
mod cli;
mod handlers;
mod input;
mod log;
mod safety;
mod state;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

use backend::Backend;
use cli::{BackendKind, Cli};
use state::Aurora;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    log::install_panic_hook();
    log::init();

    let cli = Cli::parse()?;
    tracing::info!(backend = ?cli.backend, timeout = ?cli.timeout, "aurora starting");

    // Declared before `state` so the state (seat, session, devices) drops first.
    let mut event_loop: EventLoop<Aurora> = EventLoop::try_new()?;
    let display: Display<Aurora> = Display::new()?;

    let backend = match cli.backend {
        BackendKind::Winit => Backend::Winit,
        BackendKind::Drm => return Err("the DRM backend is not implemented yet".into()),
    };
    let mut state = Aurora::new(&mut event_loop, display, backend);

    let handle = event_loop.handle();
    safety::insert_signals(&handle);
    if let Some(timeout) = cli.timeout {
        safety::insert_timeout(&handle, timeout);
    }

    match cli.backend {
        BackendKind::Winit => backend::winit::init(&mut event_loop, &mut state)?,
        BackendKind::Drm => unreachable!("rejected above"),
    }

    // Children spawned from here on connect to us, not the host compositor.
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
    tracing::info!(socket = ?state.socket_name, "aurora listening");

    spawn_client(&cli.command);

    event_loop.run(None, &mut state, |_| {})?;
    tracing::info!("aurora exiting");
    Ok(())
}

fn spawn_client(command: &str) {
    if let Err(err) = std::process::Command::new(command).spawn() {
        tracing::warn!(%command, %err, "failed to spawn startup client");
    }
}
