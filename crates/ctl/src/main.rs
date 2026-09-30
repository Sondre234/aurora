//! `auroractl`: debug and scripting client for the Aurora compositor IPC.
//!
//! ```text
//! auroractl snapshot                 print the full state as pretty JSON
//! auroractl events [topic...]        subscribe and print one JSON event per line
//!                                    (topics: workspaces windows focus outputs theme config;
//!                                    none means all)
//! auroractl raw '<json>'             send one Request given as JSON, print the reply
//!                                    e.g. raw '{"SwitchWorkspace":{"output":null,"index":2}}'
//!                                         raw '"ListWindows"'
//! ```
//!
//! The socket is `$AURORA_IPC_SOCK` or `$XDG_RUNTIME_DIR/aurora/ipc.sock`. The client is
//! blocking and minimal on purpose; JSON is serde's default external tagging of the
//! `aurora-ipc` types. Exit status: 0 ok, 1 runtime failure, 2 usage error.

use std::{
    io::{self, Write},
    os::unix::net::UnixStream,
    process::ExitCode,
};

use aurora_ipc::{
    Body, Event, Frame, Request, Topic, check_first_frame, read_frame, socket_path, write_frame,
};

#[derive(Debug, PartialEq)]
enum Command {
    Snapshot,
    Events(Vec<Topic>),
    Raw(String),
    Help,
}

const USAGE: &str = "usage: auroractl snapshot | events [topic...] | raw '<json request>'\n\
topics: workspaces windows focus outputs theme config";

fn parse_topic(name: &str) -> Option<Topic> {
    Some(match name {
        "workspaces" => Topic::Workspaces,
        "windows" => Topic::Windows,
        "focus" => Topic::Focus,
        "outputs" => Topic::Outputs,
        "theme" => Topic::Theme,
        "config" => Topic::Config,
        _ => return None,
    })
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Err("missing command".into());
    };
    match (cmd.as_str(), rest) {
        ("help" | "-h" | "--help", _) => Ok(Command::Help),
        ("snapshot", []) => Ok(Command::Snapshot),
        ("snapshot", _) => Err("snapshot takes no arguments".into()),
        ("events", names) => {
            let mut topics = Vec::new();
            for n in names {
                topics.push(parse_topic(n).ok_or_else(|| format!("unknown topic {n:?}"))?);
            }
            if topics.is_empty() {
                topics = Topic::ALL.to_vec();
            }
            Ok(Command::Events(topics))
        }
        ("raw", [json]) => Ok(Command::Raw(json.clone())),
        ("raw", _) => Err("raw takes exactly one JSON argument".into()),
        (other, _) => Err(format!("unknown command {other:?}")),
    }
}

fn connect() -> Result<UnixStream, String> {
    let path = socket_path().ok_or("neither AURORA_IPC_SOCK nor XDG_RUNTIME_DIR is set")?;
    let mut stream =
        UnixStream::connect(&path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    write_frame(&mut stream, &Frame::hello("auroractl")).map_err(|e| e.to_string())?;
    let first = read_frame(&mut stream)
        .map_err(|e| e.to_string())?
        .ok_or("compositor closed the connection during the handshake")?;
    check_first_frame(&first).map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Sends `request` with id 1 and returns the matching reply body, skipping events.
fn call(stream: &mut UnixStream, request: Request) -> Result<Body, String> {
    write_frame(stream, &Frame::request(1, request)).map_err(|e| e.to_string())?;
    loop {
        let frame = read_frame(stream)
            .map_err(|e| e.to_string())?
            .ok_or("compositor closed the connection")?;
        if frame.id == 1 && matches!(frame.body, Body::Response(_) | Body::Error(_)) {
            return Ok(frame.body);
        }
    }
}

fn print_reply(body: &Body) -> Result<ExitCode, String> {
    let (json, code) = match body {
        Body::Response(r) => (serde_json::to_string_pretty(r), ExitCode::SUCCESS),
        Body::Error(e) => (serde_json::to_string_pretty(e), ExitCode::FAILURE),
        other => return Err(format!("unexpected reply {other:?}")),
    };
    println!("{}", json.map_err(|e| e.to_string())?);
    Ok(code)
}

fn run(command: Command) -> Result<ExitCode, String> {
    match command {
        Command::Help => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Snapshot => {
            let mut stream = connect()?;
            print_reply(&call(&mut stream, Request::GetSnapshot)?)
        }
        Command::Raw(json) => {
            let request: Request =
                serde_json::from_str(&json).map_err(|e| format!("bad request json: {e}"))?;
            let mut stream = connect()?;
            print_reply(&call(&mut stream, request)?)
        }
        Command::Events(topics) => {
            let mut stream = connect()?;
            write_frame(&mut stream, &Frame::new(0, Body::Subscribe(topics)))
                .map_err(|e| e.to_string())?;
            let stdout = io::stdout();
            loop {
                let Some(frame) = read_frame(&mut stream).map_err(|e| e.to_string())? else {
                    return Ok(ExitCode::SUCCESS);
                };
                if let Body::Event(event) = &frame.body {
                    print_event(&stdout, event)?;
                }
            }
        }
    }
}

fn print_event(stdout: &io::Stdout, event: &Event) -> Result<(), String> {
    let line = serde_json::to_string(event).map_err(|e| e.to_string())?;
    let mut out = stdout.lock();
    // A closed pipe (`auroractl events | head`) ends the stream quietly.
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
        std::process::exit(0);
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("auroractl: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("auroractl: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_args(&args(&["snapshot"])), Ok(Command::Snapshot));
        assert_eq!(parse_args(&args(&["--help"])), Ok(Command::Help));
        assert_eq!(
            parse_args(&args(&["events"])),
            Ok(Command::Events(Topic::ALL.to_vec()))
        );
        assert_eq!(
            parse_args(&args(&["events", "focus", "theme"])),
            Ok(Command::Events(vec![Topic::Focus, Topic::Theme]))
        );
        assert_eq!(
            parse_args(&args(&["raw", "\"ListWindows\""])),
            Ok(Command::Raw("\"ListWindows\"".into()))
        );
    }

    #[test]
    fn rejects_bad_usage() {
        for bad in [
            &[][..],
            &["snapshot", "x"],
            &["events", "bogus"],
            &["raw"],
            &["raw", "a", "b"],
            &["nope"],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn raw_json_shapes_parse_as_requests() {
        let r: Request = serde_json::from_str("\"ListWindows\"").unwrap();
        assert_eq!(r, Request::ListWindows);
        let r: Request =
            serde_json::from_str(r#"{"SwitchWorkspace":{"output":null,"index":2}}"#).unwrap();
        assert_eq!(
            r,
            Request::SwitchWorkspace {
                output: None,
                index: 2
            }
        );
        assert!(serde_json::from_str::<Request>("{\"Nope\":1}").is_err());
    }
}
