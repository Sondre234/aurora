//! The pure model of one window: directory entries, sorting, filtering, selection and
//! navigation history. No I/O and no Wayland types; every transition is a plain method so it
//! can be tested without a connection.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// What an entry is on disk (symlinks are not followed for this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Dir,
    File,
    /// A symlink whose target exists.
    Symlink,
    /// A symlink whose target does not exist.
    Broken,
    /// Sockets, fifos, devices.
    Other,
}

/// One row of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: OsString,
    pub kind: Kind,
    /// A directory, or a symlink to one: Enter navigates into it.
    pub is_dir: bool,
    /// Bytes (the target's for a symlink); 0 for directories.
    pub size: u64,
    /// Seconds since the epoch of the last modification; 0 when unknown.
    pub mtime: i64,
    pub mode: u32,
    pub hidden: bool,
}

impl Entry {
    pub fn new(name: impl Into<OsString>, kind: Kind) -> Self {
        let name = name.into();
        Self {
            hidden: is_hidden(&name),
            is_dir: kind == Kind::Dir,
            name,
            kind,
            size: 0,
            mtime: 0,
            mode: 0,
        }
    }

    pub fn display_name(&self) -> String {
        self.name.to_string_lossy().into_owned()
    }

    pub fn executable(&self) -> bool {
        !self.is_dir && self.mode & 0o111 != 0
    }

    /// Lower-case extension without the dot ("" when none).
    pub fn extension(&self) -> String {
        let (_, ext) = crate::names::split_ext(self.name.as_bytes());
        String::from_utf8_lossy(ext.get(1..).unwrap_or(&[])).to_lowercase()
    }

    /// Human label of the type, also the key of the "kind" sort.
    pub fn type_label(&self) -> String {
        match self.kind {
            Kind::Dir => "Folder".into(),
            Kind::Broken => "Broken link".into(),
            _ if self.is_dir => "Link to folder".into(),
            Kind::Other => "Special file".into(),
            _ => match self.extension().as_str() {
                "" => "File".into(),
                ext => format!("{} file", ext.to_uppercase()),
            },
        }
    }
}

/// Dot files are hidden.
pub fn is_hidden(name: &OsStr) -> bool {
    name.as_bytes().first() == Some(&b'.')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            descending: false,
        }
    }
}

impl Sort {
    /// The sort after choosing `key`: the same key flips the direction, a new one starts
    /// ascending (newest and largest first would surprise nobody, but consistency wins).
    pub fn choose(self, key: SortKey) -> Sort {
        if self.key == key {
            Sort {
                key,
                descending: !self.descending,
            }
        } else {
            Sort {
                key,
                descending: false,
            }
        }
    }
}

/// Natural, case-insensitive order: digit runs compare by value, so `file2` < `file10`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let run = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(&c) = it.peek() {
                        if !c.is_ascii_digit() {
                            break;
                        }
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (ra, rb) = (run(&mut a), run(&mut b));
                let (ta, tb) = (ra.trim_start_matches('0'), rb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let (lx, ly) = (fold(x), fold(y));
                if lx != ly {
                    return lx.cmp(&ly);
                }
                a.next();
                b.next();
            }
        }
    }
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Order of two entries: directories first, then the chosen key, then the raw name so the
/// result is deterministic.
pub fn cmp_entries(a: &Entry, b: &Entry, sort: Sort) -> Ordering {
    match (a.is_dir, b.is_dir) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let by_name = || natural_cmp(&a.name.to_string_lossy(), &b.name.to_string_lossy());
    let ord = match sort.key {
        SortKey::Name => by_name(),
        SortKey::Size if a.is_dir => by_name(),
        SortKey::Size => a.size.cmp(&b.size).then_with(by_name),
        SortKey::Modified => a.mtime.cmp(&b.mtime).then_with(by_name),
        SortKey::Kind => a
            .type_label()
            .to_lowercase()
            .cmp(&b.type_label().to_lowercase())
            .then_with(by_name),
    };
    let ord = if sort.descending { ord.reverse() } else { ord };
    ord.then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
}

