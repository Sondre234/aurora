//! ```text
//! aurora-files [DIR]     open a window on DIR (default: $HOME)
//! ```
//!
//! Exit status: 0 closed normally, 1 failure (no Wayland compositor, ...), 2 usage error.

use std::process::ExitCode;

use aurora_files::app::{run, start_dir};

const USAGE: &str = "usage: aurora-files [DIR]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = match args.as_slice() {
        [] => None,
        [a] if a == "-h" || a == "--help" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        [a] if !a.starts_with('-') => Some(a.as_str()),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match run(start_dir(arg, home)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("files: {err}");
            ExitCode::from(1)
        }
    }
}
