//! FreeDesktop trash (spec 1.0), home volume only: `$XDG_DATA_HOME/Trash/{files,info}` with
//! a `.trashinfo` per item. A file on another device than the trash is refused with
//! [`TrashError::CrossDevice`]; it is never copied and never deleted for good instead.
//!
//! Naming, `.trashinfo` text and path encoding are pure; [`trash`] and [`restore`] are the
//! only parts that touch the disk.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::names::unique_trash_name;
use crate::uri::{percent_decode, percent_encode};

/// Broken-down local time for `DeletionDate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

impl LocalTime {
    /// The current local time (libc `localtime_r`).
    pub fn now() -> Self {
        // Safety: `time` accepts a null out-pointer.
        Self::from_unix(unsafe { libc::time(std::ptr::null_mut()) } as i64)
    }

    /// Local time of a unix timestamp (libc `localtime_r`); the epoch if it cannot be
    /// converted.
    pub fn from_unix(secs: i64) -> Self {
        let t = secs as libc::time_t;
        // Safety: a zeroed `tm` is a valid out-parameter and `localtime_r` is thread safe.
        let tm = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&t, &mut tm).is_null() {
                return Self {
                    year: 1970,
                    month: 1,
                    day: 1,
                    hour: 0,
                    min: 0,
                    sec: 0,
                };
            }
            tm
        };
        Self {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1) as u32,
            day: tm.tm_mday as u32,
            hour: tm.tm_hour as u32,
            min: tm.tm_min as u32,
            sec: tm.tm_sec as u32,
        }
    }

    /// `YYYY-MM-DDThh:mm:ss` as the spec wants it.
    pub fn format(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.min, self.sec
        )
    }
}

/// `$XDG_DATA_HOME/Trash`, or `$HOME/.local/share/Trash`. Relative values are ignored, as
/// the basedir spec says.
pub fn trash_root(data_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let abs = |v: Option<&OsStr>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let base = abs(data_home).or_else(|| abs(home).map(|h| h.join(".local/share")))?;
    Some(base.join("Trash"))
}

/// [`trash_root`] from the process environment.
pub fn trash_root_from_env() -> Option<PathBuf> {
    trash_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Contents of the `.trashinfo` for a file originally at `original` (absolute).
pub fn info_contents(original: &Path, when: &LocalTime) -> String {
    use std::os::unix::ffi::OsStrExt;
    format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(original.as_os_str().as_bytes()),
        when.format()
    )
}

/// Reads `Path=` and `DeletionDate=` back from a `.trashinfo`.
pub fn parse_info(text: &str) -> Option<(PathBuf, String)> {
    use std::os::unix::ffi::OsStringExt;
    let mut lines = text.lines();
    if lines.next()?.trim() != "[Trash Info]" {
        return None;
    }
    let (mut path, mut date) = (None, None);
    for l in lines {
        if let Some(v) = l.strip_prefix("Path=") {
            path = Some(PathBuf::from(OsString::from_vec(percent_decode(v))));
        } else if let Some(v) = l.strip_prefix("DeletionDate=") {
            date = Some(v.trim().to_string());
        }
    }
    Some((path?, date?))
}

/// An item moved to the trash; enough to put it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedItem {
    pub original: PathBuf,
    pub trashed: PathBuf,
    pub info: PathBuf,
}

#[derive(Debug)]
pub enum TrashError {
    Io(io::Error),
    /// The file is not on the trash's device.
    CrossDevice,
    NotAbsolute,
    /// The path is already inside the trash.
    InsideTrash,
}

impl fmt::Display for TrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrashError::Io(e) => write!(f, "{e}"),
            TrashError::CrossDevice => f.write_str(
                "cannot move to the trash: the file is on another filesystem (use Shift+Delete to delete it permanently)",
            ),
            TrashError::NotAbsolute => f.write_str("the path is not absolute"),
            TrashError::InsideTrash => f.write_str("the file is already in the trash"),
        }
    }
}

impl From<io::Error> for TrashError {
    fn from(e: io::Error) -> Self {
        TrashError::Io(e)
    }
}

