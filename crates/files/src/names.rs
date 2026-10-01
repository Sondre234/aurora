//! File name rules: validation of user-typed names and collision-free name generation.
//! Pure; existence checks are passed in.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

/// Why a typed name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    Empty,
    Reserved,
    HasSlash,
    HasNul,
    TooLong,
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NameError::Empty => "the name is empty",
            NameError::Reserved => "\".\" and \"..\" are not valid names",
            NameError::HasSlash => "a name cannot contain \"/\"",
            NameError::HasNul => "a name cannot contain a NUL byte",
            NameError::TooLong => "the name is too long",
        })
    }
}

/// Checks a single path component typed by the user.
pub fn validate_name(name: &OsStr) -> Result<(), NameError> {
    let b = name.as_bytes();
    if b.is_empty() {
        Err(NameError::Empty)
    } else if b == b"." || b == b".." {
        Err(NameError::Reserved)
    } else if b.contains(&b'/') {
        Err(NameError::HasSlash)
    } else if b.contains(&0) {
        Err(NameError::HasNul)
    } else if b.len() > 255 {
        Err(NameError::TooLong)
    } else {
        Ok(())
    }
}

/// Splits `archive.tar.gz` into (`archive.tar`, `.gz`); a leading dot is not an extension
/// (`.bashrc` has none) and neither is a trailing one.
pub fn split_ext(name: &[u8]) -> (&[u8], &[u8]) {
    match name.iter().rposition(|&b| b == b'.') {
        Some(i) if i > 0 && i + 1 < name.len() => name.split_at(i),
        _ => (name, &[]),
    }
}

fn join(stem: &[u8], mid: &str, ext: &[u8]) -> OsString {
    let mut v = stem.to_vec();
    v.extend_from_slice(mid.as_bytes());
    v.extend_from_slice(ext);
    OsString::from_vec(v)
}

/// `name`, `stem.2.ext`, `stem.3.ext`, ... (the trash directory's scheme). `n <= 1` is the
/// name itself.
pub fn numbered_name(name: &OsStr, n: u32) -> OsString {
    if n <= 1 {
        return name.to_os_string();
    }
    let (stem, ext) = split_ext(name.as_bytes());
    join(stem, &format!(".{n}"), ext)
}

/// `name`, `stem (copy).ext`, `stem (copy 2).ext`, ... (the "keep both" scheme). `n == 0`
/// is the name itself.
pub fn copy_name(name: &OsStr, n: u32) -> OsString {
    if n == 0 {
        return name.to_os_string();
    }
    let (stem, ext) = split_ext(name.as_bytes());
    let mid = if n == 1 {
        " (copy)".to_string()
    } else {
        format!(" (copy {n})")
    };
    join(stem, &mid, ext)
}

/// First free name by `scheme` (called with 0, 1, 2, ...) according to `taken`.
pub fn first_free(scheme: impl Fn(u32) -> OsString, taken: impl Fn(&OsStr) -> bool) -> OsString {
    (0..)
        .map(&scheme)
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| scheme(0))
}

/// Free "keep both" name for `name`.
pub fn unique_copy_name(name: &OsStr, taken: impl Fn(&OsStr) -> bool) -> OsString {
    first_free(|n| copy_name(name, n), taken)
}

/// Free trash name for `name`.
pub fn unique_trash_name(name: &OsStr, taken: impl Fn(&OsStr) -> bool) -> OsString {
    first_free(|n| numbered_name(name, n.max(1)), taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn validation() {
        assert_eq!(validate_name(&o("a b")), Ok(()));
        assert_eq!(validate_name(&o("")), Err(NameError::Empty));
        assert_eq!(validate_name(&o(".")), Err(NameError::Reserved));
        assert_eq!(validate_name(&o("..")), Err(NameError::Reserved));
        assert_eq!(validate_name(&o("a/b")), Err(NameError::HasSlash));
        assert_eq!(validate_name(&o("a\0b")), Err(NameError::HasNul));
        assert_eq!(validate_name(&o(&"x".repeat(256))), Err(NameError::TooLong));
        assert_eq!(validate_name(&o(&"x".repeat(255))), Ok(()));
        assert_eq!(validate_name(&o(".hidden")), Ok(()));
    }

    #[test]
    fn extension_split() {
        assert_eq!(split_ext(b"a.txt"), (&b"a"[..], &b".txt"[..]));
        assert_eq!(split_ext(b"a.tar.gz"), (&b"a.tar"[..], &b".gz"[..]));
        assert_eq!(split_ext(b".bashrc"), (&b".bashrc"[..], &b""[..]));
        assert_eq!(split_ext(b"end."), (&b"end."[..], &b""[..]));
        assert_eq!(split_ext(b"plain"), (&b"plain"[..], &b""[..]));
    }

    #[test]
    fn numbering_schemes() {
        assert_eq!(numbered_name(&o("a.txt"), 1), o("a.txt"));
        assert_eq!(numbered_name(&o("a.txt"), 2), o("a.2.txt"));
        assert_eq!(numbered_name(&o("README"), 3), o("README.3"));
        assert_eq!(copy_name(&o("a.txt"), 0), o("a.txt"));
        assert_eq!(copy_name(&o("a.txt"), 1), o("a (copy).txt"));
        assert_eq!(copy_name(&o("a.txt"), 2), o("a (copy 2).txt"));
        assert_eq!(copy_name(&o(".rc"), 1), o(".rc (copy)"));
    }

    #[test]
    fn first_free_skips_taken_names() {
        let taken = |n: &OsStr| matches!(n.to_str(), Some("a.txt" | "a (copy).txt"));
        assert_eq!(unique_copy_name(&o("a.txt"), taken), o("a (copy 2).txt"));
        assert_eq!(unique_copy_name(&o("b.txt"), taken), o("b.txt"));
        let t = |n: &OsStr| matches!(n.to_str(), Some("x" | "x.2"));
        assert_eq!(unique_trash_name(&o("x"), t), o("x.3"));
    }
}
