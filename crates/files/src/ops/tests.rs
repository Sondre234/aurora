//! Executor tests. Every path lives under a `tempfile` directory the test creates; nothing
//! here touches the real home, trash or any other user file.

use std::cell::Cell;
use std::collections::VecDeque;
use std::os::unix::fs::symlink;

use super::*;

/// Answers conflicts from a script and records what was asked.
struct Script {
    answers: VecDeque<Resolution>,
    asked: Vec<(PathBuf, PathBuf)>,
    cancel: Cell<bool>,
    progress_calls: usize,
}

impl Script {
    fn new(answers: &[Resolution]) -> Self {
        Self {
            answers: answers.iter().copied().collect(),
            asked: Vec::new(),
            cancel: Cell::new(false),
            progress_calls: 0,
        }
    }
}

impl Reporter for Script {
    fn progress(&mut self, _: &Progress) {
        self.progress_calls += 1;
    }
    fn conflict(&mut self, src: &Path, dst: &Path) -> Resolution {
        self.asked.push((src.to_path_buf(), dst.to_path_buf()));
        self.answers
            .pop_front()
            .expect("unexpected conflict question")
    }
    fn cancelled(&self) -> bool {
        self.cancel.get()
    }
}

fn answer(choice: Choice, apply_all: bool) -> Resolution {
    Resolution { choice, apply_all }
}

struct Sandbox {
    dir: tempfile::TempDir,
    env: Env,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir(&home).unwrap();
        let env = Env {
            home: Some(home.clone()),
            trash_root: Some(home.join(".local/share/Trash")),
        };
        Self { dir, env }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        p
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let p = self.p(rel);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn run(&self, op: &Op, script: &mut Script) -> Outcome {
        run(op, &self.env, script)
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.p(rel)).unwrap()
    }
}

#[test]
fn copy_keeps_content_mode_and_mtime() {
    let sb = Sandbox::new();
    let src = sb.write("a/f.txt", "hello");
    fs::set_permissions(&src, fs::Permissions::from_mode(0o750)).unwrap();
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let dest = sb.dir("b");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Copy {
            srcs: vec![src.clone()],
            dest: dest.clone(),
        },
        &mut s,
    );
    assert_eq!((out.ok, out.failed), (1, 0), "{:?}", out.errors);
    let copy = dest.join("f.txt");
    assert_eq!(fs::read_to_string(&copy).unwrap(), "hello");
    let m = fs::metadata(&copy).unwrap();
    assert_eq!(m.mode() & 0o7777, 0o750);
    assert_eq!(m.modified().unwrap(), old);
    assert!(src.exists(), "copy leaves the source");
    assert!(s.progress_calls >= 1);
}

#[test]
fn copy_tree_preserves_symlinks_as_symlinks() {
    let sb = Sandbox::new();
    sb.write("t/sub/x", "x");
    sb.write("outside/secret", "s");
    symlink("../sub/x", sb.p("t/rel-link")).unwrap();
    symlink(sb.p("outside"), sb.p("t/dir-link")).unwrap();
    symlink("/nowhere/at/all", sb.p("t/dangling")).unwrap();
    let dest = sb.dir("d");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Copy {
            srcs: vec![sb.p("t")],
            dest: dest.clone(),
        },
        &mut s,
    );
    assert_eq!((out.ok, out.failed), (1, 0), "{:?}", out.errors);
    let t = dest.join("t");
    assert_eq!(fs::read_to_string(t.join("sub/x")).unwrap(), "x");
    for (name, target) in [
        ("rel-link", sb.p("t/rel-link")),
        ("dir-link", sb.p("t/dir-link")),
        ("dangling", sb.p("t/dangling")),
    ] {
        let copied = fs::symlink_metadata(t.join(name)).unwrap();
        assert!(copied.is_symlink(), "{name} must stay a symlink");
        assert_eq!(
            fs::read_link(t.join(name)).unwrap(),
            fs::read_link(target).unwrap()
        );
    }
    assert!(verify_tree(&sb.p("t"), &t));
}

