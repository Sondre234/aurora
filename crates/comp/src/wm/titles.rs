//! Window title and app id tracking. `WinData` keeps both current (the IPC snapshot reads
//! them), and each change is forwarded to the window's ext-foreign-toplevel-list handle.
use aurora_layout::WinId;
use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::foreign_toplevel_list::ForeignToplevelHandle, xwayland::X11Surface,
};

use super::apply::read_strings;
use crate::Aurora;

/// Text a client controls ends up in logs, IPC frames and protocol events: bound its size.
const MAX_TEXT: usize = 4096;

/// `s` cut to at most [`MAX_TEXT`] bytes on a char boundary.
pub fn bounded(mut s: String) -> String {
    if s.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

impl Aurora {
    /// The xdg toplevel set a new title.
    pub fn xdg_title_changed(&mut self, surface: &WlSurface) {
        if let Some(id) = self.wm.id_of(surface) {
            let (_, title) = read_strings(surface);
            self.set_strings(id, Some(title), None);
        }
    }

    /// The xdg toplevel set a new app id.
    pub fn xdg_app_id_changed(&mut self, surface: &WlSurface) {
        if let Some(id) = self.wm.id_of(surface) {
            let (app_id, _) = read_strings(surface);
            self.set_strings(id, None, Some(app_id));
        }
    }

    /// `_NET_WM_NAME` / `WM_NAME` changed.
    pub fn x11_title_changed(&mut self, x11: &X11Surface) {
        if let Some(id) = self.x11_id(x11) {
            self.set_strings(id, Some(x11.title()), None);
        }
    }

    /// `WM_CLASS` changed; X11 windows report their class as app id.
    pub fn x11_class_changed(&mut self, x11: &X11Surface) {
        if let Some(id) = self.x11_id(x11) {
            self.set_strings(id, None, Some(x11.class()));
        }
    }

    fn set_strings(&mut self, id: WinId, title: Option<String>, app_id: Option<String>) {
        let Some(win) = self.wm.windows.get_mut(&id) else {
            return;
        };
        if let Some(title) = title {
            win.title = bounded(title);
        }
        if let Some(app_id) = app_id {
            win.app_id = bounded(app_id);
        }
        if let Some(handle) = &win.foreign {
            push_strings(handle, &win.title, &win.app_id);
        }
    }

    /// Announces a window that just got its first buffer to ext-foreign-toplevel-list clients.
    pub(super) fn foreign_announce(&mut self, id: WinId) {
        let Some(win) = self.wm.windows.get_mut(&id) else {
            return;
        };
        if win.foreign.is_none() {
            win.foreign = Some(
                self.protocols
                    .foreign_toplevel
                    .new_toplevel::<Aurora>(win.title.clone(), win.app_id.clone()),
            );
        }
    }

    /// The window unmapped: its handle reports `closed` and goes inert.
    pub(super) fn foreign_close(&mut self, id: WinId) {
        if let Some(handle) = self.wm.windows.get_mut(&id).and_then(|w| w.foreign.take()) {
            self.protocols.foreign_toplevel.remove_toplevel(&handle);
        }
    }
}

/// `send_*` only emit for a value that differs from the handle's own, so this is cheap to
/// call for any change.
fn push_strings(handle: &ForeignToplevelHandle, title: &str, app_id: &str) {
    let changed = handle.title() != title || handle.app_id() != app_id;
    handle.send_title(title);
    handle.send_app_id(app_id);
    if changed {
        handle.send_done();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_keeps_short_text() {
        assert_eq!(bounded("hello".into()), "hello");
    }

    #[test]
    fn bounded_cuts_on_a_char_boundary() {
        let s = "é".repeat(MAX_TEXT); // 2 bytes each
        let cut = bounded(s);
        assert!(cut.len() <= MAX_TEXT);
        assert!(cut.chars().all(|c| c == 'é'));
    }
}
