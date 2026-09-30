//! Command line: `aurora-notifd [--replace] [--bus <address>] [--help]`.

pub const USAGE: &str = "usage: aurora-notifd [--replace] [--bus <address>]\n\
\n\
  --replace        ask the current owner of org.freedesktop.Notifications to leave\n\
                   (default: fail with exit code 4 when the name is taken)\n\
  --bus <address>  connect to this D-Bus address instead of the session bus\n\
                   (for QA against a private dbus-daemon)\n\
\n\
exit codes: 0 ok, 2 usage, 3 bus unreachable, 4 name taken, 5 wayland unavailable";

pub const EXIT_USAGE: u8 = 2;
pub const EXIT_BUS: u8 = 3;
pub const EXIT_NAME_TAKEN: u8 = 4;
pub const EXIT_WAYLAND: u8 = 5;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub replace: bool,
    pub bus: Option<String>,
    pub help: bool,
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut o = Options::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--replace" => o.replace = true,
            "--bus" => match it.next() {
                Some(addr) if !addr.is_empty() => o.bus = Some(addr),
                _ => return Err("--bus needs an address".into()),
            },
            "-h" | "--help" => o.help = true,
            other => match other.strip_prefix("--bus=") {
                Some(addr) if !addr.is_empty() => o.bus = Some(addr.to_string()),
                _ => return Err(format!("unknown argument {other:?}")),
            },
        }
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Options, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_never_replace() {
        assert_eq!(p(&[]).unwrap(), Options::default());
        assert!(!p(&[]).unwrap().replace);
    }

    #[test]
    fn flags() {
        assert!(p(&["--replace"]).unwrap().replace);
        assert_eq!(
            p(&["--bus", "unix:path=/x"]).unwrap().bus.as_deref(),
            Some("unix:path=/x")
        );
        assert_eq!(
            p(&["--bus=unix:path=/y"]).unwrap().bus.as_deref(),
            Some("unix:path=/y")
        );
        assert!(p(&["-h"]).unwrap().help);
    }

    #[test]
    fn errors() {
        assert!(p(&["--bus"]).is_err());
        assert!(p(&["--bus="]).is_err());
        assert!(p(&["--nope"]).is_err());
    }
}
