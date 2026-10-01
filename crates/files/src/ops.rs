//! File operations: planning helpers, guards and the executor.
//!
//! [`run`] executes one [`Op`] on the calling thread (the app runs it on a worker) and
//! talks to the outside only through a [`Reporter`]: progress, conflict questions and the
//! cancel flag. That keeps the executor testable with a scripted reporter inside a temp dir.
//!
//! Safety rules enforced here, not in the UI:
//! - a source that is `/` or `$HOME` itself is refused for every operation;
//! - a mount point is never trashed, moved or deleted;
//! - deleting never follows symlinks (a symlink to a directory is removed, not emptied);
//! - a directory is never copied or moved into itself;
//! - a trash request for a file on another filesystem fails, it does not delete.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, FileTimes, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::names::{unique_copy_name, validate_name};
use crate::trash::{self, LocalTime, TrashedItem};

/// What to do. Paths are absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Copy { srcs: Vec<PathBuf>, dest: PathBuf },
    Move { srcs: Vec<PathBuf>, dest: PathBuf },
    Trash { srcs: Vec<PathBuf> },
    Delete { srcs: Vec<PathBuf> },
    Rename { src: PathBuf, new_name: OsString },
    Mkdir { parent: PathBuf, name: OsString },
    Touch { parent: PathBuf, name: OsString },
    Undo(Undo),
}

impl Op {
    /// The name used in the `files: op start kind=...` log line.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Op::Copy { .. } => "copy",
            Op::Move { .. } => "move",
            Op::Trash { .. } => "trash",
            Op::Delete { .. } => "delete",
            Op::Rename { .. } => "rename",
            Op::Mkdir { .. } => "mkdir",
            Op::Touch { .. } => "touch",
            Op::Undo(_) => "undo",
        }
    }

    pub fn item_count(&self) -> usize {
        match self {
            Op::Copy { srcs, .. }
            | Op::Move { srcs, .. }
            | Op::Trash { srcs }
            | Op::Delete { srcs } => srcs.len(),
            Op::Undo(Undo::Trash(items)) => items.len(),
            Op::Undo(Undo::Move(items)) => items.len(),
            Op::Rename { .. }
            | Op::Mkdir { .. }
            | Op::Touch { .. }
            | Op::Undo(Undo::Rename { .. }) => 1,
        }
    }

    /// Verb for the status bar.
    pub fn verb(&self) -> &'static str {
        match self {
            Op::Copy { .. } => "Copying",
            Op::Move { .. } => "Moving",
            Op::Trash { .. } => "Moving to trash",
            Op::Delete { .. } => "Deleting",
            Op::Rename { .. } => "Renaming",
            Op::Mkdir { .. } | Op::Touch { .. } => "Creating",
            Op::Undo(_) => "Undoing",
        }
    }
}

/// How to reverse a finished operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undo {
    Trash(Vec<TrashedItem>),
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// `(original location, where it is now)`.
    Move(Vec<(PathBuf, PathBuf)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Skip,
    Replace,
    KeepBoth,
    /// Stop the whole operation.
    Cancel,
}

/// The user's answer to a conflict question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub choice: Choice,
    /// Use `choice` for every further conflict of this operation.
    pub apply_all: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub items_done: usize,
    pub items_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current: String,
}

/// The executor's window to the world.
pub trait Reporter {
    fn progress(&mut self, p: &Progress);
    /// `dst` already exists while `src` is being put there. May block for the answer.
    fn conflict(&mut self, src: &Path, dst: &Path) -> Resolution;
    fn cancelled(&self) -> bool;
}

/// What the operation may touch outside its sources.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub home: Option<PathBuf>,
    pub trash_root: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute()),
            trash_root: trash::trash_root_from_env(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub ok: usize,
    pub failed: usize,
    pub errors: Vec<String>,
    pub undo: Option<Undo>,
    pub cancelled: bool,
}

// ---- guards and planning (pure or read-only) ----

