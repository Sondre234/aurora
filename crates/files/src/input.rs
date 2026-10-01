//! Pointer and keyboard handling. Keys are mapped to [`Command`]s by a pure function, so
//! the keymap is tested without a window; applying a command lives on [`Files`].

use std::time::{Duration, Instant};

use aurora_ui::runtime::{CursorIcon, Runtime};
use aurora_ui::{Button, Input, Key, KeyEvent, Mods, Point};

use crate::app::{Files, ModalKind};
use crate::edit::EditOutcome;
use crate::model::{SortKey, typeahead};
use crate::scene::Editing;
use crate::view::Hit;

const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Evdev codes of the side mouse buttons.
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

// X11 keysyms of the function keys the toolkit passes through as `Key::Other`.
const KEY_F2: u32 = 0xFFBF;
const KEY_F4: u32 = 0xFFC1;
const KEY_F5: u32 = 0xFFC2;

/// What a key does in the file list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Move the cursor by this many rows; `extend` selects the range.
    Move {
        delta: i64,
        extend: bool,
    },
    /// Move by a page (+1 / -1).
    Page {
        dir: i64,
        extend: bool,
    },
    First {
        extend: bool,
    },
    Last {
        extend: bool,
    },
    Open,
    OpenNewWindow,
    Up,
    Back,
    Forward,
    /// Escape: cancel the operation, clear the filter, then the selection.
    Dismiss,
    Trash,
    DeletePermanently,
    SelectAll,
    InvertSelection,
    FocusPath,
    ToggleHidden,
    Copy,
    Cut,
    Paste,
    Undo,
    Refresh,
    Rename,
    NewFolder,
    NewFile,
    Terminal,
    Filter,
    Sort(SortKey),
    /// A printable character for jump-to-name.
    Typeahead(char),
}

/// The keymap of the file list (no editor or modal open).
pub fn command_for(k: &KeyEvent) -> Option<Command> {
    let m = k.mods;
    let ctrl = m.ctrl && !m.alt && !m.logo;
    let alt = m.alt && !m.ctrl && !m.logo;
    let both = m.ctrl && m.alt && !m.logo;
    if m.logo {
        return None;
    }
    Some(match k.key {
        Key::Up if alt => Command::Up,
        Key::Left if alt => Command::Back,
        Key::Right if alt => Command::Forward,
        Key::Up => Command::Move {
            delta: -1,
            extend: m.shift,
        },
        Key::Down => Command::Move {
            delta: 1,
            extend: m.shift,
        },
        Key::PageUp => Command::Page {
            dir: -1,
            extend: m.shift,
        },
        Key::PageDown => Command::Page {
            dir: 1,
            extend: m.shift,
        },
        Key::Home => Command::First { extend: m.shift },
        Key::End => Command::Last { extend: m.shift },
        Key::Enter if ctrl => Command::OpenNewWindow,
        Key::Enter => Command::Open,
        Key::Backspace => Command::Up,
        Key::Escape => Command::Dismiss,
        Key::Delete if m.shift => Command::DeletePermanently,
        Key::Delete => Command::Trash,
        Key::Other(KEY_F2) => Command::Rename,
        Key::Other(KEY_F4) => Command::Terminal,
        Key::Other(KEY_F5) => Command::Refresh,
        Key::Char(c) if both => match c.to_ascii_lowercase() {
            't' => Command::Terminal,
            'n' => Command::NewFile,
            _ => return None,
        },
        Key::Char(c) if ctrl => match c.to_ascii_lowercase() {
            'a' => Command::SelectAll,
            'i' => Command::InvertSelection,
            'l' => Command::FocusPath,
            'h' => Command::ToggleHidden,
            'c' => Command::Copy,
            'x' => Command::Cut,
            'v' => Command::Paste,
            'z' => Command::Undo,
            'r' => Command::Refresh,
            'n' if m.shift => Command::NewFolder,
            '1' => Command::Sort(SortKey::Name),
            '2' => Command::Sort(SortKey::Size),
            '3' => Command::Sort(SortKey::Modified),
            '4' => Command::Sort(SortKey::Kind),
            _ => return None,
        },
        Key::Char('/') if !m.ctrl && !m.alt => Command::Filter,
        Key::Char(_) if !m.ctrl && !m.alt => {
            let c = k.text.as_deref()?.chars().next()?;
            if c.is_control() || c.is_whitespace() && c != ' ' {
                return None;
            }
            Command::Typeahead(c)
        }
        _ => return None,
    })
}

