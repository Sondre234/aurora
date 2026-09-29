//! Interactive pointer drags: move and resize of floating windows, split resize of tiled
//! ones and drag-and-drop reordering. A grab holds only a `WinId` and plain rectangles
//! (grabs must be `Send`), so a window closed mid-drag just ends it at the next motion.
//!
//! Callbacks run while the pointer's inner lock is held, so nothing reachable from an op may
//! call a `PointerHandle` method.
use aurora_layout::{
    Edges, Point, Rect, ResizeHandle, Side, Size, WinId, drop_side, edges_for_point, resize_rect,
};
use smithay::{
    input::pointer::{
        AxisFrame, ButtonEvent, Focus, GestureHoldBeginEvent, GestureHoldEndEvent,
        GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
        GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
        GrabStartData as StartData, MotionEvent, PointerGrab, PointerInnerHandle,
        RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point as SPoint, Serial},
};

use super::Anchor;
use crate::{Aurora, focus::FocusTarget};

/// Smallest content size a floating window can be dragged down to.
const MIN_CONTENT: i32 = 50;

/// One kind of drag. `motion` returns false to end the drag (window gone or mode changed).
pub trait Drag: Send + 'static {
    fn name(&self) -> &'static str;
    fn motion(&mut self, aurora: &mut Aurora, loc: SPoint<f64, Logical>) -> bool;
    /// The last button was released.
    fn release(&mut self, _aurora: &mut Aurora, _loc: SPoint<f64, Logical>) {}
    /// The grab is over, however it ended; a relayout follows.
    fn end(&mut self, _aurora: &mut Aurora) {}
}

pub struct Grab<T: Drag> {
    op: T,
    start: StartData<Aurora>,
    /// The drag ended on its own (window closed, mode changed). The grab stays until the
    /// button is released so the client never sees a release it saw no press for.
    ended: bool,
}

impl<T: Drag> Grab<T> {
    fn finish(&mut self, data: &mut Aurora) {
        if !std::mem::replace(&mut self.ended, true) {
            data.wm.drag = None;
            self.op.end(data);
            // The pointer lock is held here, so the relayout (which also logs the final
            // layout and resends pointer focus) waits for idle.
            data.handle.insert_idle(|a| a.relayout_all());
        }
    }
}

impl<T: Drag> PointerGrab<Aurora> for Grab<T> {
    fn motion(
        &mut self,
        data: &mut Aurora,
        handle: &mut PointerInnerHandle<'_, Aurora>,
        _focus: Option<(FocusTarget, SPoint<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // No client sees the pointer while it drags.
        handle.motion(data, None, event);
        if !self.ended && !self.op.motion(data, event.location) {
            self.finish(data);
        }
    }

    fn relative_motion(
        &mut self,
        _data: &mut Aurora,
        _handle: &mut PointerInnerHandle<'_, Aurora>,
        _focus: Option<(FocusTarget, SPoint<f64, Logical>)>,
        _event: &RelativeMotionEvent,
    ) {
    }

    fn button(
        &mut self,
        data: &mut Aurora,
        handle: &mut PointerInnerHandle<'_, Aurora>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            if !self.ended {
                self.op.release(data, handle.current_location());
            }
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(&mut self, _: &mut Aurora, _: &mut PointerInnerHandle<'_, Aurora>, _: AxisFrame) {}

    fn frame(&mut self, data: &mut Aurora, handle: &mut PointerInnerHandle<'_, Aurora>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        _: &mut Aurora,
        _: &mut PointerInnerHandle<'_, Aurora>,
        _: &GestureSwipeBeginEvent,
    ) {
    }

    fn gesture_swipe_update(
        &mut self,
        _: &mut Aurora,
        _: &mut PointerInnerHandle<'_, Aurora>,
        _: &GestureSwipeUpdateEvent,
    ) {
    }

    fn gesture_swipe_end(
        &mut self,
        _: &mut Aurora,
        _: &mut PointerInnerHandle<'_, Aurora>,
        _: &GestureSwipeEndEvent,
    ) {
    }

    fn gesture_pinch_begin(
        &mut self,
        _: &mut Aurora,
        _: &mut PointerInnerHandle<'_, Aurora>,
        _: &GesturePinchBeginEvent,
    ) {
    }

    fn gesture_pinch_update(
        &mut self,
        _: &mut Aurora,
        _: &mut PointerInnerHandle<'_, Aurora>,
        _: &GesturePinchUpdateEvent,
    ) {
    }

    fn gesture_pinch_end(
        &mut self,
        _: &mut Aurora,
        _: &mut PointerInnerHandle<'_, Aurora>,
        _: &GesturePinchEndEvent,
    ) {
    }

    fn gesture_hold_begin(
        &mut self,
        _: &mut Aurora,
        _: &mut PointerInnerHandle<'_, Aurora>,
        _: &GestureHoldBeginEvent,
    ) {
    }

    fn gesture_hold_end(
        &mut self,
        _: &mut Aurora,
        _: &mut PointerInnerHandle<'_, Aurora>,
        _: &GestureHoldEndEvent,
    ) {
    }

    fn start_data(&self) -> &StartData<Aurora> {
        &self.start
    }

    fn unset(&mut self, data: &mut Aurora) {
        self.finish(data);
    }
}

fn delta(from: (f64, f64), loc: SPoint<f64, Logical>) -> (i32, i32) {
    (
        (loc.x - from.0).round() as i32,
        (loc.y - from.1).round() as i32,
    )
}

pub struct FloatMoveGrab {
    id: WinId,
    rect: Rect,
    from: (f64, f64),
}

impl Drag for FloatMoveGrab {
    fn name(&self) -> &'static str {
        "move"
    }

    fn motion(&mut self, a: &mut Aurora, loc: SPoint<f64, Logical>) -> bool {
        let Some(mut ws) = a.wm.windows.get(&self.id).map(|w| w.ws) else {
            return false;
        };
        // Dropping on another output moves the window to the workspace shown there.
        if let Some(dst) = a.workspace_at(loc)
            && dst != ws
        {
            a.rehome_floating(self.id, dst);
            ws = dst;
        }
        let (dx, dy) = delta(self.from, loc);
        let rect = Rect::new(self.rect.x + dx, self.rect.y + dy, self.rect.w, self.rect.h);
        let moved =
            a.wm.workspaces
                .get_mut(&ws)
                .is_some_and(|w| w.set_floating_rect(self.id, rect));
        if moved {
            a.relayout_ws(ws);
        }
        moved
    }
}

pub struct FloatResizeGrab {
    id: WinId,
    rect: Rect,
    from: (f64, f64),
    edges: Edges,
}

impl Drag for FloatResizeGrab {
    fn name(&self) -> &'static str {
        "resize"
    }