#[test]
fn read_only_directories_are_copied_whole() {
    let sb = Sandbox::new();
    sb.write("ro/inner", "i");
    fs::set_permissions(sb.p("ro"), fs::Permissions::from_mode(0o555)).unwrap();
    let dest = sb.dir("d");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Copy {
            srcs: vec![sb.p("ro")],
            dest: dest.clone(),
        },
        &mut s,
    );
    assert_eq!(out.failed, 0, "{:?}", out.errors);
    assert_eq!(fs::read_to_string(dest.join("ro/inner")).unwrap(), "i");
    assert_eq!(
        fs::metadata(dest.join("ro")).unwrap().mode() & 0o7777,
        0o555
    );
    // Let the tempdir clean up.
    fs::set_permissions(sb.p("ro"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(dest.join("ro"), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn conflict_skip_replace_keep_both() {
    let sb = Sandbox::new();
    let src = sb.write("s/a.txt", "new");
    let dest = sb.write("d/a.txt", "old");
    let op = Op::Copy {
        srcs: vec![src],
        dest: sb.p("d"),
    };

    let mut s = Script::new(&[answer(Choice::Skip, false)]);
    let out = sb.run(&op, &mut s);
    assert_eq!(s.asked.len(), 1);
    assert_eq!((out.ok, out.failed), (1, 0));
    assert_eq!(fs::read_to_string(&dest).unwrap(), "old");

    let mut s = Script::new(&[answer(Choice::KeepBoth, false)]);
    sb.run(&op, &mut s);
    assert_eq!(fs::read_to_string(&dest).unwrap(), "old");
    assert_eq!(sb.read("d/a (copy).txt"), "new");

    let mut s = Script::new(&[answer(Choice::KeepBoth, false)]);
    sb.run(&op, &mut s);
    assert_eq!(sb.read("d/a (copy 2).txt"), "new");

    let mut s = Script::new(&[answer(Choice::Replace, false)]);
    sb.run(&op, &mut s);
    assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
    let leftovers: Vec<_> = fs::read_dir(sb.p("d"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(
        leftovers
            .iter()
            .all(|n| !n.to_string_lossy().contains("aurora-tmp")),
        "{leftovers:?}"
    );
}

#[test]
fn apply_to_all_asks_once() {
    let sb = Sandbox::new();
    let srcs: Vec<PathBuf> = ["a", "b", "c"]
        .iter()
        .map(|n| sb.write(&format!("s/{n}"), "new"))
        .collect();
    for n in ["a", "b", "c"] {
        sb.write(&format!("d/{n}"), "old");
    }
    let mut s = Script::new(&[answer(Choice::Skip, true)]);
    let out = sb.run(
        &Op::Copy {
            srcs,
            dest: sb.p("d"),
        },
        &mut s,
    );
    assert_eq!(s.asked.len(), 1);
    assert_eq!(out.ok, 3);
    assert_eq!(sb.read("d/b"), "old");
}

#[test]
fn cancelling_a_conflict_stops_the_operation() {
    let sb = Sandbox::new();
    let srcs = vec![sb.write("s/a", "1"), sb.write("s/b", "2")];
    sb.write("d/a", "old");
    let mut s = Script::new(&[answer(Choice::Cancel, false)]);
    let out = sb.run(
        &Op::Copy {
            srcs,
            dest: sb.p("d"),
        },
        &mut s,
    );
    assert!(out.cancelled);
    assert!(!sb.p("d/b").exists(), "nothing after the cancel ran");
    assert_eq!(sb.read("d/a"), "old");
}

#[test]
fn cancel_flag_stops_between_items() {
    let sb = Sandbox::new();
    let srcs = vec![sb.write("s/a", "1"), sb.write("s/b", "2")];
    let dest = sb.dir("d");
    let mut s = Script::new(&[]);
    s.cancel.set(true);
    let out = sb.run(
        &Op::Copy {
            srcs,
            dest: dest.clone(),
        },
        &mut s,
    );
    assert!(out.cancelled && out.ok == 0);
    assert_eq!(fs::read_dir(dest).unwrap().count(), 0);
}

#[test]
fn copying_onto_itself_makes_a_sibling_copy() {
    let sb = Sandbox::new();
    let f = sb.write("d/a.txt", "x");
    sb.write("d/sub/y", "y");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Copy {
            srcs: vec![f, sb.p("d/sub")],
            dest: sb.p("d"),
        },
        &mut s,
    );
    assert_eq!((out.ok, out.failed), (2, 0), "{:?}", out.errors);
    assert!(s.asked.is_empty());
    assert_eq!(sb.read("d/a (copy).txt"), "x");
    assert_eq!(sb.read("d/sub (copy)/y"), "y");
}

#[test]
fn a_folder_cannot_go_into_itself() {
    let sb = Sandbox::new();
    sb.write("a/b/f", "x");
    let mut s = Script::new(&[]);
    for op in [
        Op::Copy {
            srcs: vec![sb.p("a")],
            dest: sb.p("a/b"),
        },
        Op::Move {
            srcs: vec![sb.p("a")],
            dest: sb.p("a"),
        },
    ] {
        let out = sb.run(&op, &mut s);
        assert_eq!((out.ok, out.failed), (0, 1), "{op:?}");
        assert!(out.errors[0].contains("inside itself"), "{:?}", out.errors);
    }
    assert!(sb.p("a/b/f").exists());
    assert!(!sb.p("a/b/a").exists());
}

#[test]
fn folders_merge_on_copy_and_move() {
    let sb = Sandbox::new();
    sb.write("s/dir/new", "n");
    sb.write("s/dir/both", "src");
    sb.write("d/dir/old", "o");
    sb.write("d/dir/both", "dst");
    let mut s = Script::new(&[answer(Choice::Replace, false)]);
    let out = sb.run(
        &Op::Move {
            srcs: vec![sb.p("s/dir")],
            dest: sb.p("d"),
        },
        &mut s,
    );
    assert_eq!(out.failed, 0, "{:?}", out.errors);
    assert_eq!(
        s.asked.len(),
        1,
        "only the file conflict is asked, not the folder"
    );
    assert_eq!(sb.read("d/dir/new"), "n");
    assert_eq!(sb.read("d/dir/old"), "o");
    assert_eq!(sb.read("d/dir/both"), "src");
    assert!(
        !sb.p("s/dir").exists(),
        "an emptied source folder is removed"
    );
    assert!(out.undo.is_none(), "merges are not undoable");
}

#[test]
fn move_renames_and_undoes() {
    let sb = Sandbox::new();
    let f = sb.write("s/a.txt", "x");
    let dir = sb.write("s/d/inner", "i");
    let dest = sb.dir("t");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Move {
            srcs: vec![f.clone(), sb.p("s/d")],
            dest: dest.clone(),
        },
        &mut s,
    );
    assert_eq!((out.ok, out.failed), (2, 0), "{:?}", out.errors);
    assert!(!f.exists() && !dir.exists());
    assert_eq!(sb.read("t/a.txt"), "x");
    let undo = out.undo.expect("move is undoable");
    let back = sb.run(&Op::Undo(undo), &mut s);
    assert_eq!((back.ok, back.failed), (2, 0), "{:?}", back.errors);
    assert_eq!(fs::read_to_string(&f).unwrap(), "x");
    assert!(dir.exists() && !sb.p("t/a.txt").exists());
}

#[test]
fn move_onto_the_same_place_is_a_noop() {
    let sb = Sandbox::new();
    let f = sb.write("s/a.txt", "x");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Move {
            srcs: vec![f.clone()],
            dest: sb.p("s"),
        },
        &mut s,
    );
    assert_eq!(out.failed, 0);
    assert_eq!(fs::read_to_string(f).unwrap(), "x");
    assert!(out.undo.is_none());
}

#[test]
fn the_cross_device_fallback_copies_verifies_and_deletes() {
    let sb = Sandbox::new();
    sb.write("s/tree/a", "a");
    sb.write("s/tree/sub/b", "bb");
    symlink("a", sb.p("s/tree/l")).unwrap();
    let meta = fs::symlink_metadata(sb.p("s/tree")).unwrap();
    let mut s = Script::new(&[]);
    let mut x = Exec {
        rep: &mut s,
        env: &sb.env,
        sticky: None,
        p: Progress::default(),
        last: Instant::now(),
        outcome: Outcome::default(),
        moved: Vec::new(),
        trashed: Vec::new(),
        merging: 0,
    };
    assert!(
        x.move_by_copy(&sb.p("s/tree"), &meta, &sb.p("moved"))
            .is_ok()
    );
    assert!(!sb.p("s/tree").exists());
    assert_eq!(sb.read("moved/sub/b"), "bb");
    assert!(fs::symlink_metadata(sb.p("moved/l")).unwrap().is_symlink());
}

#[test]
fn verification_notices_differences() {
    let sb = Sandbox::new();
    sb.write("a/f", "12");
    sb.write("b/f", "123");
    assert!(!verify_tree(&sb.p("a"), &sb.p("b")));
    sb.write("b/f", "12");
    assert!(verify_tree(&sb.p("a"), &sb.p("b")));
    sb.write("b/extra", "");
    assert!(!verify_tree(&sb.p("a"), &sb.p("b")));
}

#[test]
fn delete_never_follows_symlinks() {
    let sb = Sandbox::new();
    sb.write("outside/precious", "keep");
    sb.write("victim/file", "x");
    symlink(sb.p("outside"), sb.p("victim/link")).unwrap();
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Delete {
            srcs: vec![sb.p("victim")],
        },
        &mut s,
    );
    assert_eq!((out.ok, out.failed), (1, 0), "{:?}", out.errors);
    assert!(!sb.p("victim").exists());
    assert_eq!(sb.read("outside/precious"), "keep");

    // A symlink to a directory given directly is removed, the directory stays.
    symlink(sb.p("outside"), sb.p("direct")).unwrap();
    let out = sb.run(
        &Op::Delete {
            srcs: vec![sb.p("direct")],
        },
        &mut s,
    );
    assert_eq!(out.ok, 1);
    assert!(sb.p("outside/precious").exists());
}

#[test]
fn home_and_root_are_refused_for_every_operation() {
    let sb = Sandbox::new();
    let home = sb.env.home.clone().unwrap();
    sb.write("home/doc", "d");
    let dest = sb.dir("elsewhere");
    let mut s = Script::new(&[]);
    let ops = [
        Op::Delete {
            srcs: vec![home.clone()],
        },
        Op::Trash {
            srcs: vec![home.clone()],
        },
        Op::Copy {
            srcs: vec![home.clone()],
            dest: dest.clone(),
        },
        Op::Move {
            srcs: vec![home.clone()],
            dest: dest.clone(),
        },
        Op::Rename {
            src: home.clone(),
            new_name: "x".into(),
        },
        // Spelled differently: a dot-dot detour. (The real root is only checked through
        // `check_source`, never handed to an executor in a test.)
        Op::Delete {
            srcs: vec![home.join("..").join("home")],
        },
    ];
    for op in ops {
        let out = sb.run(&op, &mut s);
        assert_eq!((out.ok, out.failed), (0, 1), "{op:?} {:?}", out.errors);
        assert!(out.errors[0].contains("refusing"), "{:?}", out.errors);
    }
    assert!(home.join("doc").exists());
    // Files inside home are of course fine.
    let out = sb.run(
        &Op::Delete {
            srcs: vec![home.join("doc")],
        },
        &mut s,
    );
    assert_eq!(out.ok, 1);
}

#[test]
fn guards_in_isolation() {
    let sb = Sandbox::new();
    let home = sb.env.home.clone().unwrap();
    assert!(check_source(Path::new("/"), None).is_err());
    assert!(check_source(&home, Some(&home)).is_err());
    assert!(check_source(&home.join("nothing"), Some(&home)).is_ok());
    assert!(
        check_source(&home, None).is_ok(),
        "without a home nothing else is protected"
    );
    // A symlink pointing at home is a different file.
    symlink(&home, sb.p("lnk")).unwrap();
    assert!(check_source(&sb.p("lnk"), Some(&home)).is_ok());
    assert!(is_mount_point(Path::new("/")));
    assert!(!is_mount_point(&home));
    assert!(!is_mount_point(&sb.p("missing")));
    assert!(!dest_inside_source(&sb.p("missing"), &home));
}

#[test]
fn transfer_planning() {
    let plan = plan_transfers(
        &[PathBuf::from("/a/x"), PathBuf::from("/b/y.txt")],
        Path::new("/d"),
    )
    .unwrap();
    assert_eq!(
        plan,
        vec![
            (PathBuf::from("/a/x"), PathBuf::from("/d/x")),
            (PathBuf::from("/b/y.txt"), PathBuf::from("/d/y.txt")),
        ]
    );
    assert!(plan_transfers(&[PathBuf::from("/")], Path::new("/d")).is_err());
}

#[test]
fn trash_and_undo() {
    let sb = Sandbox::new();
    let f = sb.write("home/a.txt", "x");
    let d = sb.write("home/dir/inner", "i");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Trash {
            srcs: vec![f.clone(), sb.p("home/dir")],
        },
        &mut s,
    );
    assert_eq!((out.ok, out.failed), (2, 0), "{:?}", out.errors);
    assert!(!f.exists() && !d.exists());
    let root = sb.env.trash_root.clone().unwrap();
    assert!(root.join("files/a.txt").exists());
    assert!(root.join("info/a.txt.trashinfo").exists());
    assert!(root.join("files/dir/inner").exists());

    let back = sb.run(&Op::Undo(out.undo.unwrap()), &mut s);
    assert_eq!((back.ok, back.failed), (2, 0), "{:?}", back.errors);
    assert_eq!(fs::read_to_string(&f).unwrap(), "x");
    assert!(d.exists());
    assert!(!root.join("info/a.txt.trashinfo").exists());
}

