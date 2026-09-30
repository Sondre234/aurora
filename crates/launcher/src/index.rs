//! The in-memory app index: scan, diff and search.
//!
//! Built once at startup from the XDG `applications` directories and rebuilt (off the UI
//! thread) when they change; the daemon swaps the new [`Index`] in whole, so a keystroke
//! never waits on disk (performance.md rules 4 and 5). Owner: the launcher; invalidated by
//! the directory watcher; size is one small struct per app, no budget needed.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use freedesktop_desktop_entry::DesktopEntry;

use crate::entry::{AppEntry, Env, app_from_desktop};
use crate::frecency::Frecency;
use crate::fuzzy::{Match, fuzzy_match};

/// Applications nested deeper than this below an `applications` directory are ignored.
const MAX_DEPTH: usize = 4;
/// A match on anything but the name scores this much less.
const SECONDARY_PENALTY: i32 = 700;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    apps: Vec<AppEntry>,
}

/// What changed between two indexes, as sorted desktop ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl IndexDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// One search result: the app's position in [`Index::apps`], its rank score and the
/// matched char positions in its name (empty when it matched through another field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub app: usize,
    pub score: i32,
    pub positions: Vec<usize>,
}

impl Index {
    /// Builds an index from already-filtered apps: sorted by name, unique ids (the first
    /// occurrence wins).
    pub fn from_apps(mut apps: Vec<AppEntry>) -> Self {
        let mut seen = HashSet::new();
        apps.retain(|a| seen.insert(a.id.clone()));
        apps.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        Self { apps }
    }

    pub fn apps(&self) -> &[AppEntry] {
        &self.apps
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    pub fn get(&self, i: usize) -> Option<&AppEntry> {
        self.apps.get(i)
    }

    /// Ids added, removed and changed going from `self` to `new`.
    pub fn diff(&self, new: &Index) -> IndexDiff {
        let old: BTreeMap<&str, &AppEntry> = self.apps.iter().map(|a| (a.id.as_str(), a)).collect();
        let cur: BTreeMap<&str, &AppEntry> = new.apps.iter().map(|a| (a.id.as_str(), a)).collect();
        let mut d = IndexDiff::default();
        for (id, app) in &cur {
            match old.get(id) {
                None => d.added.push(id.to_string()),
                Some(prev) if prev != app => d.changed.push(id.to_string()),
                Some(_) => {}
            }
        }
        d.removed = old
            .keys()
            .filter(|id| !cur.contains_key(*id))
            .map(|id| id.to_string())
            .collect();
        d
    }

    /// Ranks apps for `query`, best first, at most `limit`. Each app is scored by its best
    /// field (name, then generic name, keywords and the id's last segment at a penalty);
    /// frecency is added on top. An empty query lists by frecency, then name.
    pub fn search(&self, query: &str, frecency: &Frecency, now: u64, limit: usize) -> Vec<Hit> {
        let mut hits: Vec<Hit> = Vec::new();
        for (i, app) in self.apps.iter().enumerate() {
            let Some(m) = best_match(query, app) else {
                continue;
            };
            hits.push(Hit {
                app: i,
                score: m.score.saturating_add(frecency.bonus(&app.id, now)),
                positions: m.positions,
            });
        }
        // Stable: equal scores keep the index's name order.
        hits.sort_by_key(|h| std::cmp::Reverse(h.score));
        hits.truncate(limit);
        hits
    }
}

fn best_match(query: &str, app: &AppEntry) -> Option<Match> {
    if query.trim().is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    let mut best = fuzzy_match(query, &app.name);
    let mut consider = |text: &str| {
        if let Some(mut m) = fuzzy_match(query, text) {
            m.score -= SECONDARY_PENALTY;
            m.positions.clear();
            if best.as_ref().is_none_or(|b| m.score > b.score) {
                best = Some(m);
            }
        }
    };
    if let Some(g) = &app.generic_name {
        consider(g);
    }
    for k in &app.keywords {
        consider(k);
    }
    consider(app.id.rsplit('.').next().unwrap_or(&app.id));
    best
}

/// Scans `dirs` (highest priority first) into an index. Ids follow the spec: the path
/// below the `applications` directory with `/` turned into `-`. The first directory that
/// provides an id wins, even when its entry is hidden, so a user override can remove a
/// system entry. Unreadable files are skipped.
pub fn scan(dirs: &[PathBuf], env: &Env) -> Index {
    let mut seen: HashSet<String> = HashSet::new();
    let mut apps = Vec::new();
    for dir in dirs {
        let mut files = Vec::new();
        collect_desktop_files(dir, dir, 0, &mut files);
        files.sort();
        for (id, path) in files {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Ok(de) = DesktopEntry::from_path(&path, Some(&env.locales)) else {
                continue;
            };
            if let Some(app) = app_from_desktop(&id, &de, env) {
                apps.push(app);
            }
        }
    }
    Index::from_apps(apps)
}

fn collect_desktop_files(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for item in read.flatten() {
        let path = item.path();
        // `metadata` follows symlinks (profile-based installs link their entries).
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_desktop_files(root, &path, depth + 1, out);
        } else if path.extension().is_some_and(|e| e == "desktop")
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push((desktop_id(rel), path));
        }
    }
}

