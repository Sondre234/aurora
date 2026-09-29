use std::time::Duration;

/// DRM sessions can black-screen the machine, so they exit on their own unless told otherwise.
const DRM_DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Winit,
    Drm,
}

#[derive(Debug)]
pub struct Cli {
    pub backend: BackendKind,
    /// `None` means run until quit.
    pub timeout: Option<Duration>,
    pub command: String,
}

const USAGE: &str =
    "usage: aurora-comp [--winit | --drm] [--timeout <secs>] [--no-timeout] [-c <command>]";

impl Cli {
    pub fn parse() -> Result<Self, String> {
        let nested =
            std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
        Self::parse_from(std::env::args().skip(1), nested)
    }

    fn parse_from(args: impl Iterator<Item = String>, nested: bool) -> Result<Self, String> {
        let (mut winit, mut drm, mut no_timeout) = (false, false, false);
        let mut timeout = None;
        let mut command = None;

        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--winit" => winit = true,
                "--drm" => drm = true,
                "--no-timeout" => no_timeout = true,
                "--timeout" => {
                    let secs = args.next().ok_or("--timeout needs a value")?;
                    let secs: u64 = secs
                        .parse()
                        .map_err(|_| format!("invalid --timeout value {secs:?}"))?;
                    timeout = Some(Duration::from_secs(secs));
                }
                "-c" | "--command" => command = Some(args.next().ok_or("-c needs a command")?),
                "-h" | "--help" => return Err(USAGE.to_string()),
                other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
            }
        }

        if winit && drm {
            return Err("--winit and --drm are mutually exclusive".into());
        }
        if no_timeout && timeout.is_some() {
            return Err("--timeout and --no-timeout are mutually exclusive".into());
        }

        let backend = match (winit, drm) {
            (true, _) => BackendKind::Winit,
            (_, true) => BackendKind::Drm,
            _ if nested => BackendKind::Winit,
            _ => BackendKind::Drm,
        };

        if backend == BackendKind::Drm && timeout.is_none() {
            if no_timeout {
                tracing::warn!(
                    "DRM backend running WITHOUT a timeout; only the quit chord or a signal ends the session"
                );
            } else {
                timeout = Some(DRM_DEFAULT_TIMEOUT);
            }
        }

        Ok(Self {
            backend,
            timeout,
            command: command.unwrap_or_else(|| "kitty".to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], nested: bool) -> Result<Cli, String> {
        Cli::parse_from(args.iter().map(|s| s.to_string()), nested)
    }

    #[test]
    fn backend_selection_and_timeouts() {
        assert_eq!(parse(&[], true).unwrap().backend, BackendKind::Winit);
        assert_eq!(parse(&[], false).unwrap().backend, BackendKind::Drm);
        assert!(parse(&["--winit", "--drm"], true).is_err());
        assert_eq!(parse(&[], true).unwrap().timeout, None);
        assert_eq!(
            parse(&["--drm"], true).unwrap().timeout,
            Some(DRM_DEFAULT_TIMEOUT)
        );
        assert_eq!(
            parse(&["--drm", "--no-timeout"], true).unwrap().timeout,
            None
        );
        assert_eq!(
            parse(&["--drm", "--timeout", "5"], true).unwrap().timeout,
            Some(Duration::from_secs(5))
        );
    }
}
