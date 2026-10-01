//! The sidebar: home, the XDG user directories, the trash, `/` and mounted volumes.
//! Parsing and selection rules are pure; only [`watch_mounts`] touches `/proc`.

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceKind {
    Home,
    UserDir,
    Trash,
    Root,
    Volume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
    pub kind: PlaceKind,
    /// Freedesktop icon name.
    pub icon: &'static str,
}

/// `XDG_*_DIR="$HOME/..."` lines of `user-dirs.dirs` as (key, path). Values must be
/// absolute or start with `$HOME/`.
pub fn parse_user_dirs(text: &str, home: &Path) -> Vec<(String, PathBuf)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (key, value) = line.split_once('=')?;
            let key = key.trim().strip_prefix("XDG_")?.strip_suffix("_DIR")?;
            let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
            let path = if value == "$HOME" {
                home.to_path_buf()
            } else if let Some(rest) = value.strip_prefix("$HOME/") {
                home.join(unescape_shell(rest))
            } else if value.starts_with('/') {
                PathBuf::from(unescape_shell(value))
            } else {
                return None;
            };
            Some((key.to_string(), path))
        })
        .collect()
}

/// Undoes the backslash escapes `xdg-user-dirs` writes (`\"`, `\\`, `\$`, `` \` ``).
fn unescape_shell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub source: String,
}

/// Decodes the octal escapes (`\040` for a space) mountinfo uses in paths.
pub fn unescape_mountinfo(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..=i + 3].iter().all(|d| (b'0'..=b'7').contains(d))
        {
            let v = (b[i + 1] - b'0') as u32 * 64
                + (b[i + 2] - b'0') as u32 * 8
                + (b[i + 3] - b'0') as u32;
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let dash = f.iter().position(|x| *x == "-")?;
            let mount_point =
                PathBuf::from(std::ffi::OsString::from_vec(unescape_mountinfo(f.get(4)?)));
            Some(Mount {
                mount_point,
                fs_type: f.get(dash + 1)?.to_string(),
                source: f.get(dash + 2)?.to_string(),
            })
        })
        .collect()
}

const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "cgroup",
    "cgroup2",
    "securityfs",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "pstore",
    "bpf",
    "mqueue",
    "hugetlbfs",
    "autofs",
    "binfmt_misc",
    "overlay",
    "squashfs",
    "ramfs",
    "efivarfs",
    "nsfs",
    "rpc_pipefs",
    "fuse.portal",
    "fuse.gvfsd-fuse",
    "fuse.xdg-document-portal",
    "selinuxfs",
];

const HIDDEN_PREFIXES: &[&str] = &[
    "/boot",
    "/var/lib",
    "/snap",
    "/sys",
    "/proc",
    "/dev",
    "/run/user",
    "/nix/store",
    "/tmp",
];

/// Mounts worth a sidebar entry: real filesystems outside system locations, never `/`.
pub fn is_volume(m: &Mount) -> bool {
    if PSEUDO_FS.contains(&m.fs_type.as_str()) || m.mount_point == Path::new("/") {
        return false;
    }
    !HIDDEN_PREFIXES.iter().any(|p| m.mount_point.starts_with(p))
}

fn user_dir_entry(key: &str) -> Option<(&'static str, &'static str)> {
    Some(match key {
        "DESKTOP" => ("Desktop", "user-desktop"),
        "DOCUMENTS" => ("Documents", "folder-documents"),
        "DOWNLOAD" => ("Downloads", "folder-download"),
        "MUSIC" => ("Music", "folder-music"),
        "PICTURES" => ("Pictures", "folder-pictures"),
        "VIDEOS" => ("Videos", "folder-videos"),
        _ => return None,
    })
}

