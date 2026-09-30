//! ```text
//! aurora-launcher            run the daemon (started by [services.launcher], starts hidden)
//! aurora-launcher toggle     show the launcher if hidden, hide it if shown
//! aurora-launcher show|hide  force one state
//! ```
//!
//! Exit status: 0 ok, 1 failure (daemon not running, cannot start), 2 usage error.

use std::process::ExitCode;

use aurora_launcher::control::{self, Command};

const USAGE: &str = "usage: aurora-launcher [daemon | toggle | show | hide]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["daemon"] => daemon(),
        [word] if control::parse_command(word).is_some() => {
            let cmd = control::parse_command(word).unwrap_or(Command::Toggle);
            match control::send(cmd) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("aurora-launcher: {err}");
                    ExitCode::from(1)
                }
            }
        }
        ["-h" | "--help" | "help"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn daemon() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    match aurora_launcher::app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("launcher: {err}");
            ExitCode::from(1)
        }
    }
}