fn same_inode(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Refuses `/` and `$HOME` itself (the real directories, found by inode, so a trailing
/// slash or `..` spelling does not slip through; a symlink that merely points there is
/// a different file and is fine).
pub fn check_source(path: &Path, home: Option<&Path>) -> Result<(), String> {
    if path.parent().is_none() {
        return Err("refusing to operate on the root directory".into());
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(()); // missing: the operation itself reports it
    };
    let protected = [Some(Path::new("/")), home].into_iter().flatten();
    for p in protected {
        if fs::metadata(p).is_ok_and(|pm| same_inode(&meta, &pm)) {
            let what = if p == Path::new("/") {
                "the root directory".to_string()
            } else {
                "the home directory".to_string()
            };
            return Err(format!("refusing to operate on {what} itself"));
        }
    }
    Ok(())
}

/// True when `path` is a real directory on another device than its parent (or `/`).
pub fn is_mount_point(path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    match path.parent() {
        None => true,
        Some(parent) => fs::metadata(if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        })
        .is_ok_and(|pm| pm.dev() != meta.dev()),
    }
}

/// True when putting something into `dest_dir` would put the directory `src` inside itself.
pub fn dest_inside_source(src: &Path, dest_dir: &Path) -> bool {
    if !fs::symlink_metadata(src).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    match (fs::canonicalize(src), fs::canonicalize(dest_dir)) {
        (Ok(s), Ok(d)) => d.starts_with(s),
        _ => false,
    }
}

/// Where each source lands in `dest`: `dest/<file name>`.
pub fn plan_transfers(srcs: &[PathBuf], dest: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    srcs.iter()
        .map(|s| {
            s.file_name()
                .map(|n| (s.clone(), dest.join(n)))
                .ok_or_else(|| format!("{}: has no file name", s.display()))
        })
        .collect()
}

/// Items and bytes under `path` (symlinks counted, not followed).
pub fn measure(path: &Path) -> (usize, u64) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if !meta.is_dir() {
        return (1, meta.len());
    }
    let (mut items, mut bytes) = (1, 0);
    if let Ok(rd) = fs::read_dir(path) {
        for e in rd.flatten() {
            let (i, b) = measure(&e.path());
            items += i;
            bytes += b;
        }
    }
    (items, bytes)
}

fn shown(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

// ---- executor ----

enum Fail {
    Cancelled,
    Msg(String),
}

fn fail_at(path: &Path, e: impl std::fmt::Display) -> Fail {
    Fail::Msg(format!("{}: {e}", shown(path)))
}

enum Flow {
    Done,
    Skipped,
}

const CHUNK: usize = 1 << 20;
const REPORT_EVERY: Duration = Duration::from_millis(100);

struct Exec<'a> {
    rep: &'a mut dyn Reporter,
    env: &'a Env,
    sticky: Option<Choice>,
    p: Progress,
    last: Instant,
    outcome: Outcome,
    moved: Vec<(PathBuf, PathBuf)>,
    trashed: Vec<TrashedItem>,
    /// Depth of folder merges in progress: what moves inside one is not recorded for undo.
    merging: u32,
}

/// Runs `op` to completion (or cancellation) and reports what happened.
pub fn run(op: &Op, env: &Env, rep: &mut dyn Reporter) -> Outcome {
    let mut x = Exec {
        rep,
        env,
        sticky: None,
        p: Progress {
            items_total: op.item_count(),
            ..Progress::default()
        },
        last: Instant::now(),
        outcome: Outcome::default(),
        moved: Vec::new(),
        trashed: Vec::new(),
        merging: 0,
    };
    x.execute(op);
    x.outcome
}

