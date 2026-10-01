//! Session glue: opening files and starting programs, the theme file, icon names.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use aurora_theme::Theme;

use crate::model::{Entry, Kind};

/// Opening a file goes through `xdg-open` (the path is absolute, so never an option).
pub fn open_argv(path: &Path) -> Vec<String> {
    vec!["xdg-open".into(), path.display().to_string()]
}

pub fn terminal_argv(dir: &Path) -> Vec<String> {
    vec![
        "aurora-term".into(),
        "--cwd".into(),
        dir.display().to_string(),
    ]
}

pub fn new_window_argv(dir: &Path) -> Vec<String> {
    vec!["aurora-files".into(), dir.display().to_string()]
}

/// Launches `argv` ourselves, detached in its own session, for when the compositor's IPC is
/// unavailable. The child is reaped on a thread so no zombie is left.
pub fn spawn_direct(argv: &[String]) -> Result<(), String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("nothing to run".into());
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Safety: setsid is async-signal-safe and nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {program}: {e}"))?;
    let _ = std::thread::Builder::new()
        .name("reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(())
}

/// `$XDG_CONFIG_HOME/aurora/theme.toml` (or under `$HOME/.config`).
pub fn theme_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("aurora").join("theme.toml"))
}

/// The theme to start with when the compositor has not sent one (yet).
pub fn load_theme() -> Theme {
    let Some(path) = theme_path() else {
        return Theme::default();
    };
    match Theme::load(&path) {
        Ok((theme, warnings)) => {
            for w in warnings {
                tracing::warn!("files: theme: {w}");
            }
            theme
        }
        Err(err) => {
            tracing::warn!("files: {err}");
            Theme::default()
        }
    }
}

/// The freedesktop icon name for an entry.
pub fn icon_name(e: &Entry) -> &'static str {
    if e.kind == Kind::Broken {
        return "dialog-error";
    }
    if e.is_dir {
        return "folder";
    }
    if e.kind == Kind::Other {
        return "text-x-generic";
    }
    match e.extension().as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "tif" | "tiff" | "avif"
        | "heic" | "ico" => "image-x-generic",
        "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a" | "aac" => "audio-x-generic",
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "m4v" => "video-x-generic",
        "pdf" => "application-pdf",
        "zip" | "tar" | "gz" | "xz" | "bz2" | "zst" | "7z" | "rar" | "tgz" | "deb" | "rpm" => {
            "package-x-generic"
        }
        "doc" | "docx" | "odt" | "rtf" => "x-office-document",
        "xls" | "xlsx" | "ods" | "csv" => "x-office-spreadsheet",
        "ppt" | "pptx" | "odp" => "x-office-presentation",
        "sh" | "bash" | "zsh" | "fish" | "py" | "pl" | "rb" => "text-x-script",
        "rs" | "c" | "h" | "cpp" | "hpp" | "js" | "ts" | "go" | "java" | "html" | "css"
        | "json" | "toml" | "yaml" | "yml" | "xml" => "text-x-generic",
        _ if e.executable() => "application-x-executable",
        _ => "text-x-generic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_shapes() {
        assert_eq!(
            open_argv(Path::new("/a b/c")),
            ["xdg-open".to_string(), "/a b/c".to_string()]
        );
        assert_eq!(terminal_argv(Path::new("/x"))[1], "--cwd");
        assert_eq!(new_window_argv(Path::new("/x"))[0], "aurora-files");
        assert!(spawn_direct(&[]).is_err());
    }

    #[test]
    fn icon_names_follow_kind_and_extension() {
        let f = |n: &str| Entry::new(n, Kind::File);
        assert_eq!(icon_name(&Entry::new("d", Kind::Dir)), "folder");
        assert_eq!(icon_name(&f("Photo.JPG")), "image-x-generic");
        assert_eq!(icon_name(&f("a.pdf")), "application-pdf");
        assert_eq!(icon_name(&f("notes")), "text-x-generic");
        assert_eq!(icon_name(&Entry::new("x", Kind::Broken)), "dialog-error");
        let mut run = f("run");
        run.mode = 0o755;
        assert_eq!(icon_name(&run), "application-x-executable");
        let mut link = Entry::new("l", Kind::Symlink);
        link.is_dir = true;
        assert_eq!(icon_name(&link), "folder");
    }
}