/// `kde/foo.desktop` -> `kde-foo`.
pub fn desktop_id(rel: &Path) -> String {
    let s = rel.to_string_lossy();
    let s = s.strip_suffix(".desktop").unwrap_or(&s);
    s.replace('/', "-")
}

/// The directories to scan, highest priority first: `$XDG_DATA_HOME/applications` then each
/// of `$XDG_DATA_DIRS`, with the spec's defaults.
pub fn application_dirs(
    data_home: Option<&str>,
    home: Option<&str>,
    data_dirs: Option<&str>,
) -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    match data_home.filter(|s| !s.is_empty()) {
        Some(h) => bases.push(h.into()),
        None => {
            if let Some(h) = home.filter(|s| !s.is_empty()) {
                bases.push(Path::new(h).join(".local/share"));
            }
        }
    }
    let dirs = data_dirs
        .filter(|s| !s.is_empty())
        .unwrap_or("/usr/local/share:/usr/share");
    bases.extend(dirs.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    let mut out: Vec<PathBuf> = Vec::new();
    for b in bases {
        let d = b.join("applications");
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str) -> AppEntry {
        AppEntry {
            id: id.into(),
            name: name.into(),
            generic_name: None,
            comment: None,
            keywords: Vec::new(),
            icon: None,
            argv: vec![id.into()],
            terminal: false,
        }
    }

    fn index(apps: Vec<AppEntry>) -> Index {
        Index::from_apps(apps)
    }

    #[test]
    fn from_apps_sorts_by_name_and_dedupes_ids() {
        let i = index(vec![
            app("b", "banana"),
            app("a", "Apple"),
            app("a", "Duplicate"),
            app("c", "cherry"),
        ]);
        let names: Vec<&str> = i.apps().iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Apple", "banana", "cherry"]);
    }

    #[test]
    fn diff_reports_added_removed_and_changed() {
        let old = index(vec![app("a", "A"), app("b", "B"), app("c", "C")]);
        let mut changed_b = app("b", "B");
        changed_b.argv = vec!["b2".into()];
        let new = index(vec![changed_b, app("c", "C"), app("d", "D")]);
        let d = old.diff(&new);
        assert_eq!(d.added, ["d"]);
        assert_eq!(d.removed, ["a"]);
        assert_eq!(d.changed, ["b"]);
        assert!(old.diff(&old.clone()).is_empty());
        assert!(!d.is_empty());
    }

    #[test]
    fn search_ranks_the_name_match_first_and_reports_positions() {
        let i = index(vec![
            app("org.mozilla.firefox", "Firefox"),
            app("fire", "Fantasy Interface Reader Editor"),
            app("term", "Terminal"),
        ]);
        let hits = i.search("fire", &Frecency::new(), 0, 10);
        assert_eq!(i.get(hits[0].app).unwrap().name, "Firefox");
        assert_eq!(hits[0].positions, vec![0, 1, 2, 3]);
        assert!(
            hits.iter()
                .all(|h| i.get(h.app).unwrap().name != "Terminal")
        );
    }

    #[test]
    fn keywords_and_generic_names_match_without_highlight() {
        let mut browser = app("fx", "Zeta");
        browser.generic_name = Some("Web Browser".into());
        browser.keywords = vec!["internet".into()];
        let i = index(vec![browser, app("other", "Other")]);
        let hits = i.search("browser", &Frecency::new(), 0, 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].positions.is_empty());
        assert_eq!(i.search("internet", &Frecency::new(), 0, 10).len(), 1);
    }

    #[test]
    fn empty_query_lists_by_frecency_then_name() {
        let i = index(vec![
            app("a", "Alpha"),
            app("b", "Bravo"),
            app("c", "Charlie"),
        ]);
        let mut f = Frecency::new();
        f.record_launch("c", 100);
        f.record_launch("c", 101);
        f.record_launch("b", 102);
        let hits = i.search("", &f, 103, 10);
        let names: Vec<&str> = hits
            .iter()
            .map(|h| i.get(h.app).unwrap().name.as_str())
            .collect();
        assert_eq!(names, ["Charlie", "Bravo", "Alpha"]);
        assert_eq!(i.search("", &f, 103, 2).len(), 2);
    }

    #[test]
    fn frecency_breaks_ties_between_equal_matches() {
        let i = index(vec![app("a", "Editor One"), app("b", "Editor Two")]);
        let mut f = Frecency::new();
        f.record_launch("b", 10);
        let hits = i.search("edit", &f, 11, 10);
        assert_eq!(i.get(hits[0].app).unwrap().id, "b");
    }

    #[test]
    fn desktop_ids_follow_the_spec() {
        assert_eq!(
            desktop_id(Path::new("org.gnome.Nautilus.desktop")),
            "org.gnome.Nautilus"
        );
        assert_eq!(desktop_id(Path::new("kde/foo.desktop")), "kde-foo");
    }

    #[test]
    fn application_dirs_follow_the_xdg_defaults() {
        assert_eq!(
            application_dirs(None, Some("/home/u"), None),
            [
                PathBuf::from("/home/u/.local/share/applications"),
                PathBuf::from("/usr/local/share/applications"),
                PathBuf::from("/usr/share/applications"),
            ]
        );
        assert_eq!(
            application_dirs(Some("/d"), Some("/home/u"), Some("/a:/b:/a")),
            [
                PathBuf::from("/d/applications"),
                PathBuf::from("/a/applications"),
                PathBuf::from("/b/applications"),
            ]
        );
    }

    #[test]
    fn scan_reads_a_directory_and_a_user_override_hides_a_system_entry() {
        let root =
            std::env::temp_dir().join(format!("aurora-launcher-scan-{}", std::process::id()));
        let user = root.join("user");
        let system = root.join("system");
        std::fs::create_dir_all(user.join("sub")).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        let entry = |name: &str| {
            format!(
                "[Desktop Entry]\nType=Application\nName={name}\nExec={}\n",
                name.to_lowercase()
            )
        };
        std::fs::write(system.join("one.desktop"), entry("One")).unwrap();
        std::fs::write(system.join("two.desktop"), entry("Two")).unwrap();
        std::fs::write(system.join("readme.txt"), "x").unwrap();
        std::fs::write(
            user.join("two.desktop"),
            "[Desktop Entry]\nType=Application\nName=Two\nExec=two\nHidden=true\n",
        )
        .unwrap();
        std::fs::write(user.join("sub").join("three.desktop"), entry("Three")).unwrap();
        let env = Env {
            desktops: Vec::new(),
            locales: Vec::new(),
            exists: &|_| true,
        };
        let idx = scan(&[user, system], &env);
        let ids: Vec<&str> = idx.apps().iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["one", "sub-three"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