/// Keys of a blocking question: `Some(button index)` when the key answers it, `Err(())`
/// style handled by the caller for the apply-all toggle.
pub fn modal_key(kind: &ModalKind, k: &KeyEvent) -> ModalKey {
    let c = match k.key {
        Key::Char(c) if !k.mods.ctrl && !k.mods.alt => Some(c.to_ascii_lowercase()),
        _ => None,
    };
    match kind {
        ModalKind::Conflict => match (k.key, c) {
            (Key::Escape, _) => ModalKey::Button(3),
            (_, Some('s')) => ModalKey::Button(0),
            (_, Some('r')) => ModalKey::Button(1),
            (_, Some('k')) => ModalKey::Button(2),
            (_, Some('a')) => ModalKey::ToggleAll,
            _ => ModalKey::None,
        },
        ModalKind::Confirm(_) => match (k.key, c) {
            (Key::Enter, _) | (_, Some('y')) => ModalKey::Button(0),
            (Key::Escape, _) | (_, Some('n')) => ModalKey::Button(1),
            _ => ModalKey::None,
        },
        ModalKind::None => ModalKey::None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalKey {
    Button(usize),
    ToggleAll,
    None,
}

impl Files {
    pub fn on_input(&mut self, rt: &mut Runtime<Self>, input: Input) {
        self.last_activity = Instant::now();
        match input {
            Input::Key(k) => self.on_key(rt, &k),
            Input::PointerMove(p) => self.pointer_move(rt, p),
            Input::PointerLeave => self.set_hover(rt, Hit::None),
            Input::PointerDown(p, Button::Left) => self.pointer_down(rt, p),
            Input::PointerDown(_, Button::Other(BTN_SIDE)) => self.go_back(rt),
            Input::PointerDown(_, Button::Other(BTN_EXTRA)) => self.go_forward(rt),
            Input::Scroll { dy, .. } => self.scroll_by(rt, dy),
            _ => {}
        }
    }

    // ---- pointer ----

    fn hit_at(&self, p: Point) -> Hit {
        let s = self.scene.borrow();
        if let Some((_, buttons)) = s.modal_layout() {
            return buttons
                .iter()
                .position(|r| r.contains(p))
                .map_or(Hit::None, Hit::ModalButton);
        }
        s.metrics()
            .hit(p, s.scroll, s.browser.len(), s.places.len())
    }

    fn set_hover(&mut self, rt: &mut Runtime<Self>, hit: Hit) {
        let old = {
            let mut s = self.scene.borrow_mut();
            if s.hover == hit {
                return;
            }
            std::mem::replace(&mut s.hover, hit)
        };
        for h in [old, hit] {
            let r = {
                let s = self.scene.borrow();
                let m = s.metrics();
                match h {
                    Hit::Row(i) => Some(m.row_rect(i, s.scroll)),
                    Hit::Place(i) => Some(m.place_rect(i)),
                    Hit::Back => Some(m.back_button()),
                    Hit::Forward => Some(m.forward_button()),
                    Hit::Up => Some(m.up_button()),
                    Hit::ModalButton(_) => None,
                    _ => None,
                }
            };
            match r {
                Some(r) => self.damage(rt, r.outset(2.0)),
                None if matches!(h, Hit::ModalButton(_)) => self.damage_all(rt),
                None => {}
            }
        }
    }

    fn pointer_move(&mut self, rt: &mut Runtime<Self>, p: Point) {
        let hit = self.hit_at(p);
        self.set_hover(rt, hit);
        if let Some(sid) = self.surface {
            rt.set_cursor(
                sid,
                if hit == Hit::PathBar {
                    CursorIcon::Text
                } else {
                    CursorIcon::Default
                },
            );
        }
    }

    fn scroll_by(&mut self, rt: &mut Runtime<Self>, dy: f32) {
        {
            let mut s = self.scene.borrow_mut();
            if s.modal.is_some() {
                return;
            }
            let m = s.metrics();
            let rows = s.browser.len();
            let next = m.clamp_scroll(s.scroll + dy, rows);
            if next == s.scroll {
                return;
            }
            s.scroll = next;
        }
        self.damage_list(rt);
    }

    fn pointer_down(&mut self, rt: &mut Runtime<Self>, p: Point) {
        let hit = self.hit_at(p);
        if let Hit::ModalButton(i) = hit {
            self.modal_button(rt, i);
            return;
        }
        if self.scene.borrow().modal.is_some() {
            return;
        }
        // A click anywhere but into the open editor ends it.
        let editing = !self.scene.borrow().editing.is_none();
        if editing && hit != Hit::PathBar {
            let keep_filter = matches!(self.scene.borrow().editing, Editing::Filter(_));
            if keep_filter {
                self.scene.borrow_mut().editing = Editing::None;
                self.damage_all(rt);
            } else {
                self.cancel_edit(rt);
            }
        }
        let mods = rt.modifiers();
        match hit {
            Hit::Back => self.go_back(rt),
            Hit::Forward => self.go_forward(rt),
            Hit::Up => self.go_up(rt),
            Hit::PathBar => self.focus_path(rt),
            Hit::Place(i) => {
                let path = self.scene.borrow().places.get(i).map(|pl| pl.path.clone());
                if let Some(path) = path {
                    self.navigate(rt, path);
                }
            }
            Hit::Header(key) => self.set_sort(rt, key),
            Hit::Row(row) => self.click_row(rt, row, mods),
            Hit::EmptyList if !mods.ctrl && !mods.shift => {
                self.scene.borrow_mut().browser.selection.clear();
                self.log_selection();
                self.damage_list(rt);
                self.damage_status(rt);
            }
            _ => {}
        }
    }

    fn click_row(&mut self, rt: &mut Runtime<Self>, row: usize, mods: Mods) {
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(t, r)| r == row && now.duration_since(t) < DOUBLE_CLICK)
            && !mods.ctrl
            && !mods.shift;
        if double {
            self.last_click = None;
            self.open_row(rt, row);
            return;
        }
        self.last_click = Some((now, row));
        self.scene
            .borrow_mut()
            .browser
            .click(row, mods.ctrl, mods.shift);
        self.ensure_cursor_visible();
        self.log_selection();
        self.damage_list(rt);
        self.damage_status(rt);
    }

    pub fn focus_path(&mut self, rt: &mut Runtime<Self>) {
        let mut edit = crate::edit::LineEdit::new(&self.cwd().display().to_string());
        edit.select_all();
        self.scene.borrow_mut().editing = Editing::Path(edit);
        self.damage_toolbar(rt);
    }

    // ---- keyboard ----

    pub fn on_key(&mut self, rt: &mut Runtime<Self>, k: &KeyEvent) {
        self.last_activity = Instant::now();
        if self.scene.borrow().modal.is_some() {
            match modal_key(&self.modal_kind, k) {
                crate::input::ModalKey::Button(i) => self.modal_button(rt, i),
                crate::input::ModalKey::ToggleAll => self.toggle_apply_all(rt),
                crate::input::ModalKey::None => {}
            }
            return;
        }
        if !self.scene.borrow().editing.is_none() {
            self.editor_key(rt, k);
            return;
        }
        let Some(cmd) = command_for(k) else { return };
        self.apply(rt, cmd);
    }

    fn editor_key(&mut self, rt: &mut Runtime<Self>, k: &KeyEvent) {
        // Ctrl+V pastes text into the editor.
        if k.mods.ctrl && matches!(k.key, Key::Char(c) if c.eq_ignore_ascii_case(&'v')) {
            self.paste_text_into_editor(rt);
            return;
        }
        let outcome = {
            let mut s = self.scene.borrow_mut();
            match s.editing.edit_mut() {
                Some(e) => e.key(k),
                None => EditOutcome::Ignored,
            }
        };
        match outcome {
            EditOutcome::Submit => self.commit_edit(rt),
            EditOutcome::Cancel => self.cancel_edit(rt),
            EditOutcome::Changed => self.on_editing_changed(rt),
            EditOutcome::Moved => self.damage_all(rt),
            EditOutcome::Ignored => {}
        }
    }

    fn apply(&mut self, rt: &mut Runtime<Self>, cmd: Command) {
        let page = {
            let s = self.scene.borrow();
            let m = s.metrics();
            ((m.list().h / m.row_h).floor() as i64 - 1).max(1)
        };
        let mut selection_changed = false;
        match cmd {
            Command::Move { delta, extend } => {
                self.scene.borrow_mut().browser.move_cursor(delta, extend);
                selection_changed = true;
            }
            Command::Page { dir, extend } => {
                self.scene
                    .borrow_mut()
                    .browser
                    .move_cursor(dir * page, extend);
                selection_changed = true;
            }
            Command::First { extend } => {
                self.scene.borrow_mut().browser.go_to_row(0, extend);
                selection_changed = true;
            }
            Command::Last { extend } => {
                let last = self.scene.borrow().browser.len().saturating_sub(1);
                self.scene.borrow_mut().browser.go_to_row(last, extend);
                selection_changed = true;
            }
            Command::SelectAll => {
                self.scene.borrow_mut().browser.select_all();
                selection_changed = true;
            }
            Command::InvertSelection => {
                self.scene.borrow_mut().browser.invert_selection();
                selection_changed = true;
            }
            Command::Open => self.open_cursor(rt),
            Command::OpenNewWindow => self.open_in_new_window(rt),
            Command::Up => self.go_up(rt),
            Command::Back => self.go_back(rt),
            Command::Forward => self.go_forward(rt),
            Command::Dismiss => selection_changed = self.dismiss(),
            Command::Trash => self.trash_selection(rt),
            Command::DeletePermanently => self.confirm_delete(rt),
            Command::FocusPath => self.focus_path(rt),
            Command::ToggleHidden => {
                self.scene.borrow_mut().browser.toggle_hidden();
                selection_changed = true;
            }
            Command::Copy => self.copy_selection(rt, false),
            Command::Cut => self.copy_selection(rt, true),
            Command::Paste => self.paste(rt),
            Command::Undo => self.undo(rt),
            Command::Refresh => self.refresh(rt),
            Command::Rename => self.start_rename(rt),
            Command::NewFolder => self.start_new(rt, true),
            Command::NewFile => self.start_new(rt, false),
            Command::Terminal => self.open_terminal(rt),
            Command::Filter => {
                let current = self.scene.borrow().browser.filter.clone();
                self.scene.borrow_mut().editing =
                    Editing::Filter(crate::edit::LineEdit::new(&current));
                self.damage_status(rt);
            }
            Command::Sort(key) => self.set_sort(rt, key),
            Command::Typeahead(c) => selection_changed = self.type_ahead(c),
        }
        if selection_changed {
            self.ensure_cursor_visible();
            self.log_selection();
            self.damage_list(rt);
            self.damage_status(rt);
        }
    }

    /// Escape: stop a running operation, else clear the filter, else the selection.
    /// True when the selection or list changed.
    fn dismiss(&mut self) -> bool {
        if self.running.is_some() {
            self.cancel_running();
            return false;
        }
        self.typeahead.clear();
        let mut s = self.scene.borrow_mut();
        if !s.browser.filter.is_empty() {
            s.browser.set_filter("");
            return true;
        }
        let had = !s.browser.selection.is_empty();
        s.browser.selection.clear();
        had
    }

    fn type_ahead(&mut self, c: char) -> bool {
        if self.typeahead_expired() {
            self.typeahead.clear();
        }
        self.typeahead.push(c);
        self.typeahead_at = Instant::now();
        let mut s = self.scene.borrow_mut();
        let names = s.browser.visible_names();
        let cursor = s.browser.selection.cursor();
        // A repeated single letter cycles through the matches.
        let from = if self.typeahead.chars().count() == 1 {
            cursor.map_or(0, |c| c + 1)
        } else {
            cursor.unwrap_or(0)
        };
        match typeahead(&names, &self.typeahead, from) {
            Some(row) => {
                s.browser.go_to_row(row, false);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: Key) -> KeyEvent {
        KeyEvent::new(k)
    }

    fn mods(ctrl: bool, alt: bool, shift: bool) -> Mods {
        Mods {
            ctrl,
            alt,
            shift,
            logo: false,
        }
    }

    fn chord(c: char, m: Mods) -> KeyEvent {
        KeyEvent {
            key: Key::Char(c),
            text: if m.ctrl || m.alt {
                None
            } else {
                Some(c.to_string())
            },
            mods: m,
        }
    }

    #[test]
    fn navigation_keys() {
        assert_eq!(
            command_for(&key(Key::Down)),
            Some(Command::Move {
                delta: 1,
                extend: false
            })
        );
        assert_eq!(
            command_for(&key(Key::Up).with_mods(mods(false, false, true))),
            Some(Command::Move {
                delta: -1,
                extend: true
            })
        );
        assert_eq!(
            command_for(&key(Key::Up).with_mods(mods(false, true, false))),
            Some(Command::Up)
        );
        assert_eq!(
            command_for(&key(Key::Left).with_mods(mods(false, true, false))),
            Some(Command::Back)
        );
        assert_eq!(
            command_for(&key(Key::Right).with_mods(mods(false, true, false))),
            Some(Command::Forward)
        );
        assert_eq!(command_for(&key(Key::Backspace)), Some(Command::Up));
        assert_eq!(command_for(&key(Key::Enter)), Some(Command::Open));
        assert_eq!(
            command_for(&key(Key::Enter).with_mods(mods(true, false, false))),
            Some(Command::OpenNewWindow)
        );
        assert_eq!(
            command_for(&key(Key::Home)),
            Some(Command::First { extend: false })
        );
        assert_eq!(
            command_for(&key(Key::PageDown)),
            Some(Command::Page {
                dir: 1,
                extend: false
            })
        );
    }

    #[test]
    fn destructive_keys_are_distinct() {
        assert_eq!(command_for(&key(Key::Delete)), Some(Command::Trash));
        assert_eq!(
            command_for(&key(Key::Delete).with_mods(mods(false, false, true))),
            Some(Command::DeletePermanently)
        );
        assert_eq!(command_for(&key(Key::Other(KEY_F2))), Some(Command::Rename));
        assert_eq!(
            command_for(&key(Key::Other(KEY_F5))),
            Some(Command::Refresh)
        );
        assert_eq!(command_for(&key(Key::Other(0xFFBE))), None, "F1 is unbound");
    }

    #[test]
    fn ctrl_chords() {
        let ctrl = mods(true, false, false);
        for (c, cmd) in [
            ('a', Command::SelectAll),
            ('i', Command::InvertSelection),
            ('l', Command::FocusPath),
            ('h', Command::ToggleHidden),
            ('c', Command::Copy),
            ('x', Command::Cut),
            ('v', Command::Paste),
            ('z', Command::Undo),
            ('r', Command::Refresh),
            ('1', Command::Sort(SortKey::Name)),
            ('2', Command::Sort(SortKey::Size)),
            ('3', Command::Sort(SortKey::Modified)),
            ('4', Command::Sort(SortKey::Kind)),
        ] {
            assert_eq!(command_for(&chord(c, ctrl)), Some(cmd), "ctrl+{c}");
            assert_eq!(
                command_for(&chord(c.to_ascii_uppercase(), ctrl)),
                Some(cmd),
                "ctrl+{c} upper"
            );
        }
        assert_eq!(
            command_for(&chord('N', mods(true, false, true))),
            Some(Command::NewFolder)
        );
        assert_eq!(
            command_for(&chord('n', ctrl)),
            None,
            "plain ctrl+n is not new folder"
        );
        assert_eq!(
            command_for(&chord('t', mods(true, true, false))),
            Some(Command::Terminal)
        );
        assert_eq!(
            command_for(&chord('n', mods(true, true, false))),
            Some(Command::NewFile)
        );
        assert_eq!(command_for(&chord('q', ctrl)), None);
    }

    #[test]
    fn slash_filters_and_letters_jump() {
        assert_eq!(
            command_for(&chord('/', mods(false, false, false))),
            Some(Command::Filter)
        );
        assert_eq!(
            command_for(&chord('/', mods(false, false, true))),
            Some(Command::Filter)
        );
        assert_eq!(
            command_for(&KeyEvent::typed('b')),
            Some(Command::Typeahead('b'))
        );
        assert_eq!(
            command_for(&KeyEvent::typed('B')),
            Some(Command::Typeahead('B'))
        );
        let no_text = KeyEvent::new(Key::Char('b'));
        assert_eq!(command_for(&no_text), None);
        let mut super_key = KeyEvent::typed('b');
        super_key.mods.logo = true;
        assert_eq!(command_for(&super_key), None);
        let alt_b = chord('b', mods(false, true, false));
        assert_eq!(command_for(&alt_b), None);
    }

    #[test]
    fn modal_keymaps() {
        let conflict = ModalKind::Conflict;
        let k = |c: char| chord(c, Mods::default());
        assert_eq!(modal_key(&conflict, &k('s')), ModalKey::Button(0));
        assert_eq!(modal_key(&conflict, &k('R')), ModalKey::Button(1));
        assert_eq!(modal_key(&conflict, &k('k')), ModalKey::Button(2));
        assert_eq!(modal_key(&conflict, &key(Key::Escape)), ModalKey::Button(3));
        assert_eq!(modal_key(&conflict, &k('a')), ModalKey::ToggleAll);
        assert_eq!(
            modal_key(&conflict, &key(Key::Enter)),
            ModalKey::None,
            "no default for a conflict"
        );
        let confirm = ModalKind::Confirm(crate::ops::Op::Delete { srcs: vec![] });
        assert_eq!(modal_key(&confirm, &k('y')), ModalKey::Button(0));
        assert_eq!(modal_key(&confirm, &key(Key::Enter)), ModalKey::Button(0));
        assert_eq!(modal_key(&confirm, &k('n')), ModalKey::Button(1));
        assert_eq!(modal_key(&confirm, &key(Key::Escape)), ModalKey::Button(1));
        assert_eq!(modal_key(&confirm, &k('x')), ModalKey::None);
        assert_eq!(modal_key(&ModalKind::None, &k('y')), ModalKey::None);
    }
}
