use std::{ffi::OsString, path::PathBuf};

/// `$AURORA_IPC_SOCK` if set and non-empty, else `$XDG_RUNTIME_DIR/aurora/ipc.sock`.
/// `None` when neither variable is usable.
pub fn socket_path() -> Option<PathBuf> {
    socket_path_from(
        std::env::var_os("AURORA_IPC_SOCK"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
}

/// Pure form of [`socket_path`] for tests and callers that already read the environment.
pub fn socket_path_from(
    ipc_sock: Option<OsString>,
    xdg_runtime: Option<OsString>,
) -> Option<PathBuf> {
    let non_empty = |v: Option<OsString>| v.filter(|v| !v.is_empty());
    if let Some(p) = non_empty(ipc_sock) {
        return Some(PathBuf::from(p));
    }
    non_empty(xdg_runtime).map(|dir| PathBuf::from(dir).join("aurora").join("ipc.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(s.into())
    }

    #[test]
    fn override_wins() {
        assert_eq!(
            socket_path_from(os("/tmp/x.sock"), os("/run/user/1000")),
            Some(PathBuf::from("/tmp/x.sock"))
        );
    }

    #[test]
    fn falls_back_to_runtime_dir() {
        let want = Some(PathBuf::from("/run/user/1000/aurora/ipc.sock"));
        assert_eq!(socket_path_from(None, os("/run/user/1000")), want);
        assert_eq!(socket_path_from(os(""), os("/run/user/1000")), want);
    }

    #[test]
    fn none_without_env() {
        assert_eq!(socket_path_from(None, None), None);
        assert_eq!(socket_path_from(os(""), os("")), None);
    }
}
