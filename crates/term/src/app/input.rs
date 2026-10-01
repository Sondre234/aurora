//! Keyboard, pointer, selection and clipboard handling of [`TermApp`].

use std::os::unix::process::CommandExt;

use aurora_ui::runtime::{Runtime, Selection, pick_text_mime};
use aurora_ui::{Button, Input, Key, KeyEvent, Point, Rect};

use super::TermApp;
use crate::backend::{Backend, Pos};
use crate::keys::{self, paste_bytes};
use crate::mouse::{self, Reporting};
use crate::select::{ClickTracker, link_openable};

/// Insert key (keysym), for Shift+Insert.
const INSERT: u32 = 0xff63;

fn mouse_button(b: Button) -> Option<mouse::Button> {
    match b {
        Button::Left => Some(mouse::Button::Left),
        Button::Middle => Some(mouse::Button::Middle),
        Button::Right => Some(mouse::Button::Right),
        Button::Other(_) => None,
    }
}

impl TermApp {
    pub(super) fn handle_input(&mut self, rt: &mut Runtime<Self>, input: Input) {
        match input {
            Input::Key(k) => self.key(rt, k),
            Input::PointerMove(p) => self.pointer_move(rt, p),
            Input::PointerLeave => self.last_motion_cell = None,
            Input::PointerDown(p, b) => self.pointer_down(rt, p, b),
            Input::PointerUp(p, b) => self.pointer_up(rt, p, b),
            Input::Scroll { pos, dy, .. } => self.wheel_scroll(rt, pos, dy),
        }
    }

    // ---- keyboard ----

    fn key(&mut self, rt: &mut Runtime<Self>, ev: KeyEvent) {
        let keysym = match ev.key {
            Key::Other(k) => Some(k),
            _ => None,
        };
        self.mods.key(
            self.now_ms(),
            keysym,
            ev.mods.ctrl,
            ev.mods.alt,
            ev.mods.shift,
        );
        let m = ev.mods;
        if m.ctrl
            && m.shift
            && !m.alt
            && let Key::Char(c) = ev.key
        {
            match c.to_ascii_lowercase() {
                'c' => return self.copy_selection(rt),
                'v' => return self.paste_clipboard(rt),
                _ => {}
            }
        }
        let alt_screen = self.view.borrow().backend.modes().alt_screen;
        if m.shift && !m.ctrl && !m.alt && !alt_screen {
            match ev.key {
                Key::PageUp | Key::PageDown => {
                    let up = ev.key == Key::PageUp;
                    self.view.borrow_mut().backend.scroll_page(up);
                    self.flush_damage(rt);
                    return;
                }
                Key::Other(INSERT) => return self.paste_primary(rt),
                _ => {}
            }
        }
        if self.ended {
            return;
        }
        let Some(press) = keys::press_from_ui(&ev) else {
            return;
        };
        let modes = self.view.borrow().backend.modes().keys;
        let bytes = keys::encode_key(&press, modes);
        if bytes.is_empty() {
            return;
        }
        // Typing returns to the live screen and drops the selection.
        let scrolled = self.view.borrow().backend.display_offset() > 0;
        if scrolled {
            self.view.borrow_mut().backend.scroll_to_bottom();
            self.flush_damage(rt);
        }
        self.mutate_selection(rt, |b| {
            b.select_clear();
        });
        self.selecting = false;
        self.cursor_solid(rt);
        self.send(rt, &bytes);
    }

    // ---- pointer ----

    fn mouse_mods(&self) -> mouse::Mods {
        let (ctrl, alt, shift) = self.mods.at(self.now_ms());
        mouse::Mods { shift, alt, ctrl }
    }

    fn report(&mut self, rt: &mut Runtime<Self>, kind: mouse::Kind, col: usize, row: usize) {
        let modes = self.view.borrow().backend.modes();
        let mods = self.mouse_mods();
        if let Some(bytes) = mouse::encode(modes.reporting, modes.sgr_mouse, kind, col, row, mods) {
            self.send(rt, &bytes);
        }
    }

