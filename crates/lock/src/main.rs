use std::path::PathBuf;
use std::process::ExitCode;

use aurora_lock::app::{self, Exit};
use aurora_lock::auth::{self, HAS_PAM};
use aurora_lock::instance::{self, GuardError};
use aurora_theme::Theme;

const DEFAULT_PAM_SERVICE: &str = "login";

const USAGE: &str = "usage: aurora-lock [--pam-service NAME] [--theme FILE]

Locks the session (ext-session-lock) and unlocks it after a correct password.
Must be started by the compositor's lock action, never by hand on a live session.

  --pam-service NAME  PAM service to authenticate against
                      (default: $AURORA_LOCK_PAM_SERVICE or `login`)
  --theme FILE        theme file (default: ~/.config/aurora/theme.toml)";

struct Args {
    pam_service: String,
    theme: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        pam_service: std::env::var("AURORA_LOCK_PAM_SERVICE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_PAM_SERVICE.into()),
        theme: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pam-service" => args.pam_service = it.next().ok_or("--pam-service needs a value")?,
            "--theme" => args.theme = Some(it.next().ok_or("--theme needs a value")?.into()),
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    Ok(args)
}

fn theme_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("aurora").join("theme.toml"))
}

fn load_theme(explicit: Option<PathBuf>) -> Theme {
    let Some(path) = explicit.or_else(theme_path) else {
        return Theme::default();
    };
    match Theme::load(&path) {
        Ok((theme, warnings)) => {
            for w in warnings {
                tracing::warn!("lock-client: theme: {w}");
            }
            theme
        }
        Err(e) => {
            tracing::warn!(
                "lock-client: theme {} unusable, using defaults: {e}",
                path.display()
            );
            Theme::default()
        }
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    // A panic unwinds (the workspace never aborts) and exits non-zero without unlocking.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("lock-client: panic, the session stays locked");
        default_hook(info);
    }));

    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let _guard = match instance::acquire(&instance::guard_path()) {
        Ok(g) => g,
        Err(GuardError::AlreadyRunning) => {
            tracing::error!("lock-client: another aurora-lock is already running");
            return ExitCode::from(3);
        }
        Err(GuardError::Io(e)) => {
            tracing::error!("lock-client: cannot take the instance guard: {e}");
            return ExitCode::FAILURE;
        }
    };

    if !HAS_PAM {
        tracing::error!(
            "lock-client: WARNING: built WITHOUT PAM. This locker can never unlock; \
             the session stays locked until you switch to a TTY or quit the compositor"
        );
    }
    let Some(user) = auth::current_user() else {
        tracing::error!("lock-client: cannot determine the current user, not locking");
        return ExitCode::FAILURE;
    };
    let authenticator = auth::default_authenticator(&args.pam_service);
    tracing::info!("lock-client: auth {}", authenticator.describe());

    match app::run(load_theme(args.theme), user, authenticator) {
        Ok(Exit::Unlocked) => ExitCode::SUCCESS,
        Ok(Exit::Failed) => ExitCode::FAILURE,
        Err(e) => {
            tracing::error!("lock-client: {e}; the session stays locked if it was locked");
            ExitCode::FAILURE
        }
    }
}
