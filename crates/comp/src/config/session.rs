//! `[session]`: how a real (DRM) session announces itself to the rest of the user's login.

use toml::Value;

use super::raw::{check_keys, get_bool, soft, table_of};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// Hand WAYLAND_DISPLAY, DISPLAY and friends to D-Bus and systemd activated services
    /// through `dbus-update-activation-environment --systemd`. DRM backend only.
    pub import_environment: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            import_environment: true,
        }
    }
}

pub fn session(section: Option<&Value>, warnings: &mut Vec<String>) -> Session {
    let mut s = Session::default();
    let Some(table) = table_of("session", section, warnings) else {
        return s;
    };
    let ctx = "session";
    check_keys(ctx, table, &["import_environment"], warnings);
    if let Some(b) = soft(get_bool(ctx, table, "import_environment"), warnings) {
        s.import_environment = b;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Session, Vec<String>) {
        let raw = super::super::raw::parse(text).expect("test toml parses");
        let mut warnings = Vec::new();
        (session(raw.session.as_ref(), &mut warnings), warnings)
    }

    #[test]
    fn import_defaults_on_and_can_be_turned_off() {
        assert!(parse("").0.import_environment);
        let (s, w) = parse("[session]\nimport_environment = false\n");
        assert!(!s.import_environment && w.is_empty(), "{w:?}");
    }

    #[test]
    fn bad_values_warn_and_keep_the_default() {
        let (s, w) = parse("[session]\nimport_environment = \"yes\"\nsystemd = 1\n");
        assert!(s.import_environment);
        assert!(w.iter().any(|w| w.contains("import_environment")), "{w:?}");
        assert!(w.iter().any(|w| w.contains("systemd")), "{w:?}");
        let (_, w) = parse("session = 3\n");
        assert!(w.iter().any(|w| w.contains("session")), "{w:?}");
    }
}
