//! Input events fed to a [`crate::Ui`] and events it reports back.
//!
//! The Wayland runtime maps seat events to [`Input`]; headless tests construct them
//! directly. Positions are logical surface coordinates.

use crate::geom::Point;

/// Identifies a widget for lookup and event routing. `Id(0)` means "no id".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Id(pub u32);

impl Id {
    pub const NONE: Id = Id(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Right,
    Middle,
    Other(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

/// Keys the toolkit itself understands; everything else is [`Key::Other`] with the raw
/// keysym so apps can still match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Enter,
    Escape,
    Backspace,
    Delete,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    /// A printable key; the character is the un-shifted-layout interpretation and the
    /// actual typed text is in [`KeyEvent::text`].
    Char(char),
    Other(u32),
}

/// A key press or auto-repeat (releases are not delivered to widgets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    /// Text the key produced with the current modifiers and layout, if any.
    pub text: Option<String>,
    pub mods: Mods,
}

impl KeyEvent {
    pub fn new(key: Key) -> Self {
        Self { key, text: None, mods: Mods::default() }
    }

    /// A typed character without modifiers.
    pub fn typed(c: char) -> Self {
        Self { key: Key::Char(c), text: Some(c.to_string()), mods: Mods::default() }
    }

    pub fn with_mods(mut self, mods: Mods) -> Self {
        self.mods = mods;
        self
    }
}

/// Raw input delivered to [`crate::Ui::handle`].
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    PointerMove(Point),
    PointerLeave,
    PointerDown(Point, Button),
    PointerUp(Point, Button),
    /// Scroll in logical pixels; positive `dy` scrolls content down (towards later items).
    Scroll { pos: Point, dx: f32, dy: f32 },
    Key(KeyEvent),
}

/// What a [`crate::Ui`] reports after handling input.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    /// A clickable widget was pressed and released on itself.
    Clicked(Id),
    /// Selection of a list changed (by key, wheel-less click or [`crate::Ui::list_move`]).
    Selected { list: Id, index: usize },
    /// A list row was activated (click release or Enter).
    Activated { list: Id, index: usize },
    /// The text of a text input changed through editing.
    TextChanged(Id),
    /// Enter pressed in a text input.
    Submitted(Id),
    /// Focus moved to another widget (or away).
    FocusChanged(Option<Id>),
    /// A key no focused widget consumed (Escape, arrows for the app to interpret, ...).
    Key(KeyEvent),
}
