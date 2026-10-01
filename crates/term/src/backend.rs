//! The emulator behind a small, alacritty-free interface.
//!
//! [`Backend`] owns the `alacritty_terminal` grid and its escape sequence parser. Nothing
//! outside this module names an alacritty type: cells come out as [`CellView`], modes as
//! [`ModeView`], events as [`Notice`], positions as viewport [`Pos`]. That keeps the
//! rest of the crate independent of the emulation crate and lets the logic here be
//! tested by feeding bytes, no window needed.

use std::cell::{Cell as StdCell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{self, Config, TermDamage, TermMode, point_to_viewport};
use alacritty_terminal::vte::ansi::{Color, CursorShape, Processor, Rgb};
use alacritty_terminal::{Grid, Term};
use aurora_theme::Rgba;

use crate::colors::ColorRef;
use crate::keys::Modes;
use crate::mouse::Reporting;
use crate::select::SelKind;

/// A cell position in the viewport (row 0 is the top visible line).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

/// Text attributes of a cell.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    /// 0 none, 1 single, 2 double (curly, dotted and dashed draw as single).
    pub underline: u8,
    pub strike: bool,
    pub inverse: bool,
    pub dim: bool,
    pub hidden: bool,
    /// The first half of a double-width character.
    pub wide: bool,
    /// The second half of a double-width character (nothing to draw).
    pub spacer: bool,
}

/// One cell, borrowed from the grid.
#[derive(Debug, Clone, Copy)]
pub struct CellView<'a> {
    pub ch: char,
    /// Combining marks attached to `ch`.
    pub zerowidth: &'a [char],
    pub fg: ColorRef,
    pub bg: ColorRef,
    pub attrs: Attrs,
    /// The cell belongs to an OSC 8 hyperlink.
    pub link: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    Block,
    Underline,
    Beam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorView {
    pub pos: Pos,
    pub kind: CursorKind,
    pub blinking: bool,
    /// The cell under the cursor is double width.
    pub wide: bool,
}

/// Modes the app reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModeView {
    pub keys: Modes,
    pub reporting: Reporting,
    pub sgr_mouse: bool,
    pub alt_screen: bool,
    /// Wheel scrolling sends arrow keys on the alternate screen.
    pub alternate_scroll: bool,
    pub focus_reporting: bool,
    pub bracketed_paste: bool,
}

/// Something the emulator wants the app to do.
pub enum Notice {
    /// New window title; `None` resets to the default.
    Title(Option<String>),
    /// OSC 52 write.
    Clipboard { primary: bool, text: String },
    /// Bytes for the child (query replies).
    PtyWrite(Vec<u8>),
    /// The program asked for a color; `reply` formats the answer for the child.
    ColorRequest {
        index: u16,
        reply: Box<dyn Fn(Rgba) -> String>,
    },
    /// DECSCUSR changed the cursor blinking.
    CursorBlink,
}

/// Damage since the last [`Backend::take_damage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    Full,
    Rows(Vec<RowSpan>),
}

/// Columns `left..=right` of viewport row `row` changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowSpan {
    pub row: usize,
    pub left: usize,
    pub right: usize,
}

/// A selection as the renderer needs it.
#[derive(Debug, Clone, Copy)]
pub struct SelView {
    range: SelectionRange,
    offset: usize,
}

impl SelView {
    pub fn contains(&self, row: usize, col: usize) -> bool {
        let p = term::viewport_to_point(self.offset, Point::new(row, Column(col)));
        self.range.contains(p)
    }
}