impl Exec<'_> {
    fn execute(&mut self, op: &Op) {
        match op {
            Op::Copy { srcs, dest } => {
                self.p.bytes_total = srcs.iter().map(|s| measure(s).1).sum();
                self.transfer(srcs, dest, false);
            }
            Op::Move { srcs, dest } => self.transfer(srcs, dest, true),
            Op::Trash { srcs } => self.each(srcs, |x, s| x.trash_one(s)),
            Op::Delete { srcs } => self.each(srcs, |x, s| {
                x.check_removable(s)?;
                x.remove_tree(s)?;
                Ok(Flow::Done)
            }),
            Op::Rename { src, new_name } => {
                self.each(std::slice::from_ref(src), |x, s| x.rename_one(s, new_name))
            }
            Op::Mkdir { parent, name } => self.create(parent, name, true),
            Op::Touch { parent, name } => self.create(parent, name, false),
            Op::Undo(u) => self.undo(u),
        }
        if !self.moved.is_empty() {
            self.outcome.undo = Some(Undo::Move(std::mem::take(&mut self.moved)));
        } else if !self.trashed.is_empty() {
            self.outcome.undo = Some(Undo::Trash(std::mem::take(&mut self.trashed)));
        }
        self.report(true);
    }

    fn report(&mut self, force: bool) {
        if force || self.last.elapsed() >= REPORT_EVERY {
            self.last = Instant::now();
            let p = self.p.clone();
            self.rep.progress(&p);
        }
    }

    fn check_cancel(&mut self) -> Result<(), Fail> {
        if self.rep.cancelled() {
            Err(Fail::Cancelled)
        } else {
            Ok(())
        }
    }

    fn ask(&mut self, src: &Path, dst: &Path) -> Result<Choice, Fail> {
        if let Some(c) = self.sticky {
            return Ok(c);
        }
        let r = self.rep.conflict(src, dst);
        if r.choice == Choice::Cancel {
            return Err(Fail::Cancelled);
        }
        if r.apply_all {
            self.sticky = Some(r.choice);
        }
        Ok(r.choice)
    }

    fn fail(&mut self, msg: String) {
        self.outcome.failed += 1;
        self.outcome.errors.push(msg);
    }

    /// Runs `f` for each top-level item, counting results; stops on cancel.
    fn each(
        &mut self,
        items: &[PathBuf],
        mut f: impl FnMut(&mut Self, &Path) -> Result<Flow, Fail>,
    ) {
        for item in items {
            if self.check_cancel().is_err() {
                self.outcome.cancelled = true;
                return;
            }
            self.p.current = shown(item);
            self.report(true);
            let failed_before = self.outcome.failed;
            let result = match check_source(item, self.env.home.as_deref()) {
                Ok(()) => f(self, item),
                Err(e) => Err(fail_at(item, e)),
            };
            match result {
                Ok(_) => {
                    if self.outcome.failed == failed_before {
                        self.outcome.ok += 1;
                    }
                }
                Err(Fail::Cancelled) => {
                    self.outcome.cancelled = true;
                    return;
                }
                Err(Fail::Msg(m)) => self.fail(m),
            }
            self.p.items_done += 1;
        }
    }

    // ---- copy and move ----

    fn transfer(&mut self, srcs: &[PathBuf], dest: &Path, moving: bool) {
        let plan = match plan_transfers(srcs, dest) {
            Ok(p) => p,
            Err(e) => {
                self.fail(e);
                return;
            }
        };
        let items: Vec<PathBuf> = plan.iter().map(|(s, _)| s.clone()).collect();
        let dests: std::collections::HashMap<PathBuf, PathBuf> = plan.into_iter().collect();
        self.each(&items, |x, src| {
            if dest_inside_source(src, dest) {
                return Err(fail_at(src, "cannot put a folder inside itself"));
            }
            if moving {
                x.check_removable(src)?;
            }
            let dst = dests[src].clone();
            if moving {
                x.move_into(src, dst)
            } else {
                x.copy_into(src, dst)
            }
        });
    }

    /// Decides what to do about an existing `dst`. `Ok(None)`: the destination is free (or
    /// is to be merged into), `Ok(Some(path))`: use that final path, `Err(Skip)` via flow.
    fn resolve(
        &mut self,
        src_meta: &fs::Metadata,
        src: &Path,
        dst: PathBuf,
    ) -> Result<Resolved, Fail> {
        let dmeta = match fs::symlink_metadata(&dst) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Resolved::Free(dst)),
            Err(e) => return Err(fail_at(&dst, e)),
        };
        if src_meta.is_dir() && dmeta.is_dir() && !same_inode(src_meta, &dmeta) {
            return Ok(Resolved::Merge(dst));
        }
        if same_inode(src_meta, &dmeta) {
            // Onto itself: the only sensible thing is a sibling copy.
            return Ok(Resolved::Free(sibling_copy(&dst)));
        }
        match self.ask(src, &dst)? {
            Choice::Skip => Ok(Resolved::Skip),
            Choice::KeepBoth => Ok(Resolved::Free(sibling_copy(&dst))),
            Choice::Replace => Ok(Resolved::Replace(dst, dmeta.is_dir())),
            Choice::Cancel => Err(Fail::Cancelled),
        }
    }

    fn copy_into(&mut self, src: &Path, dst: PathBuf) -> Result<Flow, Fail> {
        self.check_cancel()?;
        let meta = fs::symlink_metadata(src).map_err(|e| fail_at(src, e))?;
        match self.resolve(&meta, src, dst)? {
            Resolved::Skip => Ok(Flow::Skipped),
            Resolved::Merge(dst) => {
                self.copy_children(src, &dst)?;
                Ok(Flow::Done)
            }
            Resolved::Free(dst) => {
                self.copy_new(src, &meta, &dst)?;
                Ok(Flow::Done)
            }
            Resolved::Replace(dst, dst_is_dir) => {
                if meta.is_file() && !dst_is_dir {
                    // Atomic: write beside it, then rename over.
                    let tmp = temp_sibling(&dst);
                    if let Err(e) = self.copy_new(src, &meta, &tmp) {
                        let _ = fs::remove_file(&tmp);
                        return Err(e);
                    }
                    fs::rename(&tmp, &dst).map_err(|e| {
                        let _ = fs::remove_file(&tmp);
                        fail_at(&dst, e)
                    })?;
                } else {
                    self.check_removable(&dst)?;
                    self.remove_tree(&dst)?;
                    self.copy_new(src, &meta, &dst)?;
                }
                Ok(Flow::Done)
            }
        }
    }

    fn copy_children(&mut self, src: &Path, dst: &Path) -> Result<(), Fail> {
        let rd = fs::read_dir(src).map_err(|e| fail_at(src, e))?;
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    self.fail(format!("{}: {e}", shown(src)));
                    continue;
                }
            };
            match self.copy_into(&entry.path(), dst.join(entry.file_name())) {
                Ok(_) => {}
                Err(Fail::Cancelled) => return Err(Fail::Cancelled),
                Err(Fail::Msg(m)) => self.fail(m),
            }
        }
        Ok(())
    }

    /// Copies onto a path that does not exist.
    fn copy_new(&mut self, src: &Path, meta: &fs::Metadata, dst: &Path) -> Result<(), Fail> {
        self.check_cancel()?;
        let ft = meta.file_type();
        if ft.is_symlink() {
            let target = fs::read_link(src).map_err(|e| fail_at(src, e))?;
            std::os::unix::fs::symlink(&target, dst).map_err(|e| fail_at(dst, e))?;
        } else if ft.is_dir() {
            DirBuilder::new()
                .mode(meta.mode() & 0o7777 | 0o700)
                .create(dst)
                .map_err(|e| fail_at(dst, e))?;
            let result = self.copy_children(src, dst);
            // Exact mode and mtime after the contents, so a read-only source stays so.
            let _ = fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode() & 0o7777));
            if let Ok(f) = File::open(dst)
                && let Ok(m) = meta.modified()
            {
                let _ = f.set_times(FileTimes::new().set_modified(m));
            }
            result?;
        } else if ft.is_file() {
            self.copy_file(src, meta, dst)?;
        } else {
            return Err(fail_at(
                src,
                "unsupported file type (socket, pipe or device)",
            ));
        }
        Ok(())
    }

    fn copy_file(&mut self, src: &Path, meta: &fs::Metadata, dst: &Path) -> Result<(), Fail> {
        let mut input = File::open(src).map_err(|e| fail_at(src, e))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(meta.mode() & 0o7777 | 0o600)
            .open(dst)
            .map_err(|e| fail_at(dst, e))?;
        let mut buf = vec![0u8; CHUNK];
        let result: Result<(), Fail> = (|| {
            loop {
                self.check_cancel()?;
                let n = input.read(&mut buf).map_err(|e| fail_at(src, e))?;
                if n == 0 {
                    break;
                }
                output.write_all(&buf[..n]).map_err(|e| fail_at(dst, e))?;
                self.p.bytes_done += n as u64;
                self.report(false);
            }
            let _ = output.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o7777));
            if let Ok(m) = meta.modified() {
                let _ = output.set_times(FileTimes::new().set_modified(m));
            }
            output.flush().map_err(|e| fail_at(dst, e))
        })();
        if result.is_err() {
            drop(output);
            let _ = fs::remove_file(dst);
        }
        result
    }

    fn move_into(&mut self, src: &Path, dst: PathBuf) -> Result<Flow, Fail> {
        self.check_cancel()?;
        if src == dst {
            return Ok(Flow::Skipped);
        }
        let meta = fs::symlink_metadata(src).map_err(|e| fail_at(src, e))?;
        match self.resolve(&meta, src, dst.clone())? {
            Resolved::Skip => Ok(Flow::Skipped),
            Resolved::Merge(dst) => {
                // Moving into an existing folder merges; nothing of it is undoable exactly.
                let rd = fs::read_dir(src).map_err(|e| fail_at(src, e))?;
                self.merging += 1;
                let mut cancelled = false;
                for entry in rd.flatten() {
                    let child = entry.path();
                    let target = dst.join(entry.file_name());
                    match self.move_into(&child, target) {
                        Ok(_) => {}
                        Err(Fail::Cancelled) => {
                            cancelled = true;
                            break;
                        }
                        Err(Fail::Msg(m)) => self.fail(m),
                    }
                }
                self.merging -= 1;
                if cancelled {
                    return Err(Fail::Cancelled);
                }
                let _ = fs::remove_dir(src); // only succeeds when everything moved
                Ok(Flow::Done)
            }
            Resolved::Free(target) => {
                self.move_to_free(src, &meta, &target)?;
                if self.merging == 0 {
                    self.moved.push((src.to_path_buf(), target));
                }
                Ok(Flow::Done)
            }
            Resolved::Replace(target, dst_is_dir) => {
                if dst_is_dir || meta.is_dir() {
                    self.check_removable(&target)?;
                    self.remove_tree(&target)?;
                }
                // File over file: rename replaces atomically.
                self.move_to_free(src, &meta, &target)?;
                Ok(Flow::Done)
            }
        }
    }

    /// Renames, or copies and deletes across filesystems.
    fn move_to_free(&mut self, src: &Path, meta: &fs::Metadata, dst: &Path) -> Result<(), Fail> {
        match fs::rename(src, dst) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => self.move_by_copy(src, meta, dst),
            Err(e) => Err(fail_at(src, e)),
        }
    }

    /// Copy, verify, then delete the source. Anything wrong keeps the source untouched.
    fn move_by_copy(&mut self, src: &Path, meta: &fs::Metadata, dst: &Path) -> Result<(), Fail> {
        self.p.bytes_total += measure(src).1;
        let failed_before = self.outcome.failed;
        if let Err(e) = self.copy_new(src, meta, dst) {
            if fs::symlink_metadata(dst).is_ok() {
                let _ = self.remove_tree(dst);
            }
            return Err(e);
        }
        if self.outcome.failed != failed_before || !verify_tree(src, dst) {
            let _ = self.remove_tree(dst);
            return Err(fail_at(
                src,
                "the copy could not be verified, the original was kept",
            ));
        }
        self.remove_tree(src)
    }

    // ---- delete and trash ----

    fn check_removable(&self, path: &Path) -> Result<(), Fail> {
        if is_mount_point(path) {
            return Err(fail_at(path, "refusing to remove a mount point"));
        }
        Ok(())
    }

    /// Removes a tree without following symlinks.
    fn remove_tree(&mut self, path: &Path) -> Result<(), Fail> {
        self.check_cancel()?;
        let meta = fs::symlink_metadata(path).map_err(|e| fail_at(path, e))?;
        if meta.is_dir() {
            let rd = fs::read_dir(path).map_err(|e| fail_at(path, e))?;
            for entry in rd {
                let entry = entry.map_err(|e| fail_at(path, e))?;
                self.remove_tree(&entry.path())?;
            }
            fs::remove_dir(path).map_err(|e| fail_at(path, e))
        } else {
            fs::remove_file(path).map_err(|e| fail_at(path, e))
        }
    }

    fn trash_one(&mut self, path: &Path) -> Result<Flow, Fail> {
        self.check_removable(path)?;
        let Some(root) = self.env.trash_root.clone() else {
            return Err(fail_at(path, "no trash directory (HOME is not set)"));
        };
        let item = trash::trash(&root, path, &LocalTime::now()).map_err(|e| fail_at(path, e))?;
        self.trashed.push(item);
        Ok(Flow::Done)
    }

    // ---- rename, create, undo ----

    fn rename_one(&mut self, src: &Path, new_name: &OsStr) -> Result<Flow, Fail> {
        validate_name(new_name).map_err(|e| fail_at(src, e))?;
        self.check_removable(src)?;
        let dst = src.with_file_name(new_name);
        if dst == src {
            return Ok(Flow::Skipped);
        }
        if fs::symlink_metadata(&dst).is_ok() {
            return Err(fail_at(&dst, "already exists"));
        }
        fs::rename(src, &dst).map_err(|e| fail_at(src, e))?;
        self.outcome.undo = Some(Undo::Rename {
            from: src.to_path_buf(),
            to: dst,
        });
        Ok(Flow::Done)
    }

    fn create(&mut self, parent: &Path, name: &OsStr, dir: bool) {
        self.p.current = name.to_string_lossy().into_owned();
        let target = parent.join(name);
        let result = validate_name(name)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                if dir {
                    DirBuilder::new().mode(0o777).create(&target)
                } else {
                    OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o666)
                        .open(&target)
                        .map(drop)
                }
                .map_err(|e| e.to_string())
            });
        match result {
            Ok(()) => self.outcome.ok += 1,
            Err(e) => self.fail(format!("{}: {e}", shown(&target))),
        }
        self.p.items_done += 1;
    }

    fn undo(&mut self, undo: &Undo) {
        let mut results: Vec<Result<(), String>> = Vec::new();
        match undo {
            Undo::Trash(items) => {
                for i in items {
                    results.push(
                        trash::restore(i).map_err(|e| format!("{}: {e}", shown(&i.original))),
                    );
                }
            }
            Undo::Rename { from, to } => results.push(undo_move(to, from)),
            Undo::Move(items) => {
                for (orig, now) in items.iter().rev() {
                    results.push(undo_move(now, orig));
                }
            }
        }
        for r in results {
            match r {
                Ok(()) => self.outcome.ok += 1,
                Err(e) => self.fail(e),
            }
            self.p.items_done += 1;
        }
    }
}

