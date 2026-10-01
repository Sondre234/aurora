//! What the user can do to files: run operations on a worker with progress, conflict
//! questions and cancel, the clipboard, rename and create, opening, undo.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use aurora_ui::runtime::calloop::channel::Sender;
use aurora_ui::runtime::{Offer, Runtime, Selection, text_offers};

use crate::app::{Clip, Files, ModalKind, Msg, OpMsg};
use crate::edit::LineEdit;
use crate::ops::{self, Choice, Op, Progress, Reporter, Resolution};
use crate::scene::{Busy, Editing, Modal};
use crate::system;
use crate::uri::{
    ClipOp, MIME_COPIED_FILES, MIME_URI_LIST, decode_paste, encode_copied_files, encode_uri_list,
    pick_file_mime,
};

/// Clipboard read tags.
pub const TAG_FILES: u64 = 1;
pub const TAG_TEXT: u64 = 2;

/// An operation that is executing on its worker thread.
pub struct Running {
    pub id: u64,
    pub op: Op,
    pub cancel: Arc<AtomicBool>,
    pub resolver: mpsc::Sender<Resolution>,
    pub awaiting_conflict: bool,
}

/// The worker's end: reports into the loop, blocks on conflict answers.
struct ChannelReporter {
    id: u64,
    tx: Sender<Msg>,
    cancel: Arc<AtomicBool>,
    answers: mpsc::Receiver<Resolution>,
}

impl Reporter for ChannelReporter {
    fn progress(&mut self, p: &Progress) {
        let _ = self.tx.send(Msg::Op(OpMsg::Progress {
            id: self.id,
            progress: p.clone(),
        }));
    }

    fn conflict(&mut self, src: &Path, dst: &Path) -> Resolution {
        let _ = self.tx.send(Msg::Op(OpMsg::Conflict {
            id: self.id,
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
        }));
        // The window going away drops the sender: that means cancel.
        self.answers.recv().unwrap_or(Resolution {
            choice: Choice::Cancel,
            apply_all: false,
        })
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// `3 items` / `1 item`.
pub fn items(n: usize) -> String {
    format!("{n} {}", if n == 1 { "item" } else { "items" })
}

/// A path typed in the path bar: `~` expands, relative paths start at `cwd`, `.` and `..`
/// are resolved without touching the disk (symlinks stay as typed).
pub fn expand_input(text: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let text = text.trim();
    let joined = if text == "~" {
        home.map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.to_path_buf())
    } else if let Some(rest) = text.strip_prefix("~/") {
        home.map_or_else(|| cwd.join(rest), |h| h.join(rest))
    } else if text.starts_with('/') {
        PathBuf::from(text)
    } else {
        cwd.join(text)
    };
    let mut out = PathBuf::from("/");
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(n) => out.push(n),
            _ => {}
        }
    }
    out
}

/// Lines of a confirmation or toast listing some names.
pub fn name_lines(names: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = names.iter().take(3).cloned().collect();
    if names.len() > 3 {
        lines.push(format!("and {} more", names.len() - 3));
    }
    lines
}

impl Files {
    pub fn targets(&self) -> Vec<PathBuf> {
        self.scene.borrow().browser.selected_paths()
    }

    pub fn cwd(&self) -> PathBuf {
        self.scene.borrow().browser.path().to_path_buf()
    }

    // ---- running operations ----

    pub fn start_op(&mut self, rt: &mut Runtime<Self>, op: Op) {
        self.queue.push_back(op);
        self.pump_ops(rt);
    }

