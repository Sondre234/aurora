//! Reading directories: the thin `fs` layer and the worker thread that does it off the
//! event loop. Results carry the generation of the request, so the loop drops answers for
//! a directory the user already left.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::model::{Entry, Kind, Sort, sort_entries};

/// Below this many entries a re-sort is done in place on the loop thread.
pub const INLINE_SORT_LIMIT: usize = 5_000;

/// Reads one directory, unsorted. Entries that vanish or cannot be stat'ed while reading are
/// skipped; an unreadable directory is the error.
pub fn read_dir(path: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in fs::read_dir(path)? {
        let Ok(item) = item else { continue };
        let Ok(meta) = item.metadata() else { continue };
        out.push(entry_from(item.file_name(), &item.path(), &meta));
    }
    Ok(out)
}

fn entry_from(name: std::ffi::OsString, path: &Path, meta: &fs::Metadata) -> Entry {
    let ft = meta.file_type();
    let mut e = if ft.is_symlink() {
        match fs::metadata(path) {
            Ok(target) => {
                let mut e = Entry::new(name, Kind::Symlink);
                e.is_dir = target.is_dir();
                e.size = if target.is_dir() { 0 } else { target.len() };
                e.mtime = target.mtime();
                e.mode = target.mode();
                return e;
            }
            Err(_) => Entry::new(name, Kind::Broken),
        }
    } else if ft.is_dir() {
        Entry::new(name, Kind::Dir)
    } else if ft.is_file() {
        Entry::new(name, Kind::File)
    } else {
        Entry::new(name, Kind::Other)
    };
    e.size = if ft.is_dir() { 0 } else { meta.len() };
    e.mtime = meta.mtime();
    e.mode = meta.mode();
    e
}

/// Human wording of a directory error.
pub fn describe_error(path: &Path, err: &io::Error) -> String {
    let what = match err.kind() {
        io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
        io::ErrorKind::NotFound => "No such folder".to_string(),
        _ if err.raw_os_error() == Some(libc::ENOTDIR) => "Not a folder".to_string(),
        _ => err.to_string(),
    };
    format!("Cannot open {}: {what}", path.display())
}

pub enum Job {
    /// Read and sort a directory.
    Load {
        generation: u64,
        path: PathBuf,
        sort: Sort,
    },
    /// Sort entries the loop handed over.
    Sort {
        generation: u64,
        entries: Vec<Entry>,
        sort: Sort,
    },
}

/// The worker's answer.
pub struct Listed {
    pub generation: u64,
    /// The directory for a load; empty for a re-sort.
    pub path: PathBuf,
    pub result: Result<Vec<Entry>, String>,
}

/// A thread that reads and sorts. Only the newest queued job is worth doing, so older ones
/// are dropped when several are waiting.
pub struct Worker {
    tx: mpsc::Sender<Job>,
}

impl Worker {
    /// `on_done` runs on the worker thread; forward the result into the event loop.
    pub fn spawn(on_done: impl Fn(Listed) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("files-list".into())
            .spawn(move || {
                while let Ok(mut job) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        job = newer;
                    }
                    on_done(do_job(job));
                }
            });
        if let Err(err) = spawned {
            tracing::warn!("files: cannot start the listing thread: {err}");
        }
        Self { tx }
    }

    pub fn submit(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

fn do_job(job: Job) -> Listed {
    match job {
        Job::Load {
            generation,
            path,
            sort,
        } => {
            let result = match read_dir(&path) {
                Ok(mut entries) => {
                    sort_entries(&mut entries, sort);
                    Ok(entries)
                }
                Err(e) => Err(describe_error(&path, &e)),
            };
            Listed {
                generation,
                path,
                result,
            }
        }
        Job::Sort {
            generation,
            mut entries,
            sort,
        } => {
            sort_entries(&mut entries, sort);
            Listed {
                generation,
                path: PathBuf::new(),
                result: Ok(entries),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn reads_kinds_sizes_and_hidden_flags() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("f.txt"), "12345").unwrap();
        fs::write(p.join(".hidden"), "").unwrap();
        fs::create_dir(p.join("d")).unwrap();
        symlink(p.join("d"), p.join("to-dir")).unwrap();
        symlink(p.join("f.txt"), p.join("to-file")).unwrap();
        symlink(p.join("nothing"), p.join("broken")).unwrap();
        let mut es = read_dir(p).unwrap();
        sort_entries(&mut es, Sort::default());
        let find = |n: &str| es.iter().find(|e| e.name == n).unwrap().clone();
        assert_eq!(find("f.txt").size, 5);
        assert_eq!(find("f.txt").kind, Kind::File);
        assert!(find(".hidden").hidden);
        assert_eq!(find("d").kind, Kind::Dir);
        let l = find("to-dir");
        assert_eq!((l.kind, l.is_dir), (Kind::Symlink, true));
        let l = find("to-file");
        assert_eq!((l.kind, l.is_dir, l.size), (Kind::Symlink, false, 5));
        assert_eq!(find("broken").kind, Kind::Broken);
        assert!(!find("broken").is_dir);
        // Directories sort first.
        assert!(es[0].is_dir);
        assert_eq!(es.len(), 6);
    }

    #[test]
    fn unreadable_and_missing_directories_are_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read anything, so only assert when the kernel really refuses.
        if let Err(e) = read_dir(&locked) {
            assert!(describe_error(&locked, &e).contains("Permission denied"));
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let e = read_dir(&dir.path().join("missing")).unwrap_err();
        assert!(describe_error(Path::new("/missing"), &e).contains("No such folder"));
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        let e = read_dir(&file).unwrap_err();
        assert!(describe_error(&file, &e).contains("Not a folder"), "{e}");
    }

    #[test]
    fn worker_answers_with_the_request_generation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b"), "").unwrap();
        fs::write(dir.path().join("a"), "").unwrap();
        let (tx, rx) = mpsc::channel();
        let w = Worker::spawn(move |l| {
            let _ = tx.send(l);
        });
        w.submit(Job::Load {
            generation: 7,
            path: dir.path().to_path_buf(),
            sort: Sort::default(),
        });
        let l = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(l.generation, 7);
        assert_eq!(l.path, dir.path());
        let names: Vec<_> = l.result.unwrap().iter().map(Entry::display_name).collect();
        assert_eq!(names, ["a", "b"]);

        let entries = vec![Entry::new("a", Kind::File), Entry::new("b", Kind::File)];
        w.submit(Job::Sort {
            generation: 8,
            entries,
            sort: Sort {
                descending: true,
                ..Sort::default()
            },
        });
        let l = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(l.generation, 8);
        assert_eq!(l.result.unwrap()[0].display_name(), "b");
    }
}