enum Resolved {
    Skip,
    /// Both are directories: copy/move the children in.
    Merge(PathBuf),
    /// The final path is free.
    Free(PathBuf),
    /// Overwrite the existing path (`bool`: it is a directory).
    Replace(PathBuf, bool),
}

fn undo_move(now: &Path, original: &Path) -> Result<(), String> {
    if fs::symlink_metadata(original).is_ok() {
        return Err(format!("{}: already exists", shown(original)));
    }
    fs::rename(now, original).map_err(|e| format!("{}: {e}", shown(now)))
}

/// `name (copy).ext` beside `path`, the first one that is free.
pub fn sibling_copy(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default();
    dir.join(unique_copy_name(name, |n| {
        fs::symlink_metadata(dir.join(n)).is_ok()
    }))
}

fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".aurora-tmp-{}", std::process::id()));
    path.with_file_name(name)
}

/// Same structure, file sizes and symlink targets under both roots.
pub fn verify_tree(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::symlink_metadata(a), fs::symlink_metadata(b)) else {
        return false;
    };
    let (ta, tb) = (ma.file_type(), mb.file_type());
    if ta.is_symlink() || tb.is_symlink() {
        return ta.is_symlink()
            && tb.is_symlink()
            && fs::read_link(a).ok() == fs::read_link(b).ok();
    }
    if ta.is_dir() != tb.is_dir() {
        return false;
    }
    if !ta.is_dir() {
        return ma.len() == mb.len();
    }
    let (Ok(ra), Ok(rb)) = (fs::read_dir(a), fs::read_dir(b)) else {
        return false;
    };
    let names = |rd: fs::ReadDir| -> Option<Vec<OsString>> {
        let mut v = rd
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        v.sort();
        Some(v)
    };
    let (Some(na), Some(nb)) = (names(ra), names(rb)) else {
        return false;
    };
    na == nb && na.iter().all(|n| verify_tree(&a.join(n), &b.join(n)))
}

#[cfg(test)]
mod tests;
