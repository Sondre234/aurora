//! The layout interface. Everything is tolerant of unknown `WinId`s.

use crate::{
    Axis, Constraints, Edges, LayoutParams, NewWindowSide, Placement, Point, Rect, Side, WinId,
};

/// Where a new window should go. `after` is normally the focused window.
#[derive(Clone, Copy, Debug, Default)]
pub struct InsertHint {
    pub after: Option<WinId>,
    pub side: NewWindowSide,
    pub pointer: Option<Point>,
}

/// One draggable boundary: the ratio at drag start and the length it spans.
#[derive(Clone, Copy, Debug)]
pub struct AxisCtl {
    pub axis: Axis,
    pub node: usize,
    pub start: f32,
    pub len: i32,
}

/// State of a running tiled resize. Stale (and ignored) once the tree changes shape.
#[derive(Clone, Copy, Debug)]
pub struct ResizeHandle {
    pub shape: u64,
    pub ctls: [Option<AxisCtl>; 2],
}

pub trait TilingLayout {
    fn insert(&mut self, id: WinId, hint: InsertHint, c: Constraints);
    fn remove(&mut self, id: WinId) -> bool;
    fn contains(&self, id: WinId) -> bool;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn swap(&mut self, a: WinId, b: WinId) -> bool;
    /// Moves `id` next to `target`, on `side` of it.
    fn move_beside(&mut self, id: WinId, target: WinId, side: Side) -> bool;
    fn set_constraints(&mut self, id: WinId, c: Constraints) -> bool;
    fn toggle_split(&mut self, id: WinId) -> bool;
    fn resize_start(&self, id: WinId, edges: Edges) -> Option<ResizeHandle>;
    /// `dx`/`dy` are total pointer movement since `resize_start`, so drags never
    /// accumulate rounding error. Call `relayout` afterwards.
    fn resize_set(&mut self, h: &ResizeHandle, dx: i32, dy: i32) -> bool;
    fn relayout(&mut self, area: Rect, params: &LayoutParams);
    /// The window's outer rectangle (border included) from the last relayout.
    fn rect(&self, id: WinId) -> Option<Rect>;
    /// Appends one placement per window, in a stable order.
    fn placements(&self, out: &mut Vec<Placement>);
    fn windows(&self, out: &mut Vec<WinId>);
    /// Changes whenever any placement may have changed.
    fn epoch(&self) -> u64;
}
