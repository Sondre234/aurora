//! A one-line text editor (path bar, filter, rename, new name). Pure: it takes
//! [`KeyEvent`]s and changes a string, nothing more.

use aurora_ui::{Key, KeyEvent};

/// What a key did to the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    /// The text changed.
    Changed,
    /// Only the caret or selection moved.
    Moved,
    Submit,
    Cancel,
    /// The key means nothing to an editor.
    Ignored,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineEdit {
    text: String,
    /// Caret, a byte index on a char boundary.
    cursor: usize,
    /// The other end of the selection.
    anchor: Option<usize>,
}

impl LineEdit {
    /// Caret at the end.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            cursor: text.len(),
            anchor: None,
        }
    }

    /// Everything before the last extension selected, as file managers do on rename.
    pub fn with_stem_selected(text: &str) -> Self {
        let (stem, _) = crate::names::split_ext(text.as_bytes());
        let end = stem.len();
        Self {
            text: text.to_string(),
            cursor: end,
            anchor: Some(0),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Selected byte range, if any.
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.anchor
            .filter(|&a| a != self.cursor)
            .map(|a| (a.min(self.cursor), a.max(self.cursor)))
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.anchor = None;
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i]
            .char_indices()
            .next_back()
            .map_or(0, |(p, _)| p)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..]
            .chars()
            .next()
            .map_or(self.text.len(), |c| i + c.len_utf8())
    }

    fn word_back(&self, i: usize) -> usize {
        let head = &self.text[..i];
        let trimmed = head.trim_end_matches(|c: char| !c.is_alphanumeric());
        trimmed
            .trim_end_matches(|c: char| c.is_alphanumeric())
            .len()
    }

    fn word_forward(&self, i: usize) -> usize {
        let tail = &self.text[i..];
        let rest = tail.trim_start_matches(|c: char| c.is_alphanumeric());
        let rest = rest.trim_start_matches(|c: char| !c.is_alphanumeric());
        self.text.len() - rest.len()
    }

    fn delete_selection(&mut self) -> bool {
        match self.selection() {
            Some((a, b)) => {
                self.text.replace_range(a..b, "");
                self.cursor = a;
                self.anchor = None;
                true
            }
            None => {
                self.anchor = None;
                false
            }
        }
    }

    /// Inserts text at the caret (replacing the selection). Control characters are dropped.
    pub fn insert(&mut self, s: &str) {
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        if clean.is_empty() {
            return;
        }
        self.delete_selection();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    fn move_to(&mut self, to: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = to;
    }

    /// Feeds one key.
    pub fn key(&mut self, k: &KeyEvent) -> EditOutcome {
        let m = k.mods;
        let word = m.ctrl;
        match k.key {
            Key::Enter => EditOutcome::Submit,
            Key::Escape => EditOutcome::Cancel,
            Key::Left => {
                if let (Some((a, _)), false) = (self.selection(), m.shift) {
                    self.cursor = a;
                    self.anchor = None;
                } else {
                    let to = if word {
                        self.word_back(self.cursor)
                    } else {
                        self.prev(self.cursor)
                    };
                    self.move_to(to, m.shift);
                }
                EditOutcome::Moved
            }
            Key::Right => {
                if let (Some((_, b)), false) = (self.selection(), m.shift) {
                    self.cursor = b;
                    self.anchor = None;
                } else {
                    let to = if word {
                        self.word_forward(self.cursor)
                    } else {
                        self.next(self.cursor)
                    };
                    self.move_to(to, m.shift);
                }
                EditOutcome::Moved
            }
            Key::Home => {
                self.move_to(0, m.shift);
                EditOutcome::Moved
            }
            Key::End => {
                self.move_to(self.text.len(), m.shift);
                EditOutcome::Moved
            }
            Key::Backspace => {
                if self.delete_selection() {
                    return EditOutcome::Changed;
                }
                let from = if word {
                    self.word_back(self.cursor)
                } else {
                    self.prev(self.cursor)
                };
                if from == self.cursor {
                    return EditOutcome::Ignored;
                }
                self.text.replace_range(from..self.cursor, "");
                self.cursor = from;
                EditOutcome::Changed
            }
            Key::Delete => {
                if self.delete_selection() {
                    return EditOutcome::Changed;
                }
                let to = if word {
                    self.word_forward(self.cursor)
                } else {
                    self.next(self.cursor)
                };
                if to == self.cursor {
                    return EditOutcome::Ignored;
                }
                self.text.replace_range(self.cursor..to, "");
                EditOutcome::Changed
            }
            Key::Char(c) if m.ctrl && !m.alt => match c.to_ascii_lowercase() {
                'a' => {
                    self.select_all();
                    EditOutcome::Moved
                }
                'u' => {
                    self.text.replace_range(..self.cursor, "");
                    self.cursor = 0;
                    self.anchor = None;
                    EditOutcome::Changed
                }
                _ => EditOutcome::Ignored,
            },
            _ if m.ctrl || m.alt || m.logo => EditOutcome::Ignored,
            _ => match &k.text {
                Some(t) if t.chars().any(|c| !c.is_control()) => {
                    self.insert(t);
                    EditOutcome::Changed
                }
                _ => EditOutcome::Ignored,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_ui::Mods;

    fn typed(e: &mut LineEdit, s: &str) {
        for c in s.chars() {
            e.key(&KeyEvent::typed(c));
        }
    }

    fn press(e: &mut LineEdit, key: Key) -> EditOutcome {
        e.key(&KeyEvent::new(key))
    }

    fn with(key: Key, ctrl: bool, shift: bool) -> KeyEvent {
        KeyEvent::new(key).with_mods(Mods {
            ctrl,
            shift,
            ..Mods::default()
        })
    }

    #[test]
    fn typing_and_deleting() {
        let mut e = LineEdit::new("");
        typed(&mut e, "héllo");
        assert_eq!(e.text(), "héllo");
        assert_eq!(press(&mut e, Key::Backspace), EditOutcome::Changed);
        assert_eq!(e.text(), "héll");
        press(&mut e, Key::Home);
        assert_eq!(press(&mut e, Key::Backspace), EditOutcome::Ignored);
        press(&mut e, Key::Delete);
        assert_eq!(e.text(), "éll");
        press(&mut e, Key::Right);
        assert_eq!(e.cursor(), 2, "é is two bytes");
        press(&mut e, Key::End);
        assert_eq!(press(&mut e, Key::Delete), EditOutcome::Ignored);
    }

    #[test]
    fn selection_is_replaced_by_typing() {
        let mut e = LineEdit::with_stem_selected("report.final.txt");
        assert_eq!(e.selection(), Some((0, 12)));
        typed(&mut e, "x");
        assert_eq!(e.text(), "x.txt");
        let mut e = LineEdit::with_stem_selected(".bashrc");
        assert_eq!(e.selection(), Some((0, 7)));
        e.key(&KeyEvent::new(Key::Backspace));
        assert_eq!(e.text(), "");
    }

    #[test]
    fn shift_extends_and_arrows_collapse() {
        let mut e = LineEdit::new("abcd");
        e.key(&with(Key::Left, false, true));
        e.key(&with(Key::Left, false, true));
        assert_eq!(e.selection(), Some((2, 4)));
        e.key(&with(Key::Left, false, false));
        assert_eq!((e.selection(), e.cursor()), (None, 2));
        e.select_all();
        e.key(&with(Key::Right, false, false));
        assert_eq!((e.selection(), e.cursor()), (None, 4));
    }

    #[test]
    fn word_motions() {
        let mut e = LineEdit::new("/home/user/my file");
        e.key(&with(Key::Backspace, true, false));
        assert_eq!(e.text(), "/home/user/my ");
        e.key(&with(Key::Backspace, true, false));
        assert_eq!(e.text(), "/home/user/");
        e.key(&with(Key::Home, false, false));
        e.key(&with(Key::Right, true, false));
        assert_eq!(e.cursor(), 1, "to the end of the first word run's start");
    }

    #[test]
    fn ctrl_keys_and_control_text() {
        let mut e = LineEdit::new("abc");
        let ctrl_u = KeyEvent::typed('u').with_mods(Mods {
            ctrl: true,
            ..Mods::default()
        });
        assert_eq!(e.key(&ctrl_u), EditOutcome::Changed);
        assert_eq!(e.text(), "");
        let mut e = LineEdit::new("abc");
        let ctrl_a = KeyEvent::typed('a').with_mods(Mods {
            ctrl: true,
            ..Mods::default()
        });
        assert_eq!(e.key(&ctrl_a), EditOutcome::Moved);
        assert_eq!(e.selection(), Some((0, 3)));
        let ctrl_x = KeyEvent::typed('x').with_mods(Mods {
            ctrl: true,
            ..Mods::default()
        });
        assert_eq!(e.key(&ctrl_x), EditOutcome::Ignored);
        e.insert("a\nb\t");
        assert_eq!(e.text(), "ab", "control characters never get in");
        assert_eq!(e.key(&KeyEvent::new(Key::Enter)), EditOutcome::Submit);
        assert_eq!(e.key(&KeyEvent::new(Key::Escape)), EditOutcome::Cancel);
        assert_eq!(e.key(&KeyEvent::new(Key::Tab)), EditOutcome::Ignored);
    }
}