    fn pump_ops(&mut self, rt: &mut Runtime<Self>) {
        if self.running.is_some() {
            return;
        }
        let Some(op) = self.queue.pop_front() else {
            return;
        };
        let id = self.next_op;
        self.next_op += 1;
        tracing::info!(
            "files: op start id={id} kind={} items={}",
            op.kind_name(),
            op.item_count()
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let (resolver, answers) = mpsc::channel();
        let reporter = ChannelReporter {
            id,
            tx: self.tx.clone(),
            cancel: cancel.clone(),
            answers,
        };
        let env = self.env.clone();
        let worker_op = op.clone();
        let done_tx = self.tx.clone();
        let spawned = std::thread::Builder::new()
            .name("files-op".into())
            .spawn(move || {
                let mut reporter = reporter;
                let outcome = ops::run(&worker_op, &env, &mut reporter);
                let _ = done_tx.send(Msg::Op(OpMsg::Done { id, outcome }));
            });
        if let Err(e) = spawned {
            self.toast(rt, format!("cannot start the operation: {e}"), true);
            return;
        }
        self.scene.borrow_mut().busy = Some(Busy {
            text: format!("{}…", op.verb()),
            fraction: -1.0,
        });
        self.running = Some(Running {
            id,
            op,
            cancel,
            resolver,
            awaiting_conflict: false,
        });
        self.damage_status(rt);
    }

    pub fn cancel_running(&mut self) {
        if let Some(r) = &self.running {
            r.cancel.store(true, Ordering::Relaxed);
            if r.awaiting_conflict {
                let _ = r.resolver.send(Resolution {
                    choice: Choice::Cancel,
                    apply_all: false,
                });
            }
        }
    }

    pub fn on_op_msg(&mut self, rt: &mut Runtime<Self>, msg: OpMsg) {
        self.last_activity = Instant::now();
        match msg {
            OpMsg::Progress { id, progress } => {
                if self.running.as_ref().is_none_or(|r| r.id != id) {
                    return;
                }
                let verb = self.running.as_ref().map_or("", |r| r.op.verb());
                let fraction = if progress.bytes_total > 0 {
                    progress.bytes_done as f32 / progress.bytes_total as f32
                } else if progress.items_total > 1 {
                    progress.items_done as f32 / progress.items_total as f32
                } else {
                    -1.0
                };
                let shown = (progress.items_done + 1).min(progress.items_total.max(1));
                self.scene.borrow_mut().busy = Some(Busy {
                    text: format!(
                        "{verb} {shown}/{}: {}  (Esc cancels)",
                        progress.items_total, progress.current
                    ),
                    fraction,
                });
                self.damage_status(rt);
            }
            OpMsg::Conflict { id, src, dst } => {
                let Some(r) = self.running.as_mut().filter(|r| r.id == id) else {
                    return;
                };
                r.awaiting_conflict = true;
                let name = dst
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let dir = dst
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let from = src
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.modal_kind = ModalKind::Conflict;
                self.scene.borrow_mut().modal = Some(Modal {
                    title: format!("\"{name}\" already exists"),
                    lines: vec![
                        format!("in {dir}"),
                        format!("replacing the file from {from}?"),
                    ],
                    buttons: vec![
                        "Skip (s)".into(),
                        "Replace (r)".into(),
                        "Keep both (k)".into(),
                        "Cancel (Esc)".into(),
                    ],
                    apply_all: Some(false),
                });
                self.damage_all(rt);
            }
            OpMsg::Done { id, outcome } => {
                let Some(r) = self.running.take().filter(|r| r.id == id) else {
                    return;
                };
                self.finish_op(rt, r, outcome);
            }
        }
    }

    fn finish_op(&mut self, rt: &mut Runtime<Self>, r: Running, outcome: ops::Outcome) {
        tracing::info!(
            "files: op done id={} ok={} failed={}",
            r.id,
            outcome.ok,
            outcome.failed
        );
        {
            let mut s = self.scene.borrow_mut();
            s.busy = None;
            s.modal = None;
        }
        self.modal_kind = ModalKind::None;
        if let Some(u) = outcome.undo.clone() {
            self.last_undo = Some(u);
        }
        if matches!(r.op, Op::Move { .. }) {
            self.clip = self.clip.take().filter(|c| c.op != ClipOp::Cut);
            let mut s = self.scene.borrow_mut();
            s.cut_dir = None;
            s.cut.clear();
        }
        if outcome.failed > 0 {
            let first = outcome.errors.first().cloned().unwrap_or_default();
            let more = if outcome.failed > 1 {
                format!(" (and {} more failed)", outcome.failed - 1)
            } else {
                String::new()
            };
            self.toast(rt, format!("{first}{more}"), true);
        } else if outcome.cancelled {
            self.toast(rt, "Cancelled", false);
        } else if matches!(r.op, Op::Trash { .. }) {
            self.toast(
                rt,
                format!("Moved {} to the trash (Ctrl+Z undoes)", items(outcome.ok)),
                false,
            );
        }
        if outcome.failed > 0 || outcome.cancelled {
            self.pending_select = None;
        }
        self.refresh(rt);
        if self.closing {
            rt.quit();
            return;
        }
        self.pump_ops(rt);
        self.damage_status(rt);
    }

    // ---- modal answers ----

    pub fn answer_conflict(&mut self, rt: &mut Runtime<Self>, choice: Choice) {
        if self.modal_kind != ModalKind::Conflict {
            return;
        }
        let apply_all = self
            .scene
            .borrow()
            .modal
            .as_ref()
            .and_then(|m| m.apply_all)
            .unwrap_or(false);
        if let Some(r) = self.running.as_mut() {
            r.awaiting_conflict = false;
            let _ = r.resolver.send(Resolution { choice, apply_all });
        }
        self.scene.borrow_mut().modal = None;
        self.modal_kind = ModalKind::None;
        self.damage_all(rt);
    }

    pub fn toggle_apply_all(&mut self, rt: &mut Runtime<Self>) {
        if let Some(m) = self.scene.borrow_mut().modal.as_mut()
            && let Some(a) = m.apply_all.as_mut()
        {
            *a = !*a;
        }
        self.damage_all(rt);
    }

    pub fn answer_confirm(&mut self, rt: &mut Runtime<Self>, yes: bool) {
        let ModalKind::Confirm(op) = std::mem::take(&mut self.modal_kind) else {
            return;
        };
        self.scene.borrow_mut().modal = None;
        self.damage_all(rt);
        if yes {
            self.start_op(rt, op);
        }
    }

    /// A modal button, by index (0-based, left to right).
    pub fn modal_button(&mut self, rt: &mut Runtime<Self>, i: usize) {
        match self.modal_kind.clone() {
            ModalKind::Conflict => match i {
                0 => self.answer_conflict(rt, Choice::Skip),
                1 => self.answer_conflict(rt, Choice::Replace),
                2 => self.answer_conflict(rt, Choice::KeepBoth),
                _ => self.answer_conflict(rt, Choice::Cancel),
            },
            ModalKind::Confirm(_) => self.answer_confirm(rt, i == 0),
            ModalKind::None => {}
        }
    }

    // ---- trash, delete, undo ----

    pub fn trash_selection(&mut self, rt: &mut Runtime<Self>) {
        let srcs = self.targets();
        if !srcs.is_empty() {
            self.start_op(rt, Op::Trash { srcs });
        }
    }

    pub fn confirm_delete(&mut self, rt: &mut Runtime<Self>) {
        let srcs = self.targets();
        if srcs.is_empty() {
            return;
        }
        let names: Vec<String> = srcs
            .iter()
            .map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        let mut lines = vec![format!(
            "{} will be deleted for good. This cannot be undone.",
            items(srcs.len())
        )];
        lines.extend(name_lines(&names));
        self.scene.borrow_mut().modal = Some(Modal {
            title: "Delete permanently?".into(),
            lines,
            buttons: vec!["Delete (y)".into(), "Cancel (Esc)".into()],
            apply_all: None,
        });
        self.modal_kind = ModalKind::Confirm(Op::Delete { srcs });
        self.damage_all(rt);
    }

    pub fn undo(&mut self, rt: &mut Runtime<Self>) {
        match self.last_undo.take() {
            Some(u) => self.start_op(rt, Op::Undo(u)),
            None => self.toast(rt, "Nothing to undo", false),
        }
    }

    // ---- clipboard ----

    pub fn copy_selection(&mut self, rt: &mut Runtime<Self>, cut: bool) {
        let paths = self.targets();
        if paths.is_empty() {
            return;
        }
        let op = if cut { ClipOp::Cut } else { ClipOp::Copy };
        let mut offers: Vec<Offer> = vec![
            (
                MIME_COPIED_FILES.to_string(),
                Arc::from(encode_copied_files(op, &paths).into_bytes()),
            ),
            (
                MIME_URI_LIST.to_string(),
                Arc::from(encode_uri_list(&paths).into_bytes()),
            ),
        ];
        let listing: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        offers.extend(text_offers(&listing.join("\n")));
        if let Err(e) = rt.set_selection(Selection::Clipboard, offers) {
            // The window can still paste from its own buffer.
            tracing::warn!("files: cannot offer the clipboard: {e}");
        }
        let n = paths.len();
        {
            let mut s = self.scene.borrow_mut();
            if cut {
                s.cut_dir = Some(s.browser.path().to_path_buf());
                s.cut = s.browser.selection.names().cloned().collect();
            } else {
                s.cut_dir = None;
                s.cut.clear();
            }
        }
        self.clip = Some(Clip { op, paths });
        self.owns_clip = true;
        self.toast(
            rt,
            format!("{} {}", if cut { "Cut" } else { "Copied" }, items(n)),
            false,
        );
        self.damage_list(rt);
    }

    pub fn paste(&mut self, rt: &mut Runtime<Self>) {
        if self.owns_clip
            && let Some(c) = self.clip.clone()
        {
            self.paste_paths(rt, c.op, c.paths);
            return;
        }
        let offered = rt.selection_mimes(Selection::Clipboard);
        match pick_file_mime(&offered) {
            Some(mime) => {
                if let Err(e) = rt.read_selection(Selection::Clipboard, mime, TAG_FILES) {
                    self.toast(rt, format!("cannot read the clipboard: {e}"), true);
                }
            }
            None => self.toast(rt, "No files on the clipboard", false),
        }
    }

    pub fn paste_text_into_editor(&mut self, rt: &mut Runtime<Self>) {
        let offered = rt.selection_mimes(Selection::Clipboard);
        if let Some(mime) = aurora_ui::runtime::pick_text_mime(&offered) {
            let mime = mime.to_string();
            if let Err(e) = rt.read_selection(Selection::Clipboard, &mime, TAG_TEXT) {
                self.toast(rt, format!("cannot read the clipboard: {e}"), true);
            }
        }
    }

    pub fn on_selection_data(
        &mut self,
        rt: &mut Runtime<Self>,
        tag: u64,
        mime: &str,
        data: Option<Vec<u8>>,
    ) {
        let Some(data) = data else {
            self.toast(rt, "The clipboard owner did not send any data", true);
            return;
        };
        match tag {
            TAG_FILES => match decode_paste(mime, &data) {
                Some((op, paths)) => self.paste_paths(rt, op, paths),
                None => self.toast(rt, "No files on the clipboard", false),
            },
            TAG_TEXT => {
                let text = String::from_utf8_lossy(&data).into_owned();
                let first_line = text.lines().next().unwrap_or("");
                let mut s = self.scene.borrow_mut();
                if let Some(e) = s.editing.edit_mut() {
                    e.insert(first_line);
                }
                drop(s);
                self.on_editing_changed(rt);
            }
            _ => {}
        }
    }

    fn paste_paths(&mut self, rt: &mut Runtime<Self>, op: ClipOp, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let dest = self.cwd();
        self.start_op(
            rt,
            match op {
                ClipOp::Copy => Op::Copy { srcs: paths, dest },
                ClipOp::Cut => Op::Move { srcs: paths, dest },
            },
        );
    }

    // ---- rename and create ----

    pub fn start_rename(&mut self, rt: &mut Runtime<Self>) {
        let target = {
            let s = self.scene.borrow();
            s.browser.cursor_entry().map(|e| e.name.clone())
        };
        let Some(target) = target else { return };
        let edit = LineEdit::with_stem_selected(&target.to_string_lossy());
        self.scene.borrow_mut().editing = Editing::Rename { target, edit };
        self.ensure_cursor_visible();
        self.damage_all(rt);
    }

    /// Enter in an editor.
    pub fn commit_edit(&mut self, rt: &mut Runtime<Self>) {
        let editing = std::mem::take(&mut self.scene.borrow_mut().editing);
        let cwd = self.cwd();
        match editing {
            Editing::None => {}
            Editing::Path(e) => {
                let target = expand_input(e.text(), &cwd, self.home.as_deref());
                if target.is_dir() {
                    self.navigate(rt, target);
                } else {
                    self.toast(rt, format!("{} is not a folder", target.display()), true);
                }
            }
            Editing::Filter(_) => {} // the filter stays applied
            Editing::Rename { target, edit } => {
                let new_name = edit.text().to_string();
                if new_name.is_empty() || std::ffi::OsStr::new(&new_name) == target {
                    self.damage_all(rt);
                    return;
                }
                self.pending_select = Some(new_name.clone().into());
                self.start_op(
                    rt,
                    Op::Rename {
                        src: cwd.join(&target),
                        new_name: new_name.into(),
                    },
                );
            }
            Editing::New { dir, edit } => {
                let name = edit.text().to_string();
                if name.is_empty() {
                    self.damage_all(rt);
                    return;
                }
                self.pending_select = Some(name.clone().into());
                let op = if dir {
                    Op::Mkdir {
                        parent: cwd,
                        name: name.into(),
                    }
                } else {
                    Op::Touch {
                        parent: cwd,
                        name: name.into(),
                    }
                };
                self.start_op(rt, op);
            }
        }
        self.damage_all(rt);
    }

    /// Escape in an editor.
    pub fn cancel_edit(&mut self, rt: &mut Runtime<Self>) {
        let editing = std::mem::take(&mut self.scene.borrow_mut().editing);
        if matches!(editing, Editing::Filter(_)) {
            self.scene.borrow_mut().browser.set_filter("");
            self.ensure_cursor_visible();
        }
        self.damage_all(rt);
    }

    /// The text of the active editor changed (live filter).
    pub fn on_editing_changed(&mut self, rt: &mut Runtime<Self>) {
        let mut s = self.scene.borrow_mut();
        if let Editing::Filter(e) = &s.editing {
            let text = e.text().to_string();
            s.browser.set_filter(&text);
            let m = s.metrics();
            let rows = s.browser.len();
            s.scroll = m.clamp_scroll(s.scroll, rows);
        }
        drop(s);
        self.damage_all(rt);
    }

    // ---- opening ----

    pub fn open_row(&mut self, rt: &mut Runtime<Self>, row: usize) {
        let Some(entry) = self.scene.borrow().browser.entry(row).cloned() else {
            return;
        };
        let path = self.cwd().join(&entry.name);
        if entry.kind == crate::model::Kind::Broken {
            self.toast(
                rt,
                format!("{} is a broken link", entry.display_name()),
                true,
            );
        } else if entry.is_dir {
            tracing::info!("files: open path={} via=navigate", path.display());
            self.navigate(rt, path);
        } else {
            tracing::info!("files: open path={} via=xdg-open", path.display());
            self.spawn(rt, system::open_argv(&path));
        }
    }

    pub fn open_cursor(&mut self, rt: &mut Runtime<Self>) {
        let row = self.scene.borrow().browser.selection.cursor();
        if let Some(r) = row {
            self.open_row(rt, r);
        }
    }

    /// Ctrl+Enter: the cursor's folder (or, on a file, this folder) in a new window.
    pub fn open_in_new_window(&mut self, rt: &mut Runtime<Self>) {
        let dir = {
            let s = self.scene.borrow();
            match s.browser.cursor_entry() {
                Some(e) if e.is_dir => s.browser.path().join(&e.name),
                _ => s.browser.path().to_path_buf(),
            }
        };
        self.spawn(rt, system::new_window_argv(&dir));
    }

    pub fn open_terminal(&mut self, rt: &mut Runtime<Self>) {
        let dir = self.cwd();
        self.spawn(rt, system::terminal_argv(&dir));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_paths_expand_and_normalize() {
        let cwd = Path::new("/home/u/work");
        let home = Some(Path::new("/home/u"));
        let p = |t: &str| expand_input(t, cwd, home);
        assert_eq!(p("/etc"), PathBuf::from("/etc"));
        assert_eq!(p("  /etc/  "), PathBuf::from("/etc"));
        assert_eq!(p("~"), PathBuf::from("/home/u"));
        assert_eq!(p("~/Documents"), PathBuf::from("/home/u/Documents"));
        assert_eq!(p("sub/dir"), PathBuf::from("/home/u/work/sub/dir"));
        assert_eq!(p(".."), PathBuf::from("/home/u"));
        assert_eq!(p("./a/../b"), PathBuf::from("/home/u/work/b"));
        assert_eq!(p("/../.."), PathBuf::from("/"));
        assert_eq!(
            expand_input("~/x", cwd, None),
            PathBuf::from("/home/u/work/x")
        );
    }

    #[test]
    fn wording() {
        assert_eq!(items(1), "1 item");
        assert_eq!(items(0), "0 items");
        let names: Vec<String> = (1..=5).map(|i| format!("f{i}")).collect();
        assert_eq!(name_lines(&names), ["f1", "f2", "f3", "and 2 more"]);
        assert_eq!(name_lines(&names[..2]), ["f1", "f2"]);
    }
}