/// The sidebar in display order. `exists` decides which user directories and the trash
/// are shown.
pub fn build_places(
    home: Option<&Path>,
    user_dirs: &[(String, PathBuf)],
    mounts: &[Mount],
    trash_files: Option<&Path>,
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<Place> {
    let mut out = Vec::new();
    if let Some(h) = home {
        out.push(Place {
            label: "Home".into(),
            path: h.to_path_buf(),
            kind: PlaceKind::Home,
            icon: "user-home",
        });
    }
    for key in [
        "DESKTOP",
        "DOCUMENTS",
        "DOWNLOAD",
        "MUSIC",
        "PICTURES",
        "VIDEOS",
    ] {
        let Some((_, path)) = user_dirs.iter().find(|(k, _)| k == key) else {
            continue;
        };
        let Some((label, icon)) = user_dir_entry(key) else {
            continue;
        };
        if Some(path.as_path()) == home || !exists(path) {
            continue;
        }
        out.push(Place {
            label: label.into(),
            path: path.clone(),
            kind: PlaceKind::UserDir,
            icon,
        });
    }
    if let Some(t) = trash_files
        && exists(t)
    {
        out.push(Place {
            label: "Trash".into(),
            path: t.to_path_buf(),
            kind: PlaceKind::Trash,
            icon: "user-trash",
        });
    }
    out.push(Place {
        label: "File System".into(),
        path: PathBuf::from("/"),
        kind: PlaceKind::Root,
        icon: "drive-harddisk",
    });
    let mut seen = std::collections::HashSet::new();
    for m in mounts.iter().filter(|m| is_volume(m)) {
        if !seen.insert(m.mount_point.clone()) {
            continue;
        }
        let label = m
            .mount_point
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| m.mount_point.display().to_string());
        out.push(Place {
            label,
            path: m.mount_point.clone(),
            kind: PlaceKind::Volume,
            icon: "drive-removable-media",
        });
    }
    out
}

/// Index of the place that best contains `path` (the longest matching prefix), for
/// highlighting; the root place only matches `/` itself so it does not light up everywhere.
pub fn active_place(places: &[Place], path: &Path) -> Option<usize> {
    places
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            if p.kind == PlaceKind::Root {
                path == p.path
            } else {
                path.starts_with(&p.path)
            }
        })
        .max_by_key(|(_, p)| p.path.components().count())
        .map(|(i, _)| i)
}

