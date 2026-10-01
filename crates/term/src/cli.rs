//! Command line of `aurora-term`.

use std::path::PathBuf;

pub const DEFAULT_SCROLLBACK: usize = 10_000;
pub const MAX_SCROLLBACK: usize = 200_000;

pub const USAGE: &str = "usage: aurora-term [--class ID] [--title TEXT] [--cwd DIR] \
[--scrollback N] [--hold] [-e CMD [ARG...]]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// xdg app id.
    pub class: String,
    /// Initial window title (the child can change it with OSC 0/2).
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub scrollback: usize,
    /// Keep the window open after the child exits.
    pub hold: bool,
    /// Program and arguments instead of `$SHELL`.
    pub command: Option<Vec<String>>,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            class: "aurora-term".into(),
            title: "aurora-term".into(),
            cwd: None,
            scrollback: DEFAULT_SCROLLBACK,
            hold: false,
            command: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Box<Cli>),
    Help,
}

/// Parse the arguments after the program name. `-e` takes everything after it.
pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut cli = Cli::default();
    let mut it = args.iter();
    let value = |it: &mut std::slice::Iter<'_, String>, flag: &str| {
        it.next()
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--class" => cli.class = value(&mut it, "--class")?,
            "--title" => cli.title = value(&mut it, "--title")?,
            "--cwd" => cli.cwd = Some(PathBuf::from(value(&mut it, "--cwd")?)),
            "--scrollback" => {
                let v = value(&mut it, "--scrollback")?;
                let n: usize = v
                    .parse()
                    .map_err(|_| format!("--scrollback: not a number: {v}"))?;
                cli.scrollback = n.min(MAX_SCROLLBACK);
            }
            "--hold" => cli.hold = true,
            "-e" | "--command" => {
                let rest: Vec<String> = it.by_ref().cloned().collect();
                if rest.is_empty() {
                    return Err("-e needs a command".into());
                }
                cli.command = Some(rest);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Parsed::Run(Box::new(cli)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn run(v: &[&str]) -> Cli {
        match parse(&args(v)) {
            Ok(Parsed::Run(c)) => *c,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn defaults() {
        let c = run(&[]);
        assert_eq!(c, Cli::default());
        assert_eq!((c.class.as_str(), c.scrollback), ("aurora-term", 10_000));
    }

    #[test]
    fn flags() {
        let c = run(&[
            "--class",
            "x",
            "--title",
            "t",
            "--cwd",
            "/tmp",
            "--scrollback",
            "50",
            "--hold",
        ]);
        assert_eq!((c.class.as_str(), c.title.as_str()), ("x", "t"));
        assert_eq!(c.cwd, Some(PathBuf::from("/tmp")));
        assert_eq!(c.scrollback, 50);
        assert!(c.hold && c.command.is_none());
    }

    #[test]
    fn command_takes_the_rest() {
        let c = run(&["--hold", "-e", "sh", "-c", "echo --hold"]);
        assert!(c.hold);
        assert_eq!(c.command, Some(args(&["sh", "-c", "echo --hold"])));
    }

    #[test]
    fn scrollback_is_capped() {
        assert_eq!(
            run(&["--scrollback", "99999999"]).scrollback,
            MAX_SCROLLBACK
        );
        assert_eq!(run(&["--scrollback", "0"]).scrollback, 0);
    }

    #[test]
    fn errors_and_help() {
        assert!(parse(&args(&["--scrollback", "x"])).is_err());
        assert!(parse(&args(&["--class"])).is_err());
        assert!(parse(&args(&["-e"])).is_err());
        assert!(parse(&args(&["--bogus"])).is_err());
        assert_eq!(parse(&args(&["-h"])), Ok(Parsed::Help));
    }
}
