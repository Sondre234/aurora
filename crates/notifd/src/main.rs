//! `aurora-notifd`: see the library docs and `--help`.

use std::process::ExitCode;
use std::sync::Arc;

use aurora_notifd::app::{Metrics, Msg, Notifd};
use aurora_notifd::cli::{self, EXIT_BUS, EXIT_NAME_TAKEN, EXIT_USAGE, EXIT_WAYLAND};
use aurora_notifd::dbus::{self, Server, StartError};
use aurora_notifd::ipc_client;
use aurora_notifd::state::Config;
use aurora_ui::TextSystem;
use aurora_ui::runtime::Client;
use aurora_ui::runtime::calloop::channel;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .without_time()
        .with_target(false)
        .init();

    let opts = match cli::parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("aurora-notifd: {e}\n{}", cli::USAGE);
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if opts.help {
        println!("{}", cli::USAGE);
        return ExitCode::SUCCESS;
    }

    // Wayland first: a daemon that owns the name but cannot draw would swallow
    // notifications, so never take the name unless toasts can be shown.
    let mut client = match Client::connect(
        TextSystem::new(),
        Notifd::new(Config::default(), Metrics::default()),
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("notifd: wayland unavailable: {e}");
            return ExitCode::from(EXIT_WAYLAND);
        }
    };

    let (tx, rx) = channel::channel::<Msg>();
    let dbus_tx = tx.clone();
    let server = match Server::start(
        opts.bus.as_deref(),
        opts.replace,
        Arc::new(move |cmd| {
            let _ = dbus_tx.send(Msg::Dbus(cmd));
        }),
    ) {
        Ok(s) => s,
        Err(e @ StartError::NameTaken { .. }) => {
            tracing::error!("notifd: {e}");
            return ExitCode::from(EXIT_NAME_TAKEN);
        }
        Err(e) => {
            tracing::error!("notifd: {e}");
            return ExitCode::from(EXIT_BUS);
        }
    };
    let owner = server.owner.clone();
    client.app().server = Some(server);

    let inserted = client.handle().insert_source(rx, |event, _, state| {
        if let channel::Event::Msg(msg) = event {
            state.app.handle(&mut state.rt, msg);
        }
    });
    if let Err(e) = inserted {
        tracing::error!("notifd: cannot watch the message channel: {e}");
        return ExitCode::FAILURE;
    }
    ipc_client::spawn(move |m| {
        let _ = tx.send(Msg::Ipc(m));
    });

    tracing::info!("notifd: ready name={owner} bus_name={}", dbus::NAME);
    match client.run() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("notifd: event loop failed: {e}");
            ExitCode::FAILURE
        }
    }
}