struct Listener {
    notices: Rc<RefCell<Vec<Notice>>>,
    window: Rc<StdCell<WindowSize>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut q = self.notices.borrow_mut();
        match event {
            Event::Title(t) => q.push(Notice::Title(Some(t))),
            Event::ResetTitle => q.push(Notice::Title(None)),
            Event::ClipboardStore(ty, text) => q.push(Notice::Clipboard {
                primary: ty == term::ClipboardType::Selection,
                text,
            }),
            Event::PtyWrite(s) => q.push(Notice::PtyWrite(s.into_bytes())),
            Event::ColorRequest(index, f) => q.push(Notice::ColorRequest {
                index: index.min(u16::MAX as usize) as u16,
                reply: Box::new(move |c: Rgba| {
                    f(Rgb {
                        r: c.0[0],
                        g: c.0[1],
                        b: c.0[2],
                    })
                }),
            }),
            Event::TextAreaSizeRequest(f) => {
                q.push(Notice::PtyWrite(f(self.window.get()).into_bytes()));
            }
            Event::CursorBlinkingChange => q.push(Notice::CursorBlink),
            // Output arrival, bell and child exit are handled by the app itself; clipboard
            // loads are refused (OSC 52 is write-only).
            Event::MouseCursorDirty
            | Event::ClipboardLoad(..)
            | Event::Wakeup
            | Event::Bell
            | Event::Exit
            | Event::ChildExit(_) => {}
        }
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct Backend {
    term: Term<Listener>,
    parser: Processor,
    notices: Rc<RefCell<Vec<Notice>>>,
    window: Rc<StdCell<WindowSize>>,
}

fn color_ref(c: Color) -> ColorRef {
    match c {
        Color::Named(n) => ColorRef::Index(n as u16),
        Color::Indexed(i) => ColorRef::Index(i as u16),
        Color::Spec(Rgb { r, g, b }) => ColorRef::Rgb(Rgba::rgb(r, g, b)),
    }
}

fn attrs(f: Flags) -> Attrs {
    Attrs {
        bold: f.contains(Flags::BOLD),
        italic: f.contains(Flags::ITALIC),
        underline: if f.contains(Flags::DOUBLE_UNDERLINE) {
            2
        } else {
            f.intersects(
                Flags::UNDERLINE
                    | Flags::UNDERCURL
                    | Flags::DOTTED_UNDERLINE
                    | Flags::DASHED_UNDERLINE,
            ) as u8
        },
        strike: f.contains(Flags::STRIKEOUT),
        inverse: f.contains(Flags::INVERSE),
        dim: f.contains(Flags::DIM),
        hidden: f.contains(Flags::HIDDEN),
        wide: f.contains(Flags::WIDE_CHAR),
        spacer: f.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
    }
}

fn sel_type(kind: SelKind) -> SelectionType {
    match kind {
        SelKind::Simple => SelectionType::Simple,
        SelKind::Word => SelectionType::Semantic,
        SelKind::Line => SelectionType::Lines,
    }
}

impl Backend {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let notices = Rc::new(RefCell::new(Vec::new()));
        let window = Rc::new(StdCell::new(WindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width: 1,
            cell_height: 1,
        }));
        let listener = Listener {
            notices: notices.clone(),
            window: window.clone(),
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let term = Term::new(config, &Size { cols, rows }, listener);
        Self {
            term,
            parser: Processor::new(),
            notices,
            window,
        }
    }

    pub fn cols(&self) -> usize {
        self.term.columns()
    }

    pub fn rows(&self) -> usize {
        self.term.screen_lines()
    }

    pub fn history_size(&self) -> usize {
        self.term.history_size()
    }

