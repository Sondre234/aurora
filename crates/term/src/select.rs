//! Pointer-to-selection logic that needs no emulator: click counting, selection kinds
//! and the best-effort modifier state. Pure, times are passed in as milliseconds.

/// What a left press starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelKind {
    /// Drag between cells.
    Simple,
    /// Double click: whole word.
    Word,
    /// Triple click: whole line.
    Line,
}

/// Two presses within this many milliseconds on the same cell count as a multi-click.
pub const MULTI_CLICK_MS: u64 = 400;

/// Counts consecutive left clicks on one cell: 1, 2, 3, then 1 again.
#[derive(Debug, Default)]
pub struct ClickTracker {
    last: Option<(u64, usize, usize)>,
    count: u8,
}

impl ClickTracker {
    /// Register a press at `now_ms` on cell (`col`, `row`); the new click count.
    pub fn press(&mut self, now_ms: u64, col: usize, row: usize) -> u8 {
        let again = self.last.is_some_and(|(t, c, r)| {
            c == col && r == row && now_ms.saturating_sub(t) <= MULTI_CLICK_MS
        });
        self.count = if again { self.count % 3 + 1 } else { 1 };
        self.last = Some((now_ms, col, row));
        self.count
    }

    /// Forget the sequence (the content moved under the pointer).
    pub fn reset(&mut self) {
        self.last = None;
        self.count = 0;
    }

    pub fn kind(count: u8) -> SelKind {
        match count {
            2 => SelKind::Word,
            3 => SelKind::Line,
            _ => SelKind::Simple,
        }
    }
}

/// Whether a hyperlink target may be handed to `xdg-open`. Programs choose the URI, so
/// only plain document and web schemes are allowed (no `javascript:`, no custom
/// handlers that run commands).
pub fn link_openable(uri: &str) -> bool {
    let Some((scheme, rest)) = uri.split_once(':') else {
        return false;
    };
    let ok_scheme = matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "ftp" | "mailto" | "file"
    );
    ok_scheme && !rest.is_empty() && !uri.chars().any(char::is_control)
}

/// Modifier state for pointer events. The toolkit reports modifiers only on key events
/// and never key releases, so the last report is trusted for a short while only.
#[derive(Debug, Default)]
pub struct ModTracker {
    seen: Option<(u64, bool, bool, bool)>,
}

/// How long a modifier report stays valid for pointer events.
pub const MODS_VALID_MS: u64 = 1500;

impl ModTracker {
    pub fn update(&mut self, now_ms: u64, ctrl: bool, alt: bool, shift: bool) {
        self.seen = Some((now_ms, ctrl, alt, shift));
    }

    /// Record a key event: its reported modifiers, plus the modifier the key itself is
    /// (a modifier key press reports the state from before it).
    pub fn key(&mut self, now_ms: u64, keysym: Option<u32>, ctrl: bool, alt: bool, shift: bool) {
        let (mut c, mut a, mut s) = (ctrl, alt, shift);
        match keysym {
            Some(0xffe1 | 0xffe2) => s = true,
            Some(0xffe3 | 0xffe4) => c = true,
            Some(0xffe7..=0xffea) => a = true,
            _ => {}
        }
        self.update(now_ms, c, a, s);
    }

    /// (ctrl, alt, shift) as far as known at `now_ms`.
    pub fn at(&self, now_ms: u64) -> (bool, bool, bool) {
        match self.seen {
            Some((t, c, a, s)) if now_ms.saturating_sub(t) <= MODS_VALID_MS => (c, a, s),
            _ => (false, false, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_count_up_and_wrap() {
        let mut t = ClickTracker::default();
        assert_eq!(t.press(0, 3, 1), 1);
        assert_eq!(t.press(100, 3, 1), 2);
        assert_eq!(t.press(200, 3, 1), 3);
        assert_eq!(t.press(300, 3, 1), 1);
    }

    #[test]
    fn slow_or_moved_clicks_restart() {
        let mut t = ClickTracker::default();
        assert_eq!(t.press(0, 3, 1), 1);
        assert_eq!(t.press(MULTI_CLICK_MS + 1, 3, 1), 1);
        assert_eq!(t.press(MULTI_CLICK_MS + 50, 4, 1), 1);
        assert_eq!(t.press(MULTI_CLICK_MS + 60, 4, 2), 1);
        assert_eq!(t.press(MULTI_CLICK_MS + 70, 4, 2), 2);
        t.reset();
        assert_eq!(t.press(MULTI_CLICK_MS + 80, 4, 2), 1);
    }

    #[test]
    fn click_count_picks_the_selection_kind() {
        assert_eq!(ClickTracker::kind(1), SelKind::Simple);
        assert_eq!(ClickTracker::kind(2), SelKind::Word);
        assert_eq!(ClickTracker::kind(3), SelKind::Line);
    }

    #[test]
    fn only_plain_schemes_are_opened() {
        assert!(link_openable("https://example.org/a?b=c"));
        assert!(link_openable("HTTP://example.org"));
        assert!(link_openable("mailto:me@example.org"));
        assert!(link_openable("file:///tmp/x.txt"));
        assert!(!link_openable("javascript:alert(1)"));
        assert!(!link_openable("ssh://host"));
        assert!(!link_openable("https:"));
        assert!(!link_openable("no scheme"));
        assert!(!link_openable("https://a\u{7}b"));
        assert!(!link_openable("https://a\nb"));
    }

    #[test]
    fn modifier_reports_expire() {
        let mut m = ModTracker::default();
        assert_eq!(m.at(0), (false, false, false));
        m.update(1000, true, false, true);
        assert_eq!(m.at(1500), (true, false, true));
        assert_eq!(m.at(1000 + MODS_VALID_MS), (true, false, true));
        assert_eq!(m.at(1000 + MODS_VALID_MS + 1), (false, false, false));
        // Pressing Shift reports the state from before the press; the key adds itself.
        m.key(5000, Some(0xffe1), false, false, false);
        assert_eq!(m.at(5100), (false, false, true));
        m.key(5200, Some(0xffe3), false, false, true);
        assert_eq!(m.at(5300), (true, false, true));
        m.key(5400, None, false, false, false);
        assert_eq!(m.at(5500), (false, false, false));
    }
}