#[test]
fn trash_without_a_trash_dir_fails_loudly() {
    let sb = Sandbox::new();
    let f = sb.write("home/a", "x");
    let env = Env {
        home: sb.env.home.clone(),
        trash_root: None,
    };
    let mut s = Script::new(&[]);
    let out = run(
        &Op::Trash {
            srcs: vec![f.clone()],
        },
        &env,
        &mut s,
    );
    assert_eq!(out.failed, 1);
    assert!(f.exists(), "a failed trash never deletes");
}

#[test]
fn rename_and_undo() {
    let sb = Sandbox::new();
    let f = sb.write("d/a.txt", "x");
    sb.write("d/taken", "t");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Rename {
            src: f.clone(),
            new_name: "b.txt".into(),
        },
        &mut s,
    );
    assert_eq!(out.ok, 1);
    assert_eq!(sb.read("d/b.txt"), "x");
    let back = sb.run(&Op::Undo(out.undo.unwrap()), &mut s);
    assert_eq!(back.ok, 1);
    assert!(f.exists() && !sb.p("d/b.txt").exists());

    for bad in ["taken", "", "a/b", ".."] {
        let out = sb.run(
            &Op::Rename {
                src: f.clone(),
                new_name: bad.into(),
            },
            &mut s,
        );
        assert_eq!((out.ok, out.failed), (0, 1), "{bad:?}");
        assert!(f.exists());
    }
    // Same name: nothing to do, nothing to undo.
    let same = sb.run(
        &Op::Rename {
            src: f,
            new_name: "a.txt".into(),
        },
        &mut s,
    );
    assert!(same.undo.is_none() && same.failed == 0);
}