    fn motion(&mut self, a: &mut Aurora, loc: SPoint<f64, Logical>) -> bool {
        let Some(win) = a.wm.windows.get(&self.id) else {
            return false;
        };
        let (ws, c) = (win.ws, win.constraints);
        let b = a.config.general.border_width.max(0);
        let min = Size {
            w: c.min.w.max(MIN_CONTENT) + 2 * b,
            h: c.min.h.max(MIN_CONTENT) + 2 * b,
        };
        let bound = |v: i32| if v > 0 { v + 2 * b } else { 0 };
        let max = Size {
            w: bound(c.max.w),
            h: bound(c.max.h),
        };
        let (dx, dy) = delta(self.from, loc);
        let rect = resize_rect(self.rect, self.edges, dx, dy, min, max);
        let resized =
            a.wm.workspaces
                .get_mut(&ws)
                .is_some_and(|w| w.set_floating_rect(self.id, rect));
        if resized {
            a.relayout_ws(ws);
        }
        resized
    }

    fn end(&mut self, a: &mut Aurora) {
        let Some(win) = a.wm.windows.get_mut(&self.id) else {
            return;
        };
        win.resize_anchor = None;
    }
}

pub struct TiledResizeGrab {
    id: WinId,
    ws: u32,
    handle: ResizeHandle,
    from: (f64, f64),
    /// The layout epoch after our own last change: anything else changing it (a window
    /// opening, a config reload) means the handle describes a stale tree.
    epoch: u64,
}

impl Drag for TiledResizeGrab {
    fn name(&self) -> &'static str {
        "tiled-resize"
    }

    fn motion(&mut self, a: &mut Aurora, loc: SPoint<f64, Logical>) -> bool {
        let (dx, dy) = delta(self.from, loc);
        let Some(workspace) = a.wm.workspaces.get_mut(&self.ws) else {
            return false;
        };
        if workspace.tiling.epoch() != self.epoch || !workspace.tiling.contains(self.id) {
            return false;
        }
        if workspace.tiling.resize_set(&self.handle, dx, dy) {
            a.relayout_ws(self.ws);
            if let Some(workspace) = a.wm.workspaces.get(&self.ws) {
                self.epoch = workspace.tiling.epoch();
            }
        }
        true
    }
}

/// Drag and drop: the window is reinserted beside the tiled window it is dropped on.
pub struct TiledMoveGrab {
    id: WinId,
    ws: u32,
}

