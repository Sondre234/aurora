//! The live theme: `theme.toml` next to the config file, loaded at startup and on every
//! reload, pushed to subscribers as `Event::Theme` with a revision that grows per change.
//! `SetTheme` over IPC replaces it in memory only; the next reload restores the file.
use std::path::{Path, PathBuf};

use aurora_ipc::Event;
use aurora_theme::{Theme, ThemeSnapshot};

use crate::state::Aurora;

/// `theme.toml` beside the config file (`~/.config/aurora/theme.toml` by default).
pub fn path_for(config_path: &Path) -> PathBuf {
    config_path.with_file_name("theme.toml")
}

/// Startup load: any failure gives the default theme.
pub fn load_initial(path: &Path) -> ThemeSnapshot {
    match Theme::load(path) {
        Ok((theme, warnings)) => {
            log_loaded(path, &warnings);
            ThemeSnapshot::new(1, theme)
        }
        Err(err) => {
            tracing::warn!(
                "theme: error {} using defaults",
                err.lines().next().unwrap_or_default()
            );
            ThemeSnapshot::new(1, Theme::default())
        }
    }
}

fn log_loaded(path: &Path, warnings: &[String]) {
    tracing::info!(
        "theme: loaded path={} warnings={}",
        path.display(),
        warnings.len()
    );
    for warning in warnings {
        tracing::warn!("theme: warning {warning}");
    }
}

/// The snapshot after replacing `current` with `theme`: the same one (no change) or the
/// next revision.
pub fn next(current: &ThemeSnapshot, theme: Theme) -> Option<ThemeSnapshot> {
    (theme != current.theme).then(|| ThemeSnapshot::new(current.rev + 1, theme))
}

impl Aurora {
    /// Reads `theme.toml` again. A syntax error or unreadable file keeps the current theme.
    pub fn reload_theme(&mut self) {
        match Theme::load(&self.theme_path) {
            Ok((theme, warnings)) => {
                log_loaded(&self.theme_path, &warnings);
                self.set_live_theme(theme);
            }
            Err(err) => tracing::warn!(
                "theme: error {} keeping previous",
                err.lines().next().unwrap_or_default()
            ),
        }
    }

    /// Makes `theme` the live one and pushes it if it differs.
    pub fn set_live_theme(&mut self, theme: Theme) {
        let Some(snapshot) = next(&self.theme, theme) else {
            return;
        };
        tracing::info!("theme: changed rev={}", snapshot.rev);
        self.theme = snapshot;
        let event = Event::Theme(self.theme.clone());
        self.ipc_push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_file_sits_beside_the_config() {
        assert_eq!(
            path_for(Path::new("/home/u/.config/aurora/config.toml")),
            Path::new("/home/u/.config/aurora/theme.toml")
        );
        assert_eq!(path_for(Path::new("x.toml")), Path::new("theme.toml"));
    }

    #[test]
    fn an_unchanged_theme_keeps_its_revision() {
        let current = ThemeSnapshot::new(4, Theme::default());
        assert!(next(&current, Theme::default()).is_none());
        let mut other = Theme::default();
        other.shape.gap += 1;
        let changed = next(&current, other.clone()).unwrap();
        assert_eq!(changed.rev, 5);
        assert_eq!(changed.theme, other);
    }

    #[test]
    fn a_missing_file_is_the_default_theme() {
        let dir = std::env::temp_dir().join(format!("aurora-theme-test-{}", std::process::id()));
        let snap = load_initial(&dir.join("does-not-exist.toml"));
        assert_eq!(snap.rev, 1);
        assert_eq!(snap.theme, Theme::default());
    }
}
