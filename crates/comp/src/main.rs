mod action;
mod actions;
mod anim;
mod backend;
mod capture;
mod cli;
mod config;
mod debug;
mod display;
mod dmabuf;
mod effects;
mod emergency;
mod focus;
mod handlers;
mod ime;
mod input;
mod ipc;
mod keymap;
mod layers;
mod libinput;
mod lock;
mod log;
mod outputs;
mod overview;
mod pacing;
mod protocols;
mod safety;
mod sandbox;
mod scene;
mod services;
mod session;
mod session_env;
mod spawn;
mod state;
mod syncobj;
mod virtual_input;
mod wm;
mod xwayland;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

use backend::{Backend, DrmBackend};
use cli::{BackendKind, Cli};
use state::Aurora;

fn main() {
    log::install_panic_hook();
    log::init();
    // `run` owns the session and everything else that must drop before this point.
    if let Err(err) = run() {
        tracing::error!(%err, "aurora failed");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse()?;
    tracing::info!(backend = ?cli.backend, timeout = ?cli.timeout, qa = cli.qa, "aurora starting");
    let config_path = config::resolve_path(cli.config.clone());
    let config = config::Config::load_initial(&config_path);

    // Declared before `state` so the state (seat, session, devices) drops first.
    let mut event_loop: EventLoop<Aurora> = EventLoop::try_new()?;
    let display: Display<Aurora> = Display::new()?;

    let handle = event_loop.handle();
    let backend = match cli.backend {
        BackendKind::Winit => Backend::Winit,
        BackendKind::Drm => {
            // First source in the loop, and the first thing that touches the machine.
            let (session, notifier) = session::open()?;
            session::insert_notifier(&handle, notifier).map_err(arm)?;
            let seat = smithay::backend::session::Session::seat(&session);
            let primary_gpu = backend::drm::gpu::select_primary(&seat).map_err(arm)?;
            let libinput = libinput::new_context(&session, &seat).map_err(arm)?;
            Backend::Drm(Box::new(DrmBackend::new(session, libinput, primary_gpu)))
        }
    };
    let mut state =
        Aurora::new(&mut event_loop, display, backend, config, config_path).map_err(arm)?;
    state.qa = cli.qa;
    state.apply_keymap();
    // Declared after `state`, so it drops first and bounds the teardown on every exit path.
    let _deadline = ExitDeadline;

    safety::insert_signals(&handle);
    if let Some(timeout) = cli.timeout {
        safety::insert_timeout(&handle, timeout);
        safety::spawn_watchdog(timeout);
    }

    match cli.backend {
        BackendKind::Winit => backend::winit::init(&mut event_loop, &mut state)?,
        BackendKind::Drm => {
            if let Backend::Drm(drm) = &state.backend {
                libinput::insert_source(&handle, drm.input_source())?;
            }
            backend::drm::init(&handle, &mut state)?;
        }
    }

    tracing::info!(socket = ?state.socket_name, "aurora listening");

    // Before any child starts, so they all see AURORA_IPC_SOCK.
    state.ipc_start();
    state.start_xwayland();
    // D-Bus and systemd activated services (portals, keyring, polkit) find this session.
    state.import_session_env();
    state.spawn(&cli.command);
    // exec-once: once per process, never re-run by a reload.
    for cmd in state.config.autostart.clone() {
        state.spawn(&cmd);
    }
    state.services_init();

    let result = event_loop.run(None, &mut state, |state| {
        // Input and request handlers only queue events; nothing else flushes them.
        let _ = state.display_handle.flush_clients();
        // A workspace slide that ended has its outgoing windows to take out of the Space.
        state.finish_slides();
        // Window, workspace, focus and output changes reach IPC subscribers.
        state.ipc_update();
        // A lock client that vanished, or an output that changed, while locked.
        state.lock_update();
        // Output changes reach wlr-output-management clients.
        state.display_update();
    });
    // Every exit path: the window manager must go before the state drops, and the server
    // with it, so no Xwayland outlives the compositor.
    state.services_shutdown();
    state.ipc_shutdown();
    state.shutdown_xwayland();
    result?;
    safety::arm_exit_deadline();
    tracing::info!("aurora exiting");
    Ok(())
}

/// Arms the hard-exit deadline when dropped, including on early `?` returns.
struct ExitDeadline;

impl Drop for ExitDeadline {
    fn drop(&mut self) {
        safety::arm_exit_deadline();
    }
}

/// For `map_err` on startup steps that run before the `ExitDeadline` guard exists: their
/// locals (session, devices) drop before any guard could.
fn arm<E>(err: E) -> E {
    safety::arm_exit_deadline();
    err
}