#[test]
fn undo_refuses_to_overwrite() {
    let sb = Sandbox::new();
    let f = sb.write("d/a", "1");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Rename {
            src: f.clone(),
            new_name: "b".into(),
        },
        &mut s,
    );
    fs::write(&f, "new occupant").unwrap();
    let back = sb.run(&Op::Undo(out.undo.unwrap()), &mut s);
    assert_eq!(back.failed, 1);
    assert_eq!(sb.read("d/a"), "new occupant");
    assert_eq!(sb.read("d/b"), "1");
}

#[test]
fn create_folder_and_file() {
    let sb = Sandbox::new();
    let parent = sb.dir("p");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Mkdir {
            parent: parent.clone(),
            name: "New folder".into(),
        },
        &mut s,
    );
    assert_eq!(out.ok, 1);
    assert!(sb.p("p/New folder").is_dir());
    let out = sb.run(
        &Op::Touch {
            parent: parent.clone(),
            name: "n.txt".into(),
        },
        &mut s,
    );
    assert_eq!(out.ok, 1);
    assert_eq!(fs::metadata(sb.p("p/n.txt")).unwrap().len(), 0);
    for op in [
        Op::Mkdir {
            parent: parent.clone(),
            name: "New folder".into(),
        },
        Op::Touch {
            parent: parent.clone(),
            name: "n.txt".into(),
        },
        Op::Mkdir {
            parent: parent.clone(),
            name: "a/b".into(),
        },
        Op::Touch {
            parent: parent.clone(),
            name: "".into(),
        },
    ] {
        let out = sb.run(&op, &mut s);
        assert_eq!((out.ok, out.failed), (0, 1), "{op:?}");
    }
    assert_eq!(fs::read_dir(parent).unwrap().count(), 2);
}

#[test]
fn failures_are_counted_per_item() {
    let sb = Sandbox::new();
    let good = sb.write("s/good", "g");
    let missing = sb.p("s/missing");
    let dest = sb.dir("d");
    let mut s = Script::new(&[]);
    let out = sb.run(
        &Op::Copy {
            srcs: vec![missing, good],
            dest: dest.clone(),
        },
        &mut s,
    );
    assert_eq!((out.ok, out.failed), (1, 1));
    assert!(out.errors[0].contains("missing"), "{:?}", out.errors);
    assert!(dest.join("good").exists());
}
