//! ```text
//! aurora-term [--class ID] [--title TEXT] [--cwd DIR] [--scrollback N] [--hold] [-e CMD [ARG...]]
//! ```
//!
//! One process per window: an xdg-toplevel Wayland client, tiled by the compositor like
//! any other window, running `$SHELL` (or the `-e` command) on a pty. Colors and the
//! mono font come from the theme (`theme.toml`, live over the compositor IPC when
//! `AURORA_IPC_SOCK` is reachable). Logs go to stderr (`RUST_LOG` filters).
//!
//! Keys: Ctrl+Shift+C / Ctrl+Shift+V copy and paste, Shift+Insert and the middle button
//! paste the primary selection (selecting text owns it), Shift+PageUp/PageDown and the
//! wheel scroll the history, any typed key returns to the bottom. Ctrl+click opens an
//! OSC 8 hyperlink with `xdg-open`.
//!
//! Test hook: built with `--features qa-hooks` (never by default), the bytes of the file
//! named by `AURORA_TERM_TEST_INPUT` are written to the pty right after the child starts,
//! as if typed.
//!
//! Exit status: 0 normally, 1 when the window or the child could not be started, 2 for
//! a usage error.

use std::process::ExitCode;

use aurora_term::app::{self, TermApp};
use aurora_term::cli::{self, Parsed};
use aurora_ui::TextSystem;
use aurora_ui::runtime::Client;
use aurora_ui::runtime::calloop::timer::{TimeoutAction, Timer};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match cli::parse(&args) {
        Ok(Parsed::Run(c)) => *c,
        Ok(Parsed::Help) => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("aurora-term: {e}\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let text = TextSystem::new();
    let client = match Client::connect(text.clone(), TermApp::new(cli, text)) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("term: cannot start: {e}");
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
        tracing::error!("term: cannot start the event loop: {e}");
        return ExitCode::FAILURE;
    }
    match client.run() {
        Ok(app) if !app.fatal => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            tracing::error!("term: event loop failed: {e}");
            ExitCode::FAILURE
        }
    }
}
