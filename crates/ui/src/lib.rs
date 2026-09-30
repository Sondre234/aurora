//! `aurora-ui`: shared client-side UI toolkit for Aurora's layer-shell and session-lock
//! service clients (bar, launcher, notification daemon, lock screen).
//!
//! Layers, bottom to top:
//!
//! - [`geom`]: plain values (`Rect`, `Insets`, `Color`); colors and sizes are not tied to
//!   any theme.
//! - [`cache`], [`text`]: byte-budgeted LRU caches with `perf` tracing metrics, and the
//!   shared cosmic-text shaping/glyph engine ([`TextSystem`]).
//! - [`painter`], [`skia`]: the [`Painter`] trait and its tiny-skia backend drawing into
//!   ARGB8888 memory ([`PixelBuffer`] headless, `wl_shm` memory at runtime).
//! - `widget` layer (added in the following modules): retained tree, layout, hit-testing,
//!   hover/press/focus state and damage tracking.
//! - `runtime`: Wayland surface runners, shm pool, scaling, input and the calloop loop.

pub mod cache;
pub mod damage;
pub mod geom;
pub mod input;
pub mod painter;
pub mod skia;
pub mod text;
pub mod ui;
pub mod widget;

#[cfg(test)]
mod text_tests;
#[cfg(test)]
mod ui_tests;

pub use geom::{Color, Insets, Point, Rect, Size};
pub use input::{Button, Id, Input, Key, KeyEvent, Mods, UiEvent};
pub use painter::{Image, Painter};
pub use ui::Ui;
pub use widget::{Align, Dim, Justify, List, ListItem, Node, Shadow, Style, TextAlign, TextInput};
pub use skia::{PaintBudgets, PaintCaches, PixelBuffer, SkiaPainter};
pub use text::{FontFamily, ShapedText, TextBudgets, TextStyle, TextSystem, TextWrap};
