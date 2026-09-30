//! `aurora-shell`: the Aurora status bar.
//!
//! One layer-shell surface per output (Top layer, anchored top, namespace `aurora-bar`,
//! exclusive zone = bar height), drawn by `aurora-ui` with a translucent background so
//! the compositor's blur shows through. Outputs that appear, vanish or change scale are
//! followed live. Widgets, left to right: the workspaces of that output (active one
//! highlighted, occupied ones listed, urgent ones marked, click switches), the focused
//! window's app id and title when it lives on that output (ellipsized), and a clock
//! (`Tue 30 Sep  14:05`, repainted on the minute; idle in between).
//!
//! # Configuration
//!
//! There is no config file of its own. Colors, fonts, corner radius, gap and the bar
//! height (`[shape] bar_height`) come from the compositor's theme, pushed over IPC and
//! applied live without a restart (font sizes are points, converted at 96 dpi). The
//! process is started and restarted by the compositor's supervisor; declare it in
//! `config.toml`:
//!
//! ```toml
//! [services.shell]
//! command = "aurora-shell"
//! restart = "always"
//! ```
//!
//! The compositor gives it `WAYLAND_DISPLAY` and `AURORA_IPC_SOCK`. Logs go to stderr
//! (`RUST_LOG` filters); `shell: ready outputs=<n>` marks startup. If the compositor's
//! IPC socket is missing or goes away the bar keeps running, shows no workspaces or
//! title, and reconnects with backoff (250 ms doubling to 5 s).

mod app;
mod clock;
mod ipc_client;
mod model;
mod view;

use std::process::ExitCode;

use aurora_ui::TextSystem;
use aurora_ui::runtime::Client;
use aurora_ui::runtime::calloop::timer::{TimeoutAction, Timer};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let client = match Client::connect(TextSystem::new(), app::Bar::new()) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("shell: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let started = client
        .handle()
        .insert_source(Timer::immediate(), |_, _, state| {
            app::start(state);
            TimeoutAction::Drop
        });
    if let Err(e) = started {
        tracing::error!("shell: cannot start the event loop: {e}");
        return ExitCode::FAILURE;
    }
    match client.run() {
        Ok(bar) if !bar.fatal => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            tracing::error!("shell: event loop failed: {e}");
            ExitCode::FAILURE
        }
    }
}
