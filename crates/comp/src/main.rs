mod backend;
mod cli;
mod handlers;
mod input;
mod keymap;
mod libinput;
mod log;
mod safety;
mod session;
mod state;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

use backend::{Backend, DrmBackend};
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

    let handle = event_loop.handle();
    let backend = match cli.backend {
        BackendKind::Winit => Backend::Winit,
        BackendKind::Drm => {
            // First source in the loop, and the first thing that touches the machine.
            let (session, notifier) = session::open()?;
            session::insert_notifier(&handle, notifier)?;
            let seat = smithay::backend::session::Session::seat(&session);
            let libinput = libinput::new_context(&session, &seat)?;
            Backend::Drm(DrmBackend::new(session, libinput))
        }
    };
    let mut state = Aurora::new(&mut event_loop, display, backend);
    state.apply_keymap();

    safety::insert_signals(&handle);
    if let Some(timeout) = cli.timeout {
        safety::insert_timeout(&handle, timeout);
    }

    match cli.backend {
        BackendKind::Winit => backend::winit::init(&mut event_loop, &mut state)?,
        BackendKind::Drm => {
            let Backend::Drm(drm) = &state.backend else {
                unreachable!("backend chosen above")
            };
            libinput::insert_source(&handle, drm.input_source())?;
            log_udev_devices(&state.backend.seat_name());
            return Err(
                "the DRM backend is not implemented yet (session, udev and libinput are up)".into(),
            );
        }
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

/// Lists the seat's DRM nodes; GPU selection comes with the DRM device step.
fn log_udev_devices(seat: &str) {
    match smithay::backend::udev::UdevBackend::new(seat) {
        Ok(udev) => {
            for (id, path) in udev.device_list() {
                tracing::info!(id, path = %path.display(), "udev drm device");
            }
        }
        Err(err) => tracing::error!(%err, "udev backend failed"),
    }
}