    fn pointer_down(&mut self, rt: &mut Runtime<Self>, p: Point, button: Button) {
        let (col, row, right) = self.view.borrow().geom.hit(p);
        let modes = self.view.borrow().backend.modes();
        let (ctrl, _, shift) = self.mods.at(self.now_ms());
        // Shift bypasses a program's mouse mode so text can still be selected.
        if modes.reporting != Reporting::Off && !shift {
            if let Some(b) = mouse_button(button) {
                self.held = Some(b);
                self.last_motion_cell = Some((col, row));
                self.report(rt, mouse::Kind::Press(b), col, row);
            }
            return;
        }
        let pos = Pos { row, col };
        match button {
            Button::Left => {
                if ctrl && let Some(uri) = self.view.borrow().backend.link_at(pos) {
                    return open_link(&uri);
                }
                let count = self.clicks.press(self.now_ms(), col, row);
                let extend = shift && self.view.borrow().backend.has_selection();
                self.mutate_selection(rt, |b| {
                    if extend {
                        b.select_update(pos, right);
                    } else {
                        b.select_begin(ClickTracker::kind(count), pos, right);
                    }
                });
                self.selecting = true;
            }
            // xterm convention: the right button extends the selection.
            Button::Right if self.view.borrow().backend.has_selection() => {
                self.mutate_selection(rt, |b| {
                    b.select_update(pos, right);
                });
                self.selecting = true;
            }
            Button::Middle => self.paste_primary(rt),
            _ => {}
        }
    }

    fn pointer_up(&mut self, rt: &mut Runtime<Self>, p: Point, button: Button) {
        if let Some(held) = self.held
            && mouse_button(button) == Some(held)
        {
            self.held = None;
            let (col, row, _) = self.view.borrow().geom.hit(p);
            self.report(rt, mouse::Kind::Release(held), col, row);
            return;
        }
        if self.selecting && matches!(button, Button::Left | Button::Right) {
            self.selecting = false;
            let text = self.view.borrow().backend.selection_text();
            match text {
                Some(t) => self.set_selection_text(rt, true, t),
                // A click without a drag selected nothing.
                None => self.mutate_selection(rt, |b| {
                    b.select_clear();
                }),
            }
        }
    }

    fn pointer_move(&mut self, rt: &mut Runtime<Self>, p: Point) {
        let (col, row, right) = self.view.borrow().geom.hit(p);
        if self.selecting {
            self.mutate_selection(rt, |b| {
                b.select_update(Pos { row, col }, right);
            });
            return;
        }
        if self.view.borrow().backend.modes().reporting != Reporting::Off
            && self.last_motion_cell != Some((col, row))
        {
            self.last_motion_cell = Some((col, row));
            let kind = mouse::Kind::Motion(self.held);
            self.report(rt, kind, col, row);
        }
    }

    fn wheel_scroll(&mut self, rt: &mut Runtime<Self>, p: Point, dy: f32) {
        let (col, row, _) = self.view.borrow().geom.hit(p);
        let modes = self.view.borrow().backend.modes();
        let line_px = self.view.borrow().metrics.logical_height();
        let (_, _, shift) = self.mods.at(self.now_ms());
        if modes.reporting != Reporting::Off && !shift {
            let n = self.wheel.add(dy, line_px);
            let kind = if n > 0 {
                mouse::Kind::WheelDown
            } else {
                mouse::Kind::WheelUp
            };
            for _ in 0..n.unsigned_abs().min(20) {
                self.report(rt, kind, col, row);
            }
        } else if modes.alt_screen && modes.alternate_scroll && !shift {
            // Full-screen programs without mouse mode get the wheel as arrow keys.
            let n = self.wheel.add(dy, line_px);
            let fin = if n > 0 { b'B' } else { b'A' };
            let lead: &[u8] = if modes.keys.app_cursor {
                b"\x1bO"
            } else {
                b"\x1b["
            };
            for _ in 0..n.unsigned_abs().min(20) {
                let mut seq = lead.to_vec();
                seq.push(fin);
                self.send(rt, &seq);
            }
        } else {
            // About three lines per wheel notch.
            let n = self.wheel.add(dy * 2.0, line_px);
            if n != 0 {
                self.view.borrow_mut().backend.scroll(-n);
                self.clicks.reset();
                self.flush_damage(rt);
            }
        }
    }

