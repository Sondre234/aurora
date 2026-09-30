//! Frecency: how often and how recently an app was launched.
//!
//! Each app keeps one number that halves every [`HALF_LIFE_SECS`]; a launch adds 1 to the
//! decayed value. So ten launches last week outrank one launch today, and a habit that
//! stopped fades out. All functions take `now` explicitly, so they are pure and testable;
//! only [`Frecency::load`] / [`Frecency::save`] touch the disk (owner: the launcher,
//! invalidated by every launch, bounded by [`MAX_ENTRIES`]).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A launch loses half its weight after two weeks.
pub const HALF_LIFE_SECS: f64 = 14.0 * 24.0 * 3600.0;
/// Entries below this decayed score are dropped when saving.
pub const PRUNE_BELOW: f64 = 0.02;
/// Hard cap on stored apps.
pub const MAX_ENTRIES: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Record {
    /// Score as of `updated`.
    score: f64,
    /// Unix seconds.
    updated: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Frecency {
    version: u32,
    apps: HashMap<String, Record>,
}

fn decayed(r: &Record, now: u64) -> f64 {
    let age = now.saturating_sub(r.updated) as f64;
    r.score * 0.5f64.powf(age / HALF_LIFE_SECS)
}

impl Frecency {
    pub fn new() -> Self {
        Self {
            version: 1,
            apps: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    /// Current decayed score of an app (0 when never launched).
    pub fn score(&self, id: &str, now: u64) -> f64 {
        self.apps.get(id).map_or(0.0, |r| decayed(r, now))
    }

    /// Records one launch.
    pub fn record_launch(&mut self, id: &str, now: u64) {
        let score = self.score(id, now) + 1.0;
        self.apps.insert(
            id.to_string(),
            Record {
                score,
                updated: now,
            },
        );
    }

    /// Drops faded entries and, beyond [`MAX_ENTRIES`], the lowest-scoring ones.
    pub fn prune(&mut self, now: u64) {
        self.apps.retain(|_, r| decayed(r, now) >= PRUNE_BELOW);
        if self.apps.len() > MAX_ENTRIES {
            let mut scored: Vec<(String, f64)> = self
                .apps
                .iter()
                .map(|(k, r)| (k.clone(), decayed(r, now)))
                .collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            for (k, _) in scored.into_iter().skip(MAX_ENTRIES) {
                self.apps.remove(&k);
            }
        }
    }

    /// Score bonus added to a fuzzy score: logarithmic so a heavy habit cannot bury a much
    /// better textual match, capped at [`BONUS_CAP`].
    pub fn bonus(&self, id: &str, now: u64) -> i32 {
        bonus_for(self.score(id, now))
    }

    // ---- persistence ----

    /// Reads the file; a missing, unreadable or corrupt file gives an empty history.
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                tracing::warn!("launcher: ignoring corrupt frecency file: {err}");
                Self::new()
            }),
            Err(_) => Self::new(),
        }
    }

    /// Writes atomically (temp file then rename) so a crash never leaves a torn file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }
}

pub const BONUS_CAP: i32 = 1500;

/// See [`Frecency::bonus`].
pub fn bonus_for(score: f64) -> i32 {
    if score <= 0.0 {
        return 0;
    }
    ((1.0 + score).ln() * 450.0).min(BONUS_CAP as f64) as i32
}

/// `$XDG_STATE_HOME/aurora/launcher.json`, else `~/.local/state/aurora/launcher.json`.
pub fn state_path() -> Option<PathBuf> {
    state_path_from(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
    )
}

pub fn state_path_from(
    xdg_state: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let base = match xdg_state.filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home.filter(|v| !v.is_empty())?)
            .join(".local")
            .join("state"),
    };
    Some(base.join("aurora").join("launcher.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 3600;

    #[test]
    fn a_launch_scores_one_and_then_decays_by_half_lives() {
        let mut f = Frecency::new();
        f.record_launch("a", 1000);
        assert!((f.score("a", 1000) - 1.0).abs() < 1e-9);
        let later = 1000 + 14 * DAY;
        assert!((f.score("a", later) - 0.5).abs() < 1e-6);
        assert!((f.score("a", 1000 + 28 * DAY) - 0.25).abs() < 1e-6);
        assert_eq!(f.score("unknown", 5), 0.0);
    }

    #[test]
    fn repeated_launches_accumulate_on_the_decayed_value() {
        let mut f = Frecency::new();
        f.record_launch("a", 0);
        f.record_launch("a", 14 * DAY);
        assert!((f.score("a", 14 * DAY) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn frequent_old_use_can_outrank_one_fresh_launch() {
        let mut f = Frecency::new();
        for i in 0..10 {
            f.record_launch("habit", i * 3600);
        }
        f.record_launch("once", 7 * DAY);
        assert!(f.score("habit", 7 * DAY) > f.score("once", 7 * DAY));
        // ... but only for so long.
        assert!(f.score("habit", 120 * DAY)  < 0.1);
    }

    #[test]
    fn clock_going_backwards_does_not_inflate() {
        let mut f = Frecency::new();
        f.record_launch("a", 1000);
        assert!((f.score("a", 10) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn bonus_is_monotonic_and_capped() {
        assert_eq!(bonus_for(0.0), 0);
        assert!(bonus_for(1.0) > 0);
        assert!(bonus_for(5.0) > bonus_for(1.0));
        assert_eq!(bonus_for(1e9), BONUS_CAP);
    }

    #[test]
    fn prune_drops_faded_entries_and_caps_the_count() {
        let mut f = Frecency::new();
        f.record_launch("old", 0);
        f.record_launch("new", 200 * DAY);
        f.prune(200 * DAY);
        assert_eq!(f.len(), 1);
        assert!(f.score("new", 200 * DAY) > 0.9);

        let mut big = Frecency::new();
        for i in 0..MAX_ENTRIES + 50 {
            big.record_launch(&format!("app{i}"), 100);
        }
        big.record_launch("app0", 100);
        big.prune(100);
        assert_eq!(big.len(), MAX_ENTRIES);
        assert!(big.score("app0", 100) > 1.5);
    }

    #[test]
    fn save_and_load_round_trip_and_corruption_is_tolerated() {
        let dir = std::env::temp_dir().join(format!("aurora-launcher-test-{}", std::process::id()));
        let path = dir.join("nested").join("launcher.json");
        let mut f = Frecency::new();
        f.record_launch("firefox", 42);
        f.save(&path).unwrap();
        let back = Frecency::load(&path);
        assert_eq!(back, f);
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(Frecency::load(&path).is_empty());
        assert!(Frecency::load(&dir.join("missing.json")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_path_follows_xdg() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        assert_eq!(
            state_path_from(os("/s"), os("/h")),
            Some(PathBuf::from("/s/aurora/launcher.json"))
        );
        assert_eq!(
            state_path_from(None, os("/h")),
            Some(PathBuf::from("/h/.local/state/aurora/launcher.json"))
        );
        assert_eq!(state_path_from(os(""), None), None);
    }
}
