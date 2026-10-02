//! Activation environment: tells the D-Bus daemon and the systemd user manager how to reach
//! this session, so services they start on demand (xdg-desktop-portal, gnome-keyring, polkit
//! agents, anything `WantedBy=graphical-session.target`) talk to Aurora and its XWayland.
//!
//! Real sessions only (DRM backend): a nested compositor must not point the host session's
//! services at itself. Runs `dbus-update-activation-environment --systemd` once the socket
//! exists, and again whenever the values change (XWayland's DISPLAY known later). The
//! helper is never waited for on the loop; its thread logs the outcome and reaps it.
use std::{ffi::OsString, time::Duration};

use crate::{backend::Backend, state::Aurora};

/// Exported, in this order, when they have a value.
const VARS: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_DESKTOP",
    "AURORA_IPC_SOCK",
];
const SESSION_DESKTOP: &str = "aurora";
const HELPER: &str = "dbus-update-activation-environment";
/// A hung bus is killed after this; the session works on without the import.
const LIMIT: Duration = Duration::from_secs(10);

type Env = Vec<(&'static str, OsString)>;

/// The last environment handed over, so an unchanged one is not sent twice.
#[derive(Default)]
pub struct EnvImport {
    last: Option<Env>,
}

/// The variables to export, picked from `base` (the children's environment) in `VARS`
/// order, plus `XDG_SESSION_DESKTOP`.
fn import_env(base: Env) -> Env {
    let mut env = base;
    env.push(("XDG_SESSION_DESKTOP", SESSION_DESKTOP.into()));
    VARS.iter()
        .filter_map(|name| env.iter().find(|(k, _)| k == name).cloned())
        .collect()
}

/// Names only: the helper reads the values from its own environment, set on the Command.
fn import_argv(env: &Env) -> Vec<String> {
    [HELPER, "--systemd"]
        .into_iter()
        .map(str::to_string)
        .chain(env.iter().map(|(k, _)| k.to_string()))
        .collect()
}

impl Aurora {
    /// Hands the session environment over, unless nested, turned off by
    /// `[session] import_environment`, or already sent with these values.
    pub fn import_session_env(&mut self) {
        if !matches!(self.backend, Backend::Drm(_)) || !self.config.session.import_environment {
            return;
        }
        let env = import_env(self.base_env());
        if self.env_import.last.as_ref() == Some(&env) {
            return;
        }
        let argv = import_argv(&env);
        tracing::info!("session: importing environment {}", argv[2..].join(" "));
        let refs: Vec<_> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        crate::spawn::spawn_logged(&argv, &refs, "session: environment import", LIMIT);
        self.env_import.last = Some(env);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_session_variables_in_order() {
        let base: Env = vec![
            ("XDG_ACTIVATION_TOKEN", "t".into()),
            ("AURORA_IPC_SOCK", "/run/user/1000/aurora/ipc.sock".into()),
            ("XDG_CURRENT_DESKTOP", "Aurora".into()),
            ("WAYLAND_DISPLAY", "wayland-1".into()),
            ("XDG_SESSION_TYPE", "wayland".into()),
        ];
        let env = import_env(base);
        let names: Vec<_> = env.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            names,
            [
                "WAYLAND_DISPLAY",
                "XDG_CURRENT_DESKTOP",
                "XDG_SESSION_TYPE",
                "XDG_SESSION_DESKTOP",
                "AURORA_IPC_SOCK"
            ]
        );
        assert_eq!(
            import_argv(&env)[..3],
            [HELPER, "--systemd", "WAYLAND_DISPLAY"]
        );
    }

    #[test]
    fn display_is_exported_once_known() {
        let env = import_env(vec![
            ("DISPLAY", ":1".into()),
            ("WAYLAND_DISPLAY", "w".into()),
        ]);
        assert_eq!(env[1], ("DISPLAY", ":1".into()));
    }
}