impl Drag for TiledMoveGrab {
    fn name(&self) -> &'static str {
        "tiled-move"
    }

    fn motion(&mut self, a: &mut Aurora, _loc: SPoint<f64, Logical>) -> bool {
        a.wm.windows.contains_key(&self.id)
    }

    fn release(&mut self, a: &mut Aurora, loc: SPoint<f64, Logical>) {
        let Some(target) = a.space.element_under(loc).map(|(e, _)| e.id()) else {
            return;
        };
        let at = Point {
            x: loc.x as i32,
            y: loc.y as i32,
        };
        let Some(workspace) = a.wm.workspaces.get_mut(&self.ws) else {
            return;
        };
        let rect = workspace.tiling.rect(target);
        if target != self.id
            && let Some(rect) = rect
        {
            let side: Side = drop_side(rect, at);
            workspace.tiling.move_beside(self.id, target, side, None);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    Move,
    Resize,
}

impl Aurora {
    /// The workspace shown on the output under `loc`.
    fn workspace_at(&self, loc: SPoint<f64, Logical>) -> Option<u32> {
        let output = self.space.output_under(loc).next()?;
        self.wm.active_ws.get(output).copied()
    }

    /// Moves a floating window to workspace `dst` and makes that output the active one.
    fn rehome_floating(&mut self, id: WinId, dst: u32) {
        let Some(src) = self.wm.windows.get(&id).map(|w| w.ws) else {
            return;
        };
        self.relocate(id, dst);
        self.relayout_ws(src);
        if let Some(output) = self.wm.output_for_ws(dst) {
            self.wm.active_output = Some(output);
        }
    }

    /// Starts dragging `id` from the current pointer position, taking over the pointer.
    /// False (and nothing changes) when the window cannot be dragged that way.
    pub fn start_drag(
        &mut self,
        id: WinId,
        kind: DragKind,
        edges: Option<Edges>,
        button: u32,
        serial: Serial,
    ) -> bool {
        let Some(win) = self.wm.windows.get(&id) else {
            return false;
        };
        let ws = win.ws;
        let Some(workspace) = self.wm.workspaces.get(&ws) else {
            return false;
        };
        if workspace.fullscreen().is_some_and(|(f, _)| f == id) {
            return false;
        }
        let loc = self.pointer.current_location();
        let from = (loc.x, loc.y);
        let at = Point {
            x: loc.x as i32,
            y: loc.y as i32,
        };
        let start = StartData {
            focus: None,
            button,
            location: loc,
        };
        let border = self.config.general.border_width.max(0);
        let pointer = self.pointer.clone();

        if let Some(rect) = workspace.floating_rect(id) {
            match kind {
                DragKind::Move => {
                    let op = FloatMoveGrab { id, rect, from };
                    self.install(pointer, op, start, serial);
                }
                DragKind::Resize => {
                    let edges = edges.unwrap_or_else(|| edges_for_point(rect, at));
                    if let Some(win) = self.wm.windows.get_mut(&id) {
                        win.resize_anchor = Some(Anchor {
                            edges,
                            right: rect.right() - border,
                            bottom: rect.bottom() - border,
                        });
                    }
                    let op = FloatResizeGrab {
                        id,
                        rect,
                        from,
                        edges,
                    };
                    self.install(pointer, op, start, serial);
                }
            }
            return true;
        }

        let Some(rect) = workspace.tiling.rect(id) else {
            return false;
        };
        match kind {
            DragKind::Move => {
                self.install(pointer, TiledMoveGrab { id, ws }, start, serial);
                true
            }
            DragKind::Resize => {
                let edges = edges.unwrap_or_else(|| edges_for_point(rect, at));
                let Some(handle) = workspace.tiling.resize_start(id, edges) else {
                    return false;
                };
                let epoch = workspace.tiling.epoch();
                let op = TiledResizeGrab {
                    id,
                    ws,
                    handle,
                    from,
                    epoch,
                };
                self.install(pointer, op, start, serial);
                true
            }
        }
    }

    fn install<T: Drag>(
        &mut self,
        pointer: smithay::input::pointer::PointerHandle<Aurora>,
        op: T,
        start: StartData<Aurora>,
        serial: Serial,
    ) {
        let name = op.name();
        pointer.set_grab(
            self,
            Grab {
                op,
                start,
                ended: false,
            },
            serial,
            Focus::Clear,
        );
        self.wm.drag = Some(name);
    }
}

/// The edges a client asked to drag; `None` lets the pointer position decide.
pub fn xdg_edges(edge: xdg_toplevel::ResizeEdge) -> Option<Edges> {
    use xdg_toplevel::ResizeEdge as E;
    Some(match edge {
        E::Top => Edges::TOP,
        E::Bottom => Edges::BOTTOM,
        E::Left => Edges::LEFT,
        E::Right => Edges::RIGHT,
        E::TopLeft => Edges::TOP | Edges::LEFT,
        E::TopRight => Edges::TOP | Edges::RIGHT,
        E::BottomLeft => Edges::BOTTOM | Edges::LEFT,
        E::BottomRight => Edges::BOTTOM | Edges::RIGHT,
        _ => return None,
    })
}

impl Aurora {
    /// xdg `move`/`resize` request: honoured only for the serial of a pointer grab a button
    /// press started, which is the press the client is reacting to.
    pub fn xdg_drag(
        &mut self,
        surface: &WlSurface,
        kind: DragKind,
        edges: Option<Edges>,
        serial: Serial,
    ) {
        let pointer = self.pointer.clone();
        if !pointer.has_grab(serial) {
            return;
        }
        let (Some(start), Some(id)) = (pointer.grab_start_data(), self.wm.id_of(surface)) else {
            return;
        };
        self.start_drag(id, kind, edges, start.button, serial);
    }
}