pub fn sort_entries(entries: &mut [Entry], sort: Sort) {
    entries.sort_by(|a, b| cmp_entries(a, b, sort));
}

/// Case-insensitive substring match; an empty filter matches everything.
pub fn matches_filter(name: &str, filter_lower: &str) -> bool {
    filter_lower.is_empty() || name.to_lowercase().contains(filter_lower)
}

/// "1.5 MiB" style size, 1024 based.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// `YYYY-MM-DD HH:MM` in local time; empty when unknown.
pub fn format_mtime(secs: i64) -> String {
    if secs == 0 {
        return String::new();
    }
    let t = crate::trash::LocalTime::from_unix(secs);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.min
    )
}

/// Back/forward history of directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    current: PathBuf,
}

impl History {
    pub fn new(start: PathBuf) -> Self {
        Self {
            back: Vec::new(),
            forward: Vec::new(),
            current: start,
        }
    }

    pub fn current(&self) -> &Path {
        &self.current
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Navigate somewhere new: forward history is dropped. False when already there.
    pub fn visit(&mut self, path: PathBuf) -> bool {
        if path == self.current {
            return false;
        }
        let prev = std::mem::replace(&mut self.current, path);
        self.back.push(prev);
        self.forward.clear();
        true
    }

    pub fn back(&mut self) -> Option<&Path> {
        let prev = self.back.pop()?;
        let cur = std::mem::replace(&mut self.current, prev);
        self.forward.push(cur);
        Some(&self.current)
    }

    pub fn forward(&mut self) -> Option<&Path> {
        let next = self.forward.pop()?;
        let cur = std::mem::replace(&mut self.current, next);
        self.back.push(cur);
        Some(&self.current)
    }
}

/// Which entries are selected (by name, so a refresh or a re-sort keeps them), plus the
/// keyboard cursor and the anchor of range selections, both indices into the visible list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    set: HashSet<OsString>,
    cursor: Option<usize>,
    anchor: Option<usize>,
}

/// Name of the visible row `i`.
pub type NameAt<'a> = &'a dyn Fn(usize) -> OsString;