fn private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// Moves `path` into the trash at `root`. The file itself is never followed if it is a
/// symlink; the symlink is what gets trashed.
pub fn trash(root: &Path, path: &Path, when: &LocalTime) -> Result<TrashedItem, TrashError> {
    if !path.is_absolute() {
        return Err(TrashError::NotAbsolute);
    }
    if path.starts_with(root) {
        return Err(TrashError::InsideTrash);
    }
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the path has no file name"))?;
    let files = root.join("files");
    let infos = root.join("info");
    private_dir(&files)?;
    private_dir(&infos)?;
    if fs::symlink_metadata(path)?.dev() != fs::metadata(&files)?.dev() {
        return Err(TrashError::CrossDevice);
    }

    // The `.trashinfo` is created exclusively first: that reserves the name even when
    // another process trashes a file of the same name at the same time.
    let content = info_contents(path, when);
    let mut attempt = 0;
    loop {
        let chosen = unique_trash_name(name, |n| {
            fs::symlink_metadata(files.join(n)).is_ok()
                || fs::symlink_metadata(infos.join(info_name(n))).is_ok()
        });
        let info = infos.join(info_name(&chosen));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&info)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(content.as_bytes()) {
                    let _ = fs::remove_file(&info);
                    return Err(e.into());
                }
                let trashed = files.join(&chosen);
                if let Err(e) = fs::rename(path, &trashed) {
                    let _ = fs::remove_file(&info);
                    return Err(e.into());
                }
                return Ok(TrashedItem {
                    original: path.to_path_buf(),
                    trashed,
                    info,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 100 => attempt += 1,
            Err(e) => return Err(e.into()),
        }
    }
}

fn info_name(name: &OsStr) -> OsString {
    let mut n = name.to_os_string();
    n.push(".trashinfo");
    n
}

/// Puts a trashed item back where it came from. Refuses to overwrite anything.
pub fn restore(item: &TrashedItem) -> io::Result<()> {
    if fs::symlink_metadata(&item.original).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", item.original.display()),
        ));
    }
    fs::rename(&item.trashed, &item.original)?;
    let _ = fs::remove_file(&item.info);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> LocalTime {
        LocalTime {
            year: 2026,
            month: 3,
            day: 7,
            hour: 9,
            min: 5,
            sec: 1,
        }
    }

    #[test]
    fn root_follows_the_basedir_rules() {
        let s = |v: &str| OsString::from(v);
        assert_eq!(
            trash_root(Some(&s("/d")), Some(&s("/h"))),
            Some(PathBuf::from("/d/Trash"))
        );
        assert_eq!(
            trash_root(None, Some(&s("/h"))),
            Some(PathBuf::from("/h/.local/share/Trash"))
        );
        assert_eq!(
            trash_root(Some(&s("rel")), Some(&s("/h"))),
            Some(PathBuf::from("/h/.local/share/Trash"))
        );
        assert_eq!(trash_root(None, Some(&s("rel"))), None);
        assert_eq!(trash_root(None, None), None);
    }

    #[test]
    fn info_text_is_encoded_and_parses_back() {
        let p = Path::new("/home/u/My Docs/a%b é.txt");
        let text = info_contents(p, &t());
        assert_eq!(
            text,
            "[Trash Info]\nPath=/home/u/My%20Docs/a%25b%20%C3%A9.txt\nDeletionDate=2026-03-07T09:05:01\n"
        );
        assert_eq!(
            parse_info(&text),
            Some((p.to_path_buf(), "2026-03-07T09:05:01".to_string()))
        );
        assert_eq!(parse_info("nonsense"), None);
        assert_eq!(parse_info("[Trash Info]\nPath=/x\n"), None);
    }

    #[test]
    fn trash_and_restore_in_a_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Trash");
        let a = dir.path().join("a.txt");
        fs::write(&a, "one").unwrap();
        let item = trash(&root, &a, &t()).unwrap();
        assert!(!a.exists());
        assert_eq!(item.trashed, root.join("files/a.txt"));
        let info = fs::read_to_string(root.join("info/a.txt.trashinfo")).unwrap();
        assert_eq!(parse_info(&info).unwrap().0, a);

        // Same name again gets a distinct slot.
        fs::write(&a, "two").unwrap();
        let second = trash(&root, &a, &t()).unwrap();
        assert_eq!(second.trashed, root.join("files/a.2.txt"));
        assert!(root.join("info/a.2.txt.trashinfo").exists());

        // Restoring over an existing file is refused, then works once it is free.
        fs::write(&a, "three").unwrap();
        assert!(restore(&item).is_err());
        assert!(item.trashed.exists(), "nothing moved on refusal");
        fs::remove_file(&a).unwrap();
        restore(&item).unwrap();
        assert_eq!(fs::read_to_string(&a).unwrap(), "one");
        assert!(!item.info.exists());
    }

    #[test]
    fn symlinks_are_trashed_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Trash");
        let target = dir.path().join("target");
        fs::write(&target, "x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let item = trash(&root, &link, &t()).unwrap();
        assert!(target.exists());
        assert!(fs::symlink_metadata(&item.trashed).unwrap().is_symlink());
    }

    #[test]
    fn refuses_relative_and_already_trashed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Trash");
        assert!(matches!(
            trash(&root, Path::new("rel"), &t()),
            Err(TrashError::NotAbsolute)
        ));
        let a = dir.path().join("a");
        fs::write(&a, "").unwrap();
        let item = trash(&root, &a, &t()).unwrap();
        assert!(matches!(
            trash(&root, &item.trashed, &t()),
            Err(TrashError::InsideTrash)
        ));
    }
}
