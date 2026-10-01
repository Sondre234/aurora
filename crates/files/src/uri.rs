//! Clipboard codecs: `text/uri-list` and `x-special/gnome-copied-files`, plus the percent
//! encoding they share with the trash spec's `.trashinfo` files. All pure.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

pub const MIME_URI_LIST: &str = "text/uri-list";
pub const MIME_COPIED_FILES: &str = "x-special/gnome-copied-files";

/// Whether a paste copies or moves the files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipOp {
    Copy,
    Cut,
}

/// Percent-encodes bytes, leaving RFC 3986 unreserved characters and `/` as they are.
pub fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` escapes; a malformed escape is kept literally.
pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// `file:///percent/encoded/path`.
pub fn path_to_uri(p: &Path) -> String {
    format!("file://{}", percent_encode(p.as_os_str().as_bytes()))
}

/// The absolute local path of a `file:` URI (host empty or `localhost`), else `None`.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.trim().strip_prefix("file://")?;
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(i) if rest[..i].eq_ignore_ascii_case("localhost") => &rest[i..],
        _ => return None,
    };
    let bytes = percent_decode(path);
    if bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

/// `text/uri-list`: one URI per line, CRLF terminated.
pub fn encode_uri_list(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| path_to_uri(p) + "\r\n").collect()
}

/// Local paths of a `text/uri-list`; comments, blank lines and non-file URIs are dropped.
pub fn decode_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(uri_to_path)
        .collect()
}

/// `x-special/gnome-copied-files`: `copy` or `cut`, then one URI per line.
pub fn encode_copied_files(op: ClipOp, paths: &[PathBuf]) -> String {
    let mut s = String::from(match op {
        ClipOp::Copy => "copy",
        ClipOp::Cut => "cut",
    });
    for p in paths {
        s.push('\n');
        s.push_str(&path_to_uri(p));
    }
    s
}

/// Parses the gnome format; `None` when the first line is not `copy`/`cut`.
pub fn decode_copied_files(text: &str) -> Option<(ClipOp, Vec<PathBuf>)> {
    let mut lines = text.lines();
    let op = match lines.next()?.trim() {
        "copy" => ClipOp::Copy,
        "cut" => ClipOp::Cut,
        _ => return None,
    };
    let paths = lines
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(uri_to_path)
        .collect();
    Some((op, paths))
}

/// Picks the best of the offered mime types for pasting files.
pub fn pick_file_mime(offered: &[String]) -> Option<&'static str> {
    [MIME_COPIED_FILES, MIME_URI_LIST]
        .into_iter()
        .find(|m| offered.iter().any(|o| o == m))
}

/// Interprets clipboard data of `mime` as files to paste. A bare uri-list means copy.
pub fn decode_paste(mime: &str, data: &[u8]) -> Option<(ClipOp, Vec<PathBuf>)> {
    let text = std::str::from_utf8(data).ok()?;
    let parsed = match mime {
        MIME_COPIED_FILES => decode_copied_files(text),
        MIME_URI_LIST => Some((ClipOp::Copy, decode_uri_list(text))),
        _ => None,
    }?;
    (!parsed.1.is_empty()).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn percent_round_trip() {
        let raw = "/home/u/a b#c%d é.txt";
        let enc = percent_encode(raw.as_bytes());
        assert_eq!(enc, "/home/u/a%20b%23c%25d%20%C3%A9.txt");
        assert_eq!(percent_decode(&enc), raw.as_bytes());
        assert_eq!(percent_decode("100%"), b"100%");
        assert_eq!(percent_decode("%zz%4"), b"%zz%4");
    }

    #[test]
    fn non_utf8_names_survive() {
        let path = PathBuf::from(OsString::from_vec(b"/tmp/a\xff.bin".to_vec()));
        let uri = path_to_uri(&path);
        assert_eq!(uri, "file:///tmp/a%FF.bin");
        assert_eq!(uri_to_path(&uri), Some(path));
    }

    #[test]
    fn uri_parsing() {
        assert_eq!(uri_to_path("file:///a/b"), Some(p("/a/b")));
        assert_eq!(uri_to_path("file://localhost/a%20b"), Some(p("/a b")));
        assert_eq!(uri_to_path("file://other/a"), None);
        assert_eq!(uri_to_path("http://x/a"), None);
        assert_eq!(uri_to_path("file://relative"), None);
        assert_eq!(uri_to_path("file:///a%00b"), None);
    }

    #[test]
    fn uri_list_codec() {
        let paths = vec![p("/a/x y"), p("/b")];
        let text = encode_uri_list(&paths);
        assert_eq!(text, "file:///a/x%20y\r\nfile:///b\r\n");
        assert_eq!(decode_uri_list(&text), paths);
        let foreign = "# comment\r\n\r\nhttp://x/y\r\nfile:///ok\r\n";
        assert_eq!(decode_uri_list(foreign), vec![p("/ok")]);
    }

    #[test]
    fn copied_files_codec() {
        let paths = vec![p("/a"), p("/b c")];
        let text = encode_copied_files(ClipOp::Cut, &paths);
        assert_eq!(text, "cut\nfile:///a\nfile:///b%20c");
        assert_eq!(
            decode_copied_files(&text),
            Some((ClipOp::Cut, paths.clone()))
        );
        assert_eq!(
            decode_copied_files("copy\nfile:///a\n"),
            Some((ClipOp::Copy, vec![p("/a")]))
        );
        assert_eq!(decode_copied_files("move\nfile:///a"), None);
        assert_eq!(decode_copied_files(""), None);
    }

    #[test]
    fn paste_picks_and_decodes() {
        let offered = vec!["text/plain".to_string(), MIME_URI_LIST.to_string()];
        assert_eq!(pick_file_mime(&offered), Some(MIME_URI_LIST));
        let both = vec![MIME_URI_LIST.to_string(), MIME_COPIED_FILES.to_string()];
        assert_eq!(pick_file_mime(&both), Some(MIME_COPIED_FILES));
        assert_eq!(pick_file_mime(&["text/plain".to_string()]), None);
        assert_eq!(
            decode_paste(MIME_URI_LIST, b"file:///a\r\n"),
            Some((ClipOp::Copy, vec![p("/a")]))
        );
        assert_eq!(decode_paste(MIME_URI_LIST, b"# nothing\r\n"), None);
        assert_eq!(decode_paste("text/plain", b"x"), None);
    }
}