impl Selection {
    pub fn count(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub fn contains(&self, name: &OsStr) -> bool {
        self.set.contains(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &OsString> {
        self.set.iter()
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    pub fn clear(&mut self) {
        self.set.clear();
    }

    /// Cursor and anchor move without changing what is selected.
    pub fn set_cursor(&mut self, i: Option<usize>) {
        self.cursor = i;
        self.anchor = i;
    }

    /// Plain click: only row `i` is selected.
    pub fn select_only(&mut self, i: usize, name: OsString) {
        self.set.clear();
        self.set.insert(name);
        self.cursor = Some(i);
        self.anchor = Some(i);
    }

    /// Ctrl-click: flip row `i`.
    pub fn toggle(&mut self, i: usize, name: OsString) {
        if !self.set.remove(&name) {
            self.set.insert(name);
        }
        self.cursor = Some(i);
        self.anchor = Some(i);
    }

    /// Shift-click or Shift-arrow: select from the anchor to `i` inclusive. With
    /// `additive` (Ctrl+Shift) the existing selection stays. The anchor does not move.
    pub fn extend(&mut self, i: usize, additive: bool, name_at: NameAt) {
        let anchor = self.anchor.or(self.cursor).unwrap_or(i);
        if !additive {
            self.set.clear();
        }
        let (lo, hi) = (anchor.min(i), anchor.max(i));
        self.set.extend((lo..=hi).map(name_at));
        self.cursor = Some(i);
        self.anchor = Some(anchor);
    }

    pub fn select_all(&mut self, len: usize, name_at: NameAt) {
        self.set = (0..len).map(name_at).collect();
        if len > 0 && self.cursor.is_none() {
            self.cursor = Some(0);
            self.anchor = Some(0);
        }
    }

    pub fn invert(&mut self, len: usize, name_at: NameAt) {
        self.set = (0..len)
            .map(name_at)
            .filter(|n| !self.set.contains(n))
            .collect();
    }

    /// Re-targets after the visible list changed: names that are gone are dropped and the
    /// cursor is found again by `cursor_name` (or clamped).
    pub fn rebase(&mut self, visible: &[OsString], cursor_name: Option<&OsStr>) {
        let present: HashSet<&OsString> = visible.iter().collect();
        self.set.retain(|n| present.contains(n));
        let found = cursor_name.and_then(|c| visible.iter().position(|n| n == c));
        self.cursor = match (found, self.cursor) {
            (Some(i), _) => Some(i),
            (None, Some(i)) if !visible.is_empty() => Some(i.min(visible.len() - 1)),
            _ => None,
        };
        self.anchor = self.cursor;
    }
}

/// First visible row at or after `from` (wrapping) whose name starts with `prefix`
/// (case-insensitive).
pub fn typeahead(names: &[OsString], prefix: &str, from: usize) -> Option<usize> {
    if prefix.is_empty() || names.is_empty() {
        return None;
    }
    let p = prefix.to_lowercase();
    let n = names.len();
    (0..n)
        .map(|k| (from + k) % n)
        .find(|&i| names[i].to_string_lossy().to_lowercase().starts_with(&p))
}

/// The state of the file list of one window.
#[derive(Debug, Clone)]
pub struct Browser {
    pub history: History,
    /// All entries of the directory, sorted by `sort`.
    pub entries: Vec<Entry>,
    /// Indices into `entries` that pass the hidden and filter settings.
    pub visible: Vec<u32>,
    pub show_hidden: bool,
    /// Lower-cased live filter ("" = none).
    pub filter: String,
    pub sort: Sort,
    pub selection: Selection,
    /// Why the directory cannot be shown, if it cannot.
    pub error: Option<String>,
}

impl Browser {
    pub fn new(start: PathBuf) -> Self {
        Self {
            history: History::new(start),
            entries: Vec::new(),
            visible: Vec::new(),
            show_hidden: false,
            filter: String::new(),
            sort: Sort::default(),
            selection: Selection::default(),
            error: None,
        }
    }

    pub fn path(&self) -> &Path {
        self.history.current()
    }

    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    pub fn entry(&self, row: usize) -> Option<&Entry> {
        self.visible
            .get(row)
            .and_then(|&i| self.entries.get(i as usize))
    }

    pub fn name_at(&self, row: usize) -> OsString {
        self.entry(row).map(|e| e.name.clone()).unwrap_or_default()
    }

    pub fn visible_names(&self) -> Vec<OsString> {
        (0..self.len()).map(|r| self.name_at(r)).collect()
    }

    pub fn row_of(&self, name: &OsStr) -> Option<usize> {
        (0..self.len()).find(|&r| self.entry(r).is_some_and(|e| e.name == name))
    }

    pub fn cursor_entry(&self) -> Option<&Entry> {
        self.selection.cursor().and_then(|r| self.entry(r))
    }

    /// Full paths of the selected entries in list order; the cursor row when nothing is
    /// selected is NOT implied (callers decide).
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        (0..self.len())
            .filter_map(|r| self.entry(r))
            .filter(|e| self.selection.contains(&e.name))
            .map(|e| self.path().join(&e.name))
            .collect()
    }

    fn rebuild_visible(&mut self) {
        let f = self.filter.clone();
        let hidden = self.show_hidden;
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| (hidden || !e.hidden) && matches_filter(&e.display_name(), &f))
            .map(|(i, _)| i as u32)
            .collect();
    }

    fn rebase_selection(&mut self, cursor_name: Option<OsString>) {
        let names = self.visible_names();
        self.selection.rebase(&names, cursor_name.as_deref());
    }

    /// Installs a freshly read, already sorted listing. The selection keeps whatever still
    /// exists.
    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        let cursor = self.cursor_entry().map(|e| e.name.clone());
        self.entries = entries;
        self.error = None;
        self.rebuild_visible();
        self.rebase_selection(cursor);
    }

    /// Shows an error instead of entries.
    pub fn set_error(&mut self, message: String) {
        self.entries.clear();
        self.visible.clear();
        self.selection = Selection::default();
        self.error = Some(message);
    }

    /// A new directory: selection and filter reset.
    pub fn navigate_reset(&mut self) {
        self.entries.clear();
        self.visible.clear();
        self.selection = Selection::default();
        self.filter.clear();
        self.error = None;
    }

    pub fn set_filter(&mut self, text: &str) {
        let cursor = self.cursor_entry().map(|e| e.name.clone());
        self.filter = text.to_lowercase();
        self.rebuild_visible();
        self.rebase_selection(cursor);
    }

    pub fn toggle_hidden(&mut self) {
        let cursor = self.cursor_entry().map(|e| e.name.clone());
        self.show_hidden = !self.show_hidden;
        self.rebuild_visible();
        self.rebase_selection(cursor);
    }

    /// Re-sorts in place (small directories; large ones are sorted on the worker and come
    /// back through [`Browser::set_entries`]).
    pub fn resort(&mut self, sort: Sort) {
        self.sort = sort;
        let cursor = self.cursor_entry().map(|e| e.name.clone());
        sort_entries(&mut self.entries, sort);
        self.rebuild_visible();
        self.rebase_selection(cursor);
    }

    // Selection gestures on visible rows.

    /// Runs `f` on the selection with a name lookup of the visible rows. The selection is
    /// moved out for the duration so the lookup can borrow the entries.
    fn with_selection<R>(&mut self, f: impl FnOnce(&mut Selection, NameAt) -> R) -> R {
        let mut sel = std::mem::take(&mut self.selection);
        let out = f(&mut sel, &|i| self.name_at(i));
        self.selection = sel;
        out
    }

    pub fn click(&mut self, row: usize, ctrl: bool, shift: bool) {
        if row >= self.len() {
            return;
        }
        self.with_selection(|sel, at| {
            if shift {
                sel.extend(row, ctrl, at);
            } else if ctrl {
                sel.toggle(row, at(row));
            } else {
                sel.select_only(row, at(row));
            }
        });
    }

    /// Moves the cursor by `delta` rows (clamped). With `shift` the range from the anchor
    /// is selected, else only the new row.
    pub fn move_cursor(&mut self, delta: i64, shift: bool) {
        if self.is_empty() {
            return;
        }
        let from = self
            .selection
            .cursor()
            .map_or(if delta < 0 { self.len() as i64 } else { -1 }, |c| c as i64);
        let to = (from + delta).clamp(0, self.len() as i64 - 1) as usize;
        self.go_to_row(to, shift);
    }

    pub fn go_to_row(&mut self, row: usize, shift: bool) {
        if row >= self.len() {
            return;
        }
        self.with_selection(|sel, at| {
            if shift {
                sel.extend(row, false, at);
            } else {
                sel.select_only(row, at(row));
            }
        });
    }

    pub fn select_all(&mut self) {
        let len = self.len();
        self.with_selection(|sel, at| sel.select_all(len, at));
    }

    pub fn invert_selection(&mut self) {
        let len = self.len();
        self.with_selection(|sel, at| sel.invert(len, at));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64, mtime: i64) -> Entry {
        let mut e = Entry::new(name, Kind::File);
        e.size = size;
        e.mtime = mtime;
        e
    }

    fn dir(name: &str) -> Entry {
        Entry::new(name, Kind::Dir)
    }

    fn names(b: &Browser) -> Vec<String> {
        (0..b.len())
            .map(|r| b.entry(r).unwrap().display_name())
            .collect()
    }

    fn browser(mut entries: Vec<Entry>) -> Browser {
        let mut b = Browser::new(PathBuf::from("/x"));
        sort_entries(&mut entries, b.sort);
        b.set_entries(entries);
        b
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["file10", "File2", "file1", "file02", "a", "B", "file1b"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            ["a", "B", "file1", "file1b", "File2", "file02", "file10"]
        );
        assert_eq!(natural_cmp("a1", "a01"), Ordering::Equal);
        assert_eq!(
            natural_cmp("x99999999999999999999", "x100000000000000000000"),
            Ordering::Less
        );
        assert_eq!(natural_cmp("é", "f"), Ordering::Greater);
        assert_eq!(natural_cmp("", "a"), Ordering::Less);
    }

    #[test]
    fn directories_first_in_every_mode() {
        let es = vec![
            file("a", 5, 10),
            dir("z"),
            file("b", 1, 30),
            dir("m"),
            file("c", 9, 20),
        ];
        let sorted = |key, descending| {
            let mut v = es.clone();
            sort_entries(&mut v, Sort { key, descending });
            v.iter().map(Entry::display_name).collect::<Vec<_>>()
        };
        assert_eq!(sorted(SortKey::Name, false), ["m", "z", "a", "b", "c"]);
        assert_eq!(sorted(SortKey::Name, true), ["z", "m", "c", "b", "a"]);
        assert_eq!(sorted(SortKey::Size, false), ["m", "z", "b", "a", "c"]);
        assert_eq!(sorted(SortKey::Size, true), ["z", "m", "c", "a", "b"]);
        assert_eq!(sorted(SortKey::Modified, false), ["m", "z", "a", "c", "b"]);
        assert_eq!(sorted(SortKey::Modified, true), ["z", "m", "b", "c", "a"]);
    }

    #[test]
    fn kind_sort_groups_by_extension() {
        let mut v = vec![
            file("a.txt", 0, 0),
            file("b.rs", 0, 0),
            file("c.txt", 0, 0),
            file("d", 0, 0),
        ];
        sort_entries(
            &mut v,
            Sort {
                key: SortKey::Kind,
                descending: false,
            },
        );
        let n: Vec<_> = v.iter().map(Entry::display_name).collect();
        assert_eq!(n, ["d", "b.rs", "a.txt", "c.txt"]);
        assert_eq!(v[1].type_label(), "RS file");
    }

    #[test]
    fn sort_choice_flips_or_resets() {
        let s = Sort::default();
        let s = s.choose(SortKey::Name);
        assert!(s.descending);
        let s = s.choose(SortKey::Size);
        assert_eq!((s.key, s.descending), (SortKey::Size, false));
    }

    #[test]
    fn hidden_and_filter() {
        let mut b = browser(vec![
            file(".hid", 0, 0),
            file("Alpha", 0, 0),
            file("beta", 0, 0),
        ]);
        assert_eq!(names(&b), ["Alpha", "beta"]);
        b.toggle_hidden();
        assert_eq!(names(&b), [".hid", "Alpha", "beta"]);
        b.set_filter("ALP");
        assert_eq!(names(&b), ["Alpha"]);
        b.set_filter("");
        b.toggle_hidden();
        assert_eq!(names(&b), ["Alpha", "beta"]);
    }

    #[test]
    fn click_selection_algebra() {
        let mut b = browser((0..6).map(|i| file(&format!("f{i}"), 0, 0)).collect());
        b.click(1, false, false);
        assert_eq!(b.selection.count(), 1);
        b.click(3, true, false);
        assert_eq!(b.selection.count(), 2);
        b.click(3, true, false);
        assert_eq!(b.selection.count(), 1);
        // Shift-range from the last plain/ctrl click (anchor = row 3).
        b.click(5, false, true);
        let sel = |b: &Browser| {
            b.selected_paths()
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(sel(&b), ["f3", "f4", "f5"].map(String::from));
        b.click(1, false, true);
        assert_eq!(sel(&b), ["f1", "f2", "f3"].map(String::from));
        b.click(0, true, true);
        assert_eq!(sel(&b), ["f0", "f1", "f2", "f3"].map(String::from));
        b.invert_selection();
        assert_eq!(sel(&b), ["f4", "f5"].map(String::from));
        b.select_all();
        assert_eq!(b.selection.count(), 6);
        b.click(99, false, false);
        assert_eq!(b.selection.count(), 6, "out of range click is ignored");
    }

    #[test]
    fn cursor_movement() {
        let mut b = browser((0..4).map(|i| file(&format!("f{i}"), 0, 0)).collect());
        b.move_cursor(1, false);
        assert_eq!(b.selection.cursor(), Some(0));
        b.move_cursor(2, false);
        assert_eq!(b.selection.cursor(), Some(2));
        b.move_cursor(10, true);
        assert_eq!(b.selection.cursor(), Some(3));
        assert_eq!(b.selection.count(), 2, "range from the anchor");
        b.move_cursor(-100, false);
        assert_eq!(b.selection.cursor(), Some(0));
        assert_eq!(b.selection.count(), 1);
        let mut empty = browser(vec![]);
        empty.move_cursor(1, false);
        assert_eq!(empty.selection.cursor(), None);
        // Up with no cursor starts from the bottom.
        let mut c = browser((0..3).map(|i| file(&format!("f{i}"), 0, 0)).collect());
        c.move_cursor(-1, false);
        assert_eq!(c.selection.cursor(), Some(2));
    }

    #[test]
    fn selection_survives_refresh_and_resort() {
        let mut b = browser(vec![file("a", 3, 0), file("b", 1, 0), file("c", 2, 0)]);
        b.click(1, false, false); // "b"
        b.click(2, true, false); // + "c"
        b.resort(Sort {
            key: SortKey::Size,
            descending: false,
        });
        assert_eq!(names(&b), ["b", "c", "a"]);
        assert_eq!(b.selection.count(), 2);
        assert_eq!(b.selection.cursor(), Some(1), "cursor stays on c");
        // "c" disappears: the selection shrinks, the cursor clamps.
        let mut es = vec![file("a", 3, 0), file("b", 1, 0)];
        sort_entries(&mut es, b.sort);
        b.set_entries(es);
        assert_eq!(b.selection.count(), 1);
        assert!(b.selection.contains(OsStr::new("b")));
        assert_eq!(b.selection.cursor(), Some(1));
        b.set_error("denied".into());
        assert!(b.is_empty() && b.selection.is_empty());
        assert_eq!(b.error.as_deref(), Some("denied"));
    }

    #[test]
    fn history_back_forward() {
        let p = |s: &str| PathBuf::from(s);
        let mut h = History::new(p("/a"));
        assert!(!h.visit(p("/a")));
        assert!(h.visit(p("/b")));
        assert!(h.visit(p("/c")));
        assert_eq!(h.back(), Some(p("/b").as_path()));
        assert_eq!(h.back(), Some(p("/a").as_path()));
        assert_eq!(h.back(), None);
        assert!(h.can_forward() && !h.can_back());
        assert_eq!(h.forward(), Some(p("/b").as_path()));
        assert!(h.visit(p("/d")), "a new visit drops forward history");
        assert!(!h.can_forward());
        assert_eq!(h.current(), p("/d"));
    }

    #[test]
    fn typeahead_wraps_and_ignores_case() {
        let n: Vec<OsString> = ["alpha", "Beta", "bravo", "charlie"]
            .map(OsString::from)
            .to_vec();
        assert_eq!(typeahead(&n, "b", 0), Some(1));
        assert_eq!(typeahead(&n, "b", 2), Some(2));
        assert_eq!(typeahead(&n, "br", 0), Some(2));
        assert_eq!(typeahead(&n, "a", 1), Some(0), "wraps around");
        assert_eq!(typeahead(&n, "z", 0), None);
        assert_eq!(typeahead(&n, "", 0), None);
    }

    #[test]
    fn sizes_read_well() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_size(300 * 1024 * 1024), "300 MiB");
        assert_eq!(format_size(u64::MAX), "16384 PiB");
    }

    #[test]
    fn entry_helpers() {
        assert!(is_hidden(OsStr::new(".x")));
        assert!(!is_hidden(OsStr::new("x.")));
        assert_eq!(file("Photo.JPG", 0, 0).extension(), "jpg");
        assert_eq!(file(".bashrc", 0, 0).extension(), "");
        assert_eq!(dir("d").type_label(), "Folder");
        let mut x = file("run", 0, 0);
        x.mode = 0o755;
        assert!(x.executable());
        assert!(!dir("d").executable());
    }
}
