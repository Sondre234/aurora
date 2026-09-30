//! Pure view-model: compositor snapshot -> what one output's bar shows.

use aurora_ipc::Snapshot;

/// Longest window title handed to the text engine; longer ones are cut with an ellipsis
/// here so a runaway title never costs a huge shaping pass. The widget ellipsizes again
/// to the pixels actually available.
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_APP_ID_CHARS: usize = 48;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsView {
    pub index: u32,
    pub active: bool,
    pub urgent: bool,
    pub windows: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputView {
    pub workspaces: Vec<WsView>,
    pub title: String,
    pub app_id: String,
}

/// The bar of `output` shows the workspaces the compositor lists under that output that
/// are active, occupied or urgent (in index order), and the focused window when it lives
/// on that output.
pub fn output_view(snap: &Snapshot, output: &str) -> OutputView {
    let mut workspaces: Vec<WsView> = snap
        .workspaces
        .iter()
        .filter(|w| w.output == output && (w.active || w.windows > 0 || w.urgent))
        .map(|w| WsView {
            index: w.index,
            active: w.active,
            urgent: w.urgent,
            windows: w.windows,
        })
        .collect();
    workspaces.sort_by_key(|w| w.index);
    workspaces.dedup_by_key(|w| w.index);

    let focused = snap
        .focused_window
        .and_then(|id| snap.windows.iter().find(|w| w.id == id))
        .filter(|w| w.output == output);
    OutputView {
        workspaces,
        title: focused.map_or_else(String::new, |w| ellipsize(&w.title, MAX_TITLE_CHARS)),
        app_id: focused.map_or_else(String::new, |w| ellipsize(&w.app_id, MAX_APP_ID_CHARS)),
    }
}

/// Single-line, trimmed text of at most `max` chars, ending in `…` when cut.
pub fn ellipsize(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        return flat.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = flat.chars().take(keep).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_ipc::{WindowInfo, WorkspaceInfo};

    fn ws(output: &str, index: u32, active: bool, windows: u32, urgent: bool) -> WorkspaceInfo {
        WorkspaceInfo {
            output: output.into(),
            index,
            active,
            windows,
            urgent,
        }
    }

    fn win(id: u64, output: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id,
            app_id: "kitty".into(),
            title: title.into(),
            workspace: 1,
            output: output.into(),
            floating: false,
            fullscreen: false,
            urgent: false,
        }
    }

    #[test]
    fn filters_per_output_sorts_and_drops_empty_inactive() {
        let snap = Snapshot {
            workspaces: vec![
                ws("A", 3, false, 2, false),
                ws("B", 2, true, 0, false),
                ws("A", 1, true, 1, false),
                ws("A", 5, false, 0, false),
                ws("A", 4, false, 0, true),
            ],
            ..Snapshot::default()
        };
        let v = output_view(&snap, "A");
        let idx: Vec<u32> = v.workspaces.iter().map(|w| w.index).collect();
        assert_eq!(idx, [1, 3, 4]);
        assert!(v.workspaces[0].active && v.workspaces[2].urgent);
        assert_eq!(output_view(&snap, "B").workspaces.len(), 1);
        assert!(output_view(&snap, "C").workspaces.is_empty());
    }

    #[test]
    fn title_only_on_the_focused_windows_output() {
        let snap = Snapshot {
            windows: vec![win(7, "A", "vim\nmain.rs")],
            focused_window: Some(7),
            ..Snapshot::default()
        };
        let a = output_view(&snap, "A");
        assert_eq!(
            (a.title.as_str(), a.app_id.as_str()),
            ("vim main.rs", "kitty")
        );
        assert_eq!(output_view(&snap, "B"), OutputView::default());
        let none = Snapshot {
            focused_window: Some(99),
            ..snap
        };
        assert_eq!(output_view(&none, "A").title, "");
    }

    #[test]
    fn ellipsize_cuts_on_chars_not_bytes() {
        assert_eq!(ellipsize("  hello  ", 10), "hello");
        assert_eq!(ellipsize("abcdefghij", 10), "abcdefghij");
        assert_eq!(ellipsize("abcdefghijk", 10), "abcdefghi…");
        assert_eq!(ellipsize("ååååååå", 4), "ååå…");
        assert_eq!(ellipsize("ab cdefg", 4), "ab…");
        assert_eq!(ellipsize("a\tb\u{7}c", 10), "a b c");
        assert_eq!(ellipsize("", 0), "");
        assert_eq!(ellipsize("x", 0), "…");
    }
}
