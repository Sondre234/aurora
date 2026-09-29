mod handlers;
mod input;
mod state;
mod winit;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

use state::Aurora;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let mut event_loop: EventLoop<Aurora> = EventLoop::try_new()?;
    let display: Display<Aurora> = Display::new()?;
    let mut state = Aurora::new(&mut event_loop, display);

    winit::init(&mut event_loop, &mut state)?;

    // Children spawned from here on connect to us, not the host compositor.
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
    tracing::info!(socket = ?state.socket_name, "aurora listening");

    spawn_client();

    event_loop.run(None, &mut state, |_| {})?;
    Ok(())
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// `aurora-comp -c <command>` runs a client on startup; defaults to kitty.
fn spawn_client() {
    let mut args = std::env::args().skip(1);
    let command = match (args.next().as_deref(), args.next()) {
        (Some("-c" | "--command"), Some(cmd)) => cmd,
        _ => "kitty".to_string(),
    };
    if let Err(err) = std::process::Command::new(&command).spawn() {
        tracing::warn!(%command, %err, "failed to spawn startup client");
    }
}
