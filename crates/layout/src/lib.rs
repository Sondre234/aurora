//! Pure layout logic: no Smithay types, no dependencies. Window rectangles are plain
//! retained state (`Placement`), so a later milestone can animate them.

mod dwindle;
mod geom;
mod tiling;
mod workspace;

pub use dwindle::Dwindle;
pub use geom::{drop_side, edges_for_point, neighbor};
pub use tiling::{AxisCtl, InsertHint, ResizeHandle, TilingLayout};
pub use workspace::Workspace;

use std::ops::{BitOr, BitOrAssign};

/// Compositor-assigned window identity; never reused within a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WinId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(self) -> i32 {
        self.y + self.h
    }

    pub fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Shrinks each side by its own amount (CSS order); negative values grow. Never
    /// yields a negative size: an over-shrunk axis collapses to zero at its centre.
    pub fn inset(self, top: i32, right: i32, bottom: i32, left: i32) -> Rect {
        let (x, w) = shrink_axis(self.x, self.w, left, right);
        let (y, h) = shrink_axis(self.y, self.h, top, bottom);
        Rect { x, y, w, h }
    }

    /// Shrinks every side by `d`; negative grows.
    pub fn shrink(self, d: i32) -> Rect {
        self.inset(d, d, d, d)
    }

    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    pub fn center(self) -> Point {
        Point {
            x: self.x + self.w / 2,
            y: self.y + self.h / 2,
        }
    }

    /// `None` when the overlap is empty.
    pub fn intersect(self, o: Rect) -> Option<Rect> {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        (r > x && b > y).then_some(Rect {
            x,
            y,
            w: r - x,
            h: b - y,
        })
    }
}

fn shrink_axis(pos: i32, len: i32, lo: i32, hi: i32) -> (i32, i32) {
    let l = len - lo - hi;
    if l < 0 {
        (pos + len / 2, 0)
    } else {
        (pos + lo, l)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Children sit side by side.
    Horizontal,
    /// Children are stacked.
    Vertical,
}

/// Which child of a split: left/top is `First`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    First,
    Second,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges(u8);

impl Edges {
    pub const NONE: Self = Self(0);
    pub const LEFT: Self = Self(1);
    pub const RIGHT: Self = Self(2);
    pub const TOP: Self = Self(4);
    pub const BOTTOM: Self = Self(8);

    pub fn contains(self, o: Self) -> bool {
        self.0 & o.0 == o.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for Edges {
    type Output = Self;
    fn bitor(self, o: Self) -> Self {
        Self(self.0 | o.0)
    }
}

impl BitOrAssign for Edges {
    fn bitor_assign(&mut self, o: Self) {
        self.0 |= o.0;
    }
}

/// Size hints in content space (excluding border and gaps); 0 means unconstrained.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Constraints {
    pub min: Size,
    pub max: Size,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gaps {
    /// Half the distance between two tiled windows (each window keeps this margin).
    pub inner: i32,
    /// Distance between a tiled window and the edge of the work area.
    pub outer: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NewWindowSide {
    First,
    #[default]
    Second,
    /// Decided by which half of the target the pointer is over.
    Pointer,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutParams {
    pub gaps: Gaps,
    pub border: i32,
    pub new_window_side: NewWindowSide,
    /// Smallest share either side of a split may get from resizing.
    pub min_ratio: f32,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self {
            gaps: Gaps::default(),
            border: 0,
            new_window_side: NewWindowSide::default(),
            min_ratio: 0.1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Tiled,
    Floating,
    Maximized,
    Fullscreen,
}

/// Maximized keeps the work area (bars stay visible), fullscreen covers the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsMode {
    Maximized,
    Fullscreen,
}

/// Where a window should be: `outer` includes the border, `content` is the surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub id: WinId,
    pub outer: Rect,
    pub content: Rect,
    pub kind: Kind,
}