/// Calls `on_change` with the new `/proc/self/mountinfo` text now and whenever the kernel
/// signals a change (poll for `POLLPRI`). Runs on its own thread; a missing file ends it.
pub fn watch_mounts(on_change: impl Fn(String) + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("files-mounts".into())
        .spawn(move || {
            let Ok(mut file) = std::fs::File::open("/proc/self/mountinfo") else {
                return;
            };
            loop {
                let mut text = String::new();
                if file.seek(SeekFrom::Start(0)).is_err() || file.read_to_string(&mut text).is_err()
                {
                    return;
                }
                on_change(text);
                let mut pfd = libc::pollfd {
                    fd: file.as_raw_fd(),
                    events: libc::POLLPRI,
                    revents: 0,
                };
                // Safety: one valid pollfd for the duration of the call.
                let r = unsafe { libc::poll(&mut pfd, 1, -1) };
                if r < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    return;
                }
                // Several changes arrive together when a disk with partitions appears.
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        });
    if let Err(err) = spawned {
        tracing::warn!("files: cannot start the mount watcher: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_dirs_parse() {
        let text = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Down loads\"\n\
                    XDG_MUSIC_DIR=\"/data/music\"\nXDG_PUBLICSHARE_DIR=\"$HOME\"\nbroken\nXDG_VIDEOS_DIR=relative\n\
                    XDG_DOCUMENTS_DIR=\"$HOME/Doc\\\"s\"\n";
        let d = parse_user_dirs(text, Path::new("/home/u"));
        assert!(d.contains(&("DESKTOP".into(), PathBuf::from("/home/u/Desktop"))));
        assert!(d.contains(&("DOWNLOAD".into(), PathBuf::from("/home/u/Down loads"))));
        assert!(d.contains(&("MUSIC".into(), PathBuf::from("/data/music"))));
        assert!(d.contains(&("PUBLICSHARE".into(), PathBuf::from("/home/u"))));
        assert!(d.contains(&("DOCUMENTS".into(), PathBuf::from("/home/u/Doc\"s"))));
        assert_eq!(
            d.len(),
            5,
            "relative and malformed lines are dropped: {d:?}"
        );
    }

    const MOUNTINFO: &str = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n\
23 22 0:5 / /dev rw,nosuid shared:2 - devtmpfs devtmpfs rw\n\
24 22 0:22 / /proc rw shared:3 - proc proc rw\n\
30 22 259:1 / /boot rw shared:7 - vfat /dev/nvme0n1p1 rw\n\
41 22 8:17 / /run/media/u/My\\040Stick rw shared:9 - exfat /dev/sdb1 rw\n\
42 22 259:3 / /home rw shared:10 - ext4 /dev/nvme0n1p3 rw\n\
43 22 0:40 / /tmp rw shared:11 - tmpfs tmpfs rw\n\
44 22 0:41 / /mnt/nas rw shared:12 - nfs4 srv:/export rw\n\
45 42 0:42 / /home/u/.cache/doc rw - fuse.portal portal rw\n";

    #[test]
    fn mountinfo_parse_and_filter() {
        let ms = parse_mountinfo(MOUNTINFO);
        assert_eq!(ms.len(), 9);
        assert_eq!(unescape_mountinfo("a\\040b\\134c"), b"a b\\c");
        let vols: Vec<_> = ms
            .iter()
            .filter(|m| is_volume(m))
            .map(|m| m.mount_point.display().to_string())
            .collect();
        assert_eq!(vols, ["/run/media/u/My Stick", "/home", "/mnt/nas"]);
        assert!(parse_mountinfo("garbage line").is_empty());
    }

    #[test]
    fn places_in_display_order() {
        let home = Path::new("/home/u");
        let dirs = vec![
            ("DOCUMENTS".to_string(), PathBuf::from("/home/u/Documents")),
            ("DOWNLOAD".to_string(), PathBuf::from("/home/u/Downloads")),
            ("PUBLICSHARE".to_string(), PathBuf::from("/home/u/Public")),
            ("MUSIC".to_string(), PathBuf::from("/home/u")),
        ];
        let exists = |p: &Path| p != Path::new("/home/u/Downloads");
        let places = build_places(
            Some(home),
            &dirs,
            &parse_mountinfo(MOUNTINFO),
            Some(Path::new("/home/u/.local/share/Trash/files")),
            &exists,
        );
        let labels: Vec<_> = places.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Home",
                "Documents",
                "Trash",
                "File System",
                "My Stick",
                "home",
                "nas"
            ]
        );
        assert_eq!(
            build_places(None, &[], &[], None, &exists).len(),
            1,
            "always `/`"
        );
    }

    #[test]
    fn active_place_prefers_the_deepest_and_ignores_root() {
        let p = |label: &str, path: &str, kind| Place {
            label: label.into(),
            path: PathBuf::from(path),
            kind,
            icon: "x",
        };
        let places = vec![
            p("Home", "/home/u", PlaceKind::Home),
            p("Docs", "/home/u/Documents", PlaceKind::UserDir),
            p("File System", "/", PlaceKind::Root),
        ];
        assert_eq!(
            active_place(&places, Path::new("/home/u/Documents/x")),
            Some(1)
        );
        assert_eq!(active_place(&places, Path::new("/home/u/Music")), Some(0));
        assert_eq!(active_place(&places, Path::new("/")), Some(2));
        assert_eq!(active_place(&places, Path::new("/etc")), None);
        assert_eq!(active_place(&places, Path::new("/home/user2")), None);
    }
}