    // ---- selection and clipboard ----

    /// Change the selection and damage every row it touched before or after.
    pub(super) fn mutate_selection(
        &mut self,
        rt: &mut Runtime<Self>,
        f: impl FnOnce(&mut Backend),
    ) {
        let rect = {
            let mut v = self.view.borrow_mut();
            let before = v.backend.selection_rows();
            f(&mut v.backend);
            let after = v.backend.selection_rows();
            let rows = match (before, after) {
                (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
                (a, b) => a.or(b),
            };
            rows.map(|(r0, r1)| {
                let last = v.geom.cols - 1;
                let top: Rect = v.geom.damage(r0, 0, last);
                top.union(&v.geom.damage(r1, 0, last))
            })
        };
        if let Some(r) = rect {
            self.damage_rect(rt, r);
        }
    }

    fn copy_selection(&mut self, rt: &mut Runtime<Self>) {
        let text = self.view.borrow().backend.selection_text();
        if let Some(t) = text {
            self.set_selection_text(rt, false, t);
        }
    }

    /// Offer `text` on the clipboard or the primary selection.
    pub(super) fn set_selection_text(
        &mut self,
        rt: &mut Runtime<Self>,
        primary: bool,
        text: String,
    ) {
        let sel = if primary {
            Selection::Primary
        } else {
            Selection::Clipboard
        };
        match rt.set_text(sel, &text) {
            Ok(()) => {
                let name = if primary { "primary" } else { "clipboard" };
                tracing::info!("term: {name} set bytes={}", text.len());
                if primary {
                    self.own_primary = Some(text);
                } else {
                    self.own_clipboard = Some(text);
                }
            }
            Err(e) => tracing::debug!("term: cannot set the selection: {e}"),
        }
    }

    fn paste_clipboard(&mut self, rt: &mut Runtime<Self>) {
        self.paste_from(rt, Selection::Clipboard);
    }

    fn paste_primary(&mut self, rt: &mut Runtime<Self>) {
        self.paste_from(rt, Selection::Primary);
    }

    /// Paste a selection: our own text directly (a client is never offered its own
    /// selection), anyone else's through a nonblocking read that ends in
    /// [`Event::SelectionData`](aurora_ui::runtime::Event::SelectionData).
    fn paste_from(&mut self, rt: &mut Runtime<Self>, sel: Selection) {
        let own = match sel {
            Selection::Clipboard => self.own_clipboard.clone(),
            Selection::Primary => self.own_primary.clone(),
        };
        if let Some(text) = own {
            return self.paste_text(rt, &text);
        }
        let mimes = rt.selection_mimes(sel);
        let Some(mime) = pick_text_mime(&mimes).map(str::to_string) else {
            return;
        };
        let tag = self.next_tag;
        self.next_tag += 1;
        if let Err(e) = rt.read_selection(sel, &mime, tag) {
            tracing::debug!("term: cannot read the selection: {e}");
        }
    }

    /// Write pasted text to the child, bracketed when the program asked for that.
    pub(super) fn paste_text(&mut self, rt: &mut Runtime<Self>, text: &str) {
        if self.ended || text.is_empty() {
            return;
        }
        let bracketed = self.view.borrow().backend.modes().bracketed_paste;
        tracing::info!("term: paste bytes={}", text.len());
        if self.view.borrow().backend.display_offset() > 0 {
            self.view.borrow_mut().backend.scroll_to_bottom();
            self.flush_damage(rt);
        }
        self.send(rt, &paste_bytes(text, bracketed));
    }
}

/// Open a hyperlink with `xdg-open`, detached, for schemes that are safe to hand over.
fn open_link(uri: &str) {
    if !link_openable(uri) {
        tracing::info!("term: refusing to open {uri:?}");
        return;
    }
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(uri)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Safety: setsid is async-signal-safe and nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(mut child) => {
            tracing::info!("term: open {uri}");
            let _ = std::thread::Builder::new()
                .name("reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
        Err(e) => tracing::warn!("term: cannot run xdg-open: {e}"),
    }
}