    /// Parse output from the child.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// When a synchronized update (DECSET 2026) must be forced to end, if one is open.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// End an open synchronized update (its timeout passed).
    pub fn end_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
    }

    pub fn resize(&mut self, cols: usize, rows: usize, cell_w: u32, cell_h: u32) {
        self.window.set(WindowSize {
            num_lines: rows.min(u16::MAX as usize) as u16,
            num_cols: cols.min(u16::MAX as usize) as u16,
            cell_width: cell_w.min(u16::MAX as u32) as u16,
            cell_height: cell_h.min(u16::MAX as u32) as u16,
        });
        if cols != self.cols() || rows != self.rows() {
            self.term.resize(Size { cols, rows });
        }
    }

    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut *self.notices.borrow_mut())
    }

    /// Changes since the last call, as viewport rows.
    pub fn take_damage(&mut self) -> Damage {
        let out = match self.term.damage() {
            TermDamage::Full => Damage::Full,
            TermDamage::Partial(it) => Damage::Rows(
                it.map(|d| RowSpan {
                    row: d.line,
                    left: d.left,
                    right: d.right,
                })
                .collect(),
            ),
        };
        self.term.reset_damage();
        out
    }

    pub fn modes(&self) -> ModeView {
        let m = *self.term.mode();
        let reporting = if m.contains(TermMode::MOUSE_MOTION) {
            Reporting::Motion
        } else if m.contains(TermMode::MOUSE_DRAG) {
            Reporting::Drag
        } else if m.contains(TermMode::MOUSE_REPORT_CLICK) {
            Reporting::Click
        } else {
            Reporting::Off
        };
        ModeView {
            keys: Modes {
                app_cursor: m.contains(TermMode::APP_CURSOR),
                app_keypad: m.contains(TermMode::APP_KEYPAD),
            },
            reporting,
            sgr_mouse: m.contains(TermMode::SGR_MOUSE),
            alt_screen: m.contains(TermMode::ALT_SCREEN),
            alternate_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
            focus_reporting: m.contains(TermMode::FOCUS_IN_OUT),
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
        }
    }

    fn grid(&self) -> &Grid<term::cell::Cell> {
        self.term.grid()
    }

    /// The cell at a viewport position; `None` outside the grid.
    pub fn cell(&self, row: usize, col: usize) -> Option<CellView<'_>> {
        if row >= self.rows() || col >= self.cols() {
            return None;
        }
        let line = Line(row as i32 - self.grid().display_offset() as i32);
        let c = &self.grid()[line][Column(col)];
        Some(CellView {
            ch: c.c,
            zerowidth: c.zerowidth().unwrap_or(&[]),
            fg: color_ref(c.fg),
            bg: color_ref(c.bg),
            attrs: attrs(c.flags),
            link: c.extra.is_some() && c.hyperlink().is_some(),
        })
    }

    /// The URI of the hyperlink at a cell.
    pub fn link_at(&self, pos: Pos) -> Option<String> {
        let line = Line(pos.row as i32 - self.grid().display_offset() as i32);
        if pos.row >= self.rows() || pos.col >= self.cols() {
            return None;
        }
        self.grid()[line][Column(pos.col)]
            .hyperlink()
            .map(|h| h.uri().to_string())
    }

    /// A color the program set through OSC 4/10/11/12.
    pub fn color_override(&self, index: u16) -> Option<Rgba> {
        let i = index as usize;
        (i < 269)
            .then(|| self.term.colors()[i])
            .flatten()
            .map(|c| Rgba::rgb(c.r, c.g, c.b))
    }

    /// The cursor if it is visible in the viewport.
    pub fn cursor(&self) -> Option<CursorView> {
        let style = self.term.cursor_style();
        let kind = match style.shape {
            CursorShape::Hidden => return None,
            CursorShape::Underline => CursorKind::Underline,
            CursorShape::Beam => CursorKind::Beam,
            CursorShape::Block | CursorShape::HollowBlock => CursorKind::Block,
        };
        if !self.term.mode().contains(TermMode::SHOW_CURSOR) {
            return None;
        }
        let p = self.grid().cursor.point;
        let v = point_to_viewport(self.grid().display_offset(), p)?;
        if v.line >= self.rows() {
            return None;
        }
        let wide = self.grid()[p.line][p.column]
            .flags
            .contains(Flags::WIDE_CHAR);
        Some(CursorView {
            pos: Pos {
                row: v.line,
                col: v.column.0,
            },
            kind,
            blinking: style.blinking,
            wide,
        })
    }

    // Scrolling.

    pub fn display_offset(&self) -> usize {
        self.grid().display_offset()
    }

    /// Scroll the view by `lines` (positive: into the history).
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_page(&mut self, up: bool) {
        self.term
            .scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    // Selection.

    fn point(&self, pos: Pos) -> Point {
        let row = pos.row.min(self.rows() - 1);
        let col = pos.col.min(self.cols() - 1);
        term::viewport_to_point(self.display_offset(), Point::new(row, Column(col)))
    }

    /// Start a selection of `kind` at `pos`; `right` is the half of the cell it started in.
    pub fn select_begin(&mut self, kind: SelKind, pos: Pos, right: bool) {
        let side = if right { Side::Right } else { Side::Left };
        self.term.selection = Some(Selection::new(sel_type(kind), self.point(pos), side));
    }

    /// Move the free end of the selection. Returns false when there is none.
    pub fn select_update(&mut self, pos: Pos, right: bool) -> bool {
        let side = if right { Side::Right } else { Side::Left };
        let p = self.point(pos);
        match self.term.selection.as_mut() {
            Some(s) => {
                s.update(p, side);
                true
            }
            None => false,
        }
    }

    pub fn select_clear(&mut self) -> bool {
        self.term.selection.take().is_some()
    }

    pub fn has_selection(&self) -> bool {
        self.term.selection.is_some()
    }

    /// The selected text, if the selection covers anything.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// The selection for painting, if it covers anything.
    pub fn selection_view(&self) -> Option<SelView> {
        let range = self.term.selection.as_ref()?.to_range(&self.term)?;
        Some(SelView {
            range,
            offset: self.display_offset(),
        })
    }

    /// Viewport rows touched by the selection (for damage), clamped to the screen.
    pub fn selection_rows(&self) -> Option<(usize, usize)> {
        let v = self.selection_view()?;
        let top = (v.range.start.line.0 + v.offset as i32).max(0) as usize;
        let bottom = (v.range.end.line.0 + v.offset as i32).max(0) as usize;
        (top < self.rows()).then(|| (top, bottom.min(self.rows() - 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cols: usize, rows: usize) -> Backend {
        Backend::new(cols, rows, 100)
    }

    fn row_text(b: &Backend, row: usize) -> String {
        (0..b.cols())
            .filter_map(|c| b.cell(row, c))
            .filter(|c| !c.attrs.spacer)
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn output_lands_in_cells() {
        let mut b = term(10, 3);
        b.feed(b"hello\r\nworld");
        assert_eq!(row_text(&b, 0), "hello");
        assert_eq!(row_text(&b, 1), "world");
        assert!(b.cell(3, 0).is_none() && b.cell(0, 10).is_none());
    }

    #[test]
    fn colors_and_attributes() {
        let mut b = term(10, 2);
        b.feed(b"\x1b[1;31mA\x1b[0m\x1b[38;2;1;2;3mB\x1b[48;5;200mC\x1b[4;7mD");
        let a = b.cell(0, 0).expect("cell");
        assert!(a.attrs.bold);
        assert_eq!(a.fg, ColorRef::Index(1));
        assert_eq!(
            b.cell(0, 1).expect("cell").fg,
            ColorRef::Rgb(Rgba::rgb(1, 2, 3))
        );
        assert_eq!(b.cell(0, 2).expect("cell").bg, ColorRef::Index(200));
        let d = b.cell(0, 3).expect("cell");
        assert!(d.attrs.underline == 1 && d.attrs.inverse);
        // Untouched cells use the named defaults.
        let e = b.cell(0, 8).expect("cell");
        assert_eq!(e.fg, ColorRef::Index(crate::colors::FOREGROUND));
        assert_eq!(e.bg, ColorRef::Index(crate::colors::BACKGROUND));
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let mut b = term(10, 1);
        b.feed("a日b".as_bytes());
        let wide = b.cell(0, 1).expect("cell");
        assert!(wide.attrs.wide && wide.ch == '日');
        assert!(b.cell(0, 2).expect("cell").attrs.spacer);
        assert_eq!(b.cell(0, 3).expect("cell").ch, 'b');
    }

    #[test]
    fn combining_marks_stay_with_their_cell() {
        let mut b = term(10, 1);
        b.feed("e\u{301}x".as_bytes());
        let c = b.cell(0, 0).expect("cell");
        assert_eq!((c.ch, c.zerowidth), ('e', &['\u{301}'][..]));
        assert_eq!(b.cell(0, 1).expect("cell").ch, 'x');
    }

    #[test]
    fn damage_covers_changed_rows_then_resets() {
        let mut b = term(10, 4);
        assert_eq!(b.take_damage(), Damage::Full);
        b.feed(b"\x1b[3;2Hxy");
        match b.take_damage() {
            Damage::Rows(rows) => {
                assert!(
                    rows.iter()
                        .any(|r| r.row == 2 && r.left <= 1 && r.right >= 2)
                );
                assert!(rows.iter().all(|r| r.row == 2 || r.row == 0));
            }
            Damage::Full => panic!("partial damage expected"),
        }
        // Nothing new: only the cursor cell is reported.
        match b.take_damage() {
            Damage::Rows(rows) => assert!(rows.iter().all(|r| r.row == 2)),
            Damage::Full => panic!("partial damage expected"),
        }
        b.resize(12, 5, 8, 16);
        assert_eq!(b.take_damage(), Damage::Full);
        assert_eq!((b.cols(), b.rows()), (12, 5));
    }

    #[test]
    fn mode_changes_are_visible() {
        let mut b = term(10, 2);
        assert_eq!(b.modes(), ModeView::default().tap_defaults());
        b.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?1049h\x1b[?1004h");
        let m = b.modes();
        assert!(m.keys.app_cursor && m.bracketed_paste && m.sgr_mouse);
        assert!(m.alt_screen && m.focus_reporting);
        assert_eq!(m.reporting, Reporting::Drag);
        b.feed(b"\x1b[?1003h");
        assert_eq!(b.modes().reporting, Reporting::Motion);
        b.feed(b"\x1b[?1049l\x1b[?1l");
        assert!(!b.modes().alt_screen && !b.modes().keys.app_cursor);
    }

    impl ModeView {
        /// What a fresh terminal reports (alternate scroll is on by default).
        fn tap_defaults(mut self) -> Self {
            self.alternate_scroll = true;
            self
        }
    }

    #[test]
    fn title_and_clipboard_events_become_notices() {
        let mut b = term(10, 2);
        b.feed(b"\x1b]2;my title\x07");
        b.feed(b"\x1b]52;c;aGk=\x07");
        let notices = b.take_notices();
        assert!(matches!(&notices[0], Notice::Title(Some(t)) if t == "my title"));
        assert!(matches!(
            &notices[1],
            Notice::Clipboard { primary: false, text } if text == "hi"
        ));
        assert!(b.take_notices().is_empty());
    }

    #[test]
    fn queries_are_answered_through_pty_writes() {
        let mut b = term(10, 2);
        b.feed(b"\x1b[5n");
        let notices = b.take_notices();
        assert!(matches!(&notices[0], Notice::PtyWrite(w) if w == b"\x1b[0n"));
        b.resize(10, 2, 9, 18);
        b.feed(b"\x1b[18t\x1b[14t");
        let writes: Vec<String> = b
            .take_notices()
            .into_iter()
            .filter_map(|n| match n {
                Notice::PtyWrite(w) => String::from_utf8(w).ok(),
                _ => None,
            })
            .collect();
        assert_eq!(writes, ["\x1b[8;2;10t", "\x1b[4;36;90t"]);
    }

    #[test]
    fn color_requests_carry_a_reply_formatter() {
        let mut b = term(10, 2);
        b.feed(b"\x1b]11;?\x07");
        let notices = b.take_notices();
        match &notices[0] {
            Notice::ColorRequest { index, reply } => {
                assert_eq!(*index, crate::colors::BACKGROUND);
                assert!(reply(Rgba::rgb(0, 0, 0)).starts_with("\x1b]11;rgb:"));
            }
            _ => panic!("expected a color request"),
        }
        b.feed(b"\x1b]4;1;rgb:ff/00/00\x07");
        assert_eq!(b.color_override(1), Some(Rgba::rgb(255, 0, 0)));
        assert_eq!(b.color_override(2), None);
    }

    #[test]
    fn selection_by_drag_word_and_line() {
        let mut b = term(20, 3);
        b.feed(b"foo bar baz\r\nsecond line");
        b.select_begin(SelKind::Simple, Pos { row: 0, col: 4 }, false);
        assert!(b.select_update(Pos { row: 0, col: 6 }, true));
        assert_eq!(b.selection_text().as_deref(), Some("bar"));
        assert_eq!(b.selection_rows(), Some((0, 0)));
        let sel = b.selection_view().expect("selection");
        assert!(sel.contains(0, 5) && !sel.contains(0, 3) && !sel.contains(1, 5));

        b.select_begin(SelKind::Word, Pos { row: 0, col: 5 }, false);
        assert_eq!(b.selection_text().as_deref(), Some("bar"));
        b.select_begin(SelKind::Line, Pos { row: 1, col: 3 }, false);
        assert_eq!(b.selection_text().as_deref(), Some("second line\n"));
        assert!(b.select_clear() && !b.has_selection());
        assert!(!b.select_update(Pos::default(), false));
        assert_eq!(b.selection_text(), None);
    }

    #[test]
    fn a_click_without_drag_selects_nothing() {
        let mut b = term(20, 3);
        b.feed(b"foo");
        b.select_begin(SelKind::Simple, Pos { row: 0, col: 1 }, false);
        assert_eq!(b.selection_text(), None);
        assert!(b.selection_view().is_none());
    }

    #[test]
    fn scrollback_scrolls_and_snaps_back() {
        let mut b = Backend::new(10, 3, 50);
        for i in 0..10 {
            b.feed(format!("line{i}\r\n").as_bytes());
        }
        assert!(b.history_size() >= 7);
        let bottom = row_text(&b, 0);
        b.scroll(2);
        assert_eq!(b.display_offset(), 2);
        assert_ne!(row_text(&b, 0), bottom);
        b.scroll_page(false);
        assert_eq!(b.display_offset(), 0);
        b.scroll(5);
        b.scroll_to_bottom();
        assert_eq!(b.display_offset(), 0);
        assert_eq!(row_text(&b, 0), bottom);
    }

    #[test]
    fn cursor_follows_output_and_modes() {
        let mut b = term(10, 3);
        b.feed(b"ab");
        let c = b.cursor().expect("cursor");
        assert_eq!((c.pos, c.kind), (Pos { row: 0, col: 2 }, CursorKind::Block));
        b.feed(b"\x1b[?25l");
        assert!(b.cursor().is_none());
        b.feed(b"\x1b[?25h\x1b[6 q");
        let c = b.cursor().expect("cursor");
        assert_eq!(c.kind, CursorKind::Beam);
        assert!(!c.blinking);
        b.feed(b"\x1b[5 q");
        assert!(b.cursor().expect("cursor").blinking);
    }

    #[test]
    fn hyperlinks_are_stored_per_cell() {
        let mut b = term(20, 2);
        b.feed(b"\x1b]8;;https://example.org\x1b\\link\x1b]8;;\x1b\\ plain");
        assert!(b.cell(0, 0).expect("cell").link);
        assert!(!b.cell(0, 6).expect("cell").link);
        assert_eq!(
            b.link_at(Pos { row: 0, col: 2 }).as_deref(),
            Some("https://example.org")
        );
        assert_eq!(b.link_at(Pos { row: 0, col: 7 }), None);
    }

    #[test]
    fn alt_screen_has_its_own_content() {
        let mut b = term(10, 2);
        b.feed(b"main\x1b[?1049h\x1b[Halt");
        assert_eq!(row_text(&b, 0), "alt");
        b.feed(b"\x1b[?1049l");
        assert_eq!(row_text(&b, 0), "main");
    }

    #[test]
    fn named_color_numbering_matches_the_scheme() {
        use crate::colors::{BACKGROUND, CURSOR, DIM_FIRST, FOREGROUND};
        use alacritty_terminal::vte::ansi::NamedColor;
        assert_eq!(NamedColor::Foreground as u16, FOREGROUND);
        assert_eq!(NamedColor::Background as u16, BACKGROUND);
        assert_eq!(NamedColor::Cursor as u16, CURSOR);
        assert_eq!(NamedColor::DimBlack as u16, DIM_FIRST);
        assert_eq!(NamedColor::BrightWhite as u16, 15);
    }
}
