//! [`Ui`]: one widget tree bound to one surface. It owns layout, hover/press/focus state,
//! keyboard editing, hit-testing and damage tracking.
//!
//! Frame protocol (what the Wayland runtime does, and what headless tests do by hand):
//!
//! 1. feed [`Input`] via [`Ui::handle`] and mutate via [`Ui::edit`] and friends;
//! 2. [`Ui::needs_redraw`] says whether anything changed; if not, do nothing at all;
//! 3. [`Ui::layout`] then [`Ui::take_damage`] give the rectangles that changed;
//! 4. [`Ui::paint`] repaints exactly those (clearing them first, so translucent surfaces
//!    stay correct).
//!
//! Damage is exact for what the toolkit knows: edited nodes, moved or resized nodes,
//! hover/press/focus changes, list scrolling and text edits. Everything else costs nothing.

use crate::damage::Damage;
use crate::geom::{Point, Rect, Size};
use crate::input::{Button, Id, Input, Key, KeyEvent, UiEvent};
use crate::painter::Painter;
use crate::text::TextSystem;
use crate::widget::{Cx, Kind, ListItem, Node, TextInput};

/// A widget tree plus the interaction state of one surface.
pub struct Ui {
    root: Node,
    text: TextSystem,
    size: Size,
    scale: f32,
    layout_dirty: bool,
    full_damage: bool,
    damage: Damage,
    hover: Id,
    pressed: Id,
    focus: Id,
}

impl Ui {
    /// `text` should be the process-wide [`TextSystem`] handle.
    pub fn new(text: TextSystem, root: Node) -> Self {
        Self {
            root,
            text,
            size: Size::default(),
            scale: 1.0,
            layout_dirty: true,
            full_damage: true,
            damage: Damage::new(),
            hover: Id::NONE,
            pressed: Id::NONE,
            focus: Id::NONE,
        }
    }

    pub fn text(&self) -> &TextSystem {
        &self.text
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Logical size of the surface. Changing it relayouts and damages everything.
    pub fn set_size(&mut self, size: Size) {
        if size != self.size {
            self.size = size;
            self.layout_dirty = true;
            self.full_damage = true;
        }
    }

    /// Device pixels per logical pixel (text is reshaped for the new scale).
    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > f32::EPSILON {
            self.scale = scale;
            self.layout_dirty = true;
            self.full_damage = true;
        }
    }

    /// Force a full repaint at the next frame (e.g. after the compositor lost a buffer).
    pub fn invalidate_all(&mut self) {
        self.full_damage = true;
    }

    /// Repaint a logical rect at the next frame without touching the tree (what a
    /// [`crate::widget::Canvas`] needs when its app state changed).
    pub fn damage(&mut self, r: Rect) {
        self.damage.add(r);
    }

    /// True when a frame would change pixels. An idle UI answers false forever.
    pub fn needs_redraw(&self) -> bool {
        self.layout_dirty || self.full_damage || !self.damage.is_empty()
    }

    // Tree access.

    /// Natural size of the whole tree for an offered width (`None` = unconstrained);
    /// use it to size an auto-height layer surface such as a toast.
    pub fn natural_size(&self, avail_w: Option<f32>) -> Size {
        let cx = Cx {
            text: &self.text,
            scale: self.scale,
        };
        self.root.measure(&cx, avail_w)
    }

    pub fn node(&self, id: Id) -> Option<&Node> {
        find(&self.root, id)
    }

    pub fn rect_of(&self, id: Id) -> Option<Rect> {
        self.node(id).map(|n| n.rect)
    }

    /// Mutate the whole tree (e.g. rebuild children); relayouts and repaints everything.
    pub fn root_mut(&mut self) -> &mut Node {
        self.layout_dirty = true;
        self.full_damage = true;
        &mut self.root
    }

    /// Mutate one node. Its old area is damaged now and its new area after relayout.
    /// Returns false when no node has that id.
    pub fn edit(&mut self, id: Id, f: impl FnOnce(&mut Node)) -> bool {
        let Some(n) = find_mut(&mut self.root, id) else {
            return false;
        };
        self.damage.add(n.damage_rect(n.rect));
        f(n);
        n.dirty = true;
        self.layout_dirty = true;
        true
    }

    pub fn set_visible(&mut self, id: Id, visible: bool) -> bool {
        self.edit(id, |n| n.style.visible = visible)
    }

    /// Replace the text of a label or input.
    pub fn set_text(&mut self, id: Id, text: &str) -> bool {
        self.edit(id, |n| n.set_text(text))
    }

    /// Current text of an input or label.
    pub fn text_of(&self, id: Id) -> Option<&str> {
        match &self.node(id)?.kind {
            Kind::Input(i) => Some(&i.text),
            Kind::Label(l) => Some(&l.text),
            _ => None,
        }
    }

    pub fn set_items(&mut self, id: Id, items: Vec<ListItem>) -> bool {
        self.edit(id, |n| {
            if let Kind::List(l) = &mut n.kind {
                l.items = items;
                l.icons.clear();
                l.scroll = 0.0;
                l.hover = None;
                if l.selected.is_some_and(|s| s >= l.items.len()) {
                    l.selected = None;
                }
            }
        })
    }

    pub fn list_selected(&self, id: Id) -> Option<usize> {
        match &self.node(id)?.kind {
            Kind::List(l) => l.selected,
            _ => None,
        }
    }

    pub fn list_item(&self, id: Id, index: usize) -> Option<&ListItem> {
        match &self.node(id)?.kind {
            Kind::List(l) => l.items.get(index),
            _ => None,
        }
    }

    pub fn list_select(&mut self, id: Id, index: Option<usize>) -> bool {
        self.edit(id, |n| {
            if let Kind::List(l) = &mut n.kind {
                l.selected = index.filter(|i| *i < l.items.len());
                if let Some(i) = l.selected {
                    l.ensure_visible(i, n.rect.h);
                }
            }
        })
    }

    /// Move the selection by `delta` rows (clamped) and scroll it into view. Returns the
    /// new selection; with no selection yet, the first move lands on the first or last row.
    pub fn list_move(&mut self, id: Id, delta: i32) -> Option<usize> {
        let cur = self.list_selected(id);
        let Kind::List(l) = &self.node(id)?.kind else {
            return None;
        };
        let count = l.items.len();
        if count == 0 {
            return None;
        }
        let next = match cur {
            Some(c) => (c as i64 + delta as i64).clamp(0, count as i64 - 1) as usize,
            None if delta < 0 => count - 1,
            None => 0,
        };
        self.list_select(id, Some(next));
        Some(next)
    }

    pub fn focus(&self) -> Option<Id> {
        (self.focus != Id::NONE).then_some(self.focus)
    }

    /// Give keyboard focus to an input or list (no event is emitted).
    pub fn set_focus(&mut self, id: Option<Id>) {
        let id = id.unwrap_or(Id::NONE);
        if id == self.focus {
            return;
        }
        let old = std::mem::replace(&mut self.focus, id);
        self.flag(old, |n| n.focused = false);
        self.flag(id, |n| n.focused = true);
    }

    // Frame.

    /// Lay the tree out if needed and collect the damage it produced.
    pub fn layout(&mut self) {
        if !self.layout_dirty {
            return;
        }
        self.layout_dirty = false;
        let cx = Cx {
            text: &self.text,
            scale: self.scale,
        };
        self.root
            .layout(&cx, Rect::new(0.0, 0.0, self.size.w, self.size.h));
        collect_damage(&mut self.root, &mut self.damage);
    }

    /// Damaged rectangles since the last call (whole surface after a resize,
    /// scale change or [`Ui::invalidate_all`]). Call [`Ui::layout`] first.
    pub fn take_damage(&mut self) -> Vec<Rect> {
        if std::mem::take(&mut self.full_damage) {
            self.damage
                .set_full(Rect::new(0.0, 0.0, self.size.w, self.size.h));
        }
        self.damage.take()
    }

    /// Repaint `regions` (logical pixels): each is cleared to transparent and the tree is
    /// drawn into it.
    pub fn paint(&self, p: &mut dyn Painter, regions: &[Rect]) {
        let cx = Cx {
            text: &self.text,
            scale: self.scale,
        };
        for r in regions {
            p.save();
            p.clip_rect(*r);
            p.clear_rect(*r);
            self.root.paint(p, &cx, r);
            p.restore();
        }
    }

    /// `layout` + `take_damage` + `paint` in one call; returns what was repainted.
    pub fn draw(&mut self, p: &mut dyn Painter) -> Vec<Rect> {
        self.layout();
        let d = self.take_damage();
        self.paint(p, &d);
        d
    }

    // Hit testing.

    /// Deepest interactive node (clickable, list or input, with an id) under `p`.
    pub fn hit_test(&self, p: Point) -> Option<Id> {
        hit(&self.root, p).map(|n| n.id)
    }

    // Input.

    /// Process one input event; returns the events it produced.
    pub fn handle(&mut self, input: Input) -> Vec<UiEvent> {
        let mut out = Vec::new();
        match input {
            Input::PointerMove(p) => self.pointer_move(p),
            Input::PointerLeave => {
                self.set_hover(Id::NONE, None);
            }
            Input::PointerDown(p, Button::Left) => self.pointer_down(p, &mut out),
            Input::PointerUp(p, Button::Left) => self.pointer_up(p, &mut out),
            Input::PointerDown(..) | Input::PointerUp(..) => {}
            Input::Scroll { pos, dy, .. } => self.scroll(pos, dy),
            Input::Key(k) => self.key(k, &mut out),
        }
        out
    }

    /// Run `f` on the node with `id` and damage its area (state-only changes: no relayout).
    fn flag(&mut self, id: Id, f: impl FnOnce(&mut Node)) {
        if id == Id::NONE {
            return;
        }
        if let Some(n) = find_mut(&mut self.root, id) {
            self.damage.add(n.damage_rect(n.rect));
            f(n);
        }
    }

    fn set_hover(&mut self, id: Id, row_at: Option<Point>) {
        if id != self.hover {
            let old = std::mem::replace(&mut self.hover, id);
            self.flag(old, |n| {
                n.hover = false;
                if let Kind::List(l) = &mut n.kind {
                    l.hover = None;
                }
            });
            self.flag(id, |n| n.hover = true);
        }
        // Track the hovered row of a list.
        if let Some(p) = row_at {
            let mut changed = false;
            if let Some(n) = find_mut(&mut self.root, id)
                && let Kind::List(l) = &mut n.kind
            {
                let row = l.row_at(p.y - n.rect.y);
                if row != l.hover {
                    l.hover = row;
                    changed = true;
                }
            }
            if changed {
                self.flag(id, |_| {});
            }
        }
    }

    fn pointer_move(&mut self, p: Point) {
        let target = self.hit_test(p).unwrap_or(Id::NONE);
        self.set_hover(target, Some(p));
        // Drag-select inside a pressed input.
        let id = self.pressed;
        if id != Id::NONE {
            let (scale, text) = (self.scale, self.text.clone());
            let mut dragged = false;
            self.flag(id, |n| {
                let rect = n.rect;
                let pad = n.style.padding;
                if let Kind::Input(i) = &mut n.kind {
                    let shaped = text.shape(&i.display(), &i.style, scale, None);
                    i.cursor = text_hit(i, &shaped, p.x - (rect.x + pad.left) + i.scroll_x);
                    dragged = true;
                }
            });
            // The caret may have left the visible part.
            self.layout_dirty |= dragged;
        }
    }

    fn pointer_down(&mut self, p: Point, out: &mut Vec<UiEvent>) {
        let target = self.hit_test(p).unwrap_or(Id::NONE);
        self.set_hover(target, Some(p));
        if target == Id::NONE {
            return;
        }
        self.pressed = target;
        let focusable = find(&self.root, target).is_some_and(|n| n.is_focusable());
        if focusable && self.focus != target {
            self.set_focus(Some(target));
            out.push(UiEvent::FocusChanged(Some(target)));
        }
        let scale = self.scale;
        let text = self.text.clone();
        let mut selected = None;
        self.flag(target, |n| {
            n.pressed = true;
            let rect = n.rect;
            let pad = n.style.padding;
            match &mut n.kind {
                Kind::Input(i) => {
                    let shaped = text.shape(&i.display(), &i.style, scale, None);
                    let idx = text_hit(i, &shaped, p.x - (rect.x + pad.left) + i.scroll_x);
                    i.anchor = Some(idx);
                    i.cursor = idx;
                }
                Kind::List(l) => {
                    if let Some(row) = l.row_at(p.y - rect.y)
                        && l.selected != Some(row)
                    {
                        l.selected = Some(row);
                        selected = Some(row);
                    }
                }
                _ => {}
            }
        });
        if let Some(index) = selected {
            out.push(UiEvent::Selected {
                list: target,
                index,
            });
        }
    }

    fn pointer_up(&mut self, p: Point, out: &mut Vec<UiEvent>) {
        let id = std::mem::replace(&mut self.pressed, Id::NONE);
        if id == Id::NONE {
            return;
        }
        let over = self.hit_test(p) == Some(id);
        let mut activated = None;
        let mut clicked = false;
        self.flag(id, |n| {
            n.pressed = false;
            match &mut n.kind {
                Kind::Input(i) => {
                    if i.anchor == Some(i.cursor) {
                        i.anchor = None;
                    }
                }
                Kind::List(l) => {
                    if over && let Some(row) = l.row_at(p.y - n.rect.y) {
                        activated = Some(row);
                    }
                }
                _ => {
                    clicked = over && n.clickable;
                }
            }
        });
        if clicked {
            out.push(UiEvent::Clicked(id));
        }
        if let Some(index) = activated {
            out.push(UiEvent::Activated { list: id, index });
        }
    }

    fn scroll(&mut self, pos: Point, dy: f32) {
        let Some(id) = self.hit_test(pos) else { return };
        let mut moved = false;
        if let Some(n) = find_mut(&mut self.root, id)
            && let Kind::List(l) = &mut n.kind
        {
            let before = l.scroll;
            l.scroll += dy;
            l.clamp_scroll(n.rect.h);
            l.hover = l.row_at(pos.y - n.rect.y);
            moved = l.scroll != before;
        }
        if moved {
            self.flag(id, |_| {});
        }
    }

    fn key(&mut self, k: KeyEvent, out: &mut Vec<UiEvent>) {
        let focus = self.focus;
        let kind = find(&self.root, focus).map(|n| match n.kind {
            Kind::Input(_) => 1,
            Kind::List(_) => 2,
            _ => 0,
        });
        match kind {
            Some(1) => {
                let mut outcome = Outcome::Ignored;
                if let Some(n) = find_mut(&mut self.root, focus)
                    && let Kind::Input(i) = &mut n.kind
                {
                    outcome = edit_input(i, &k);
                    if outcome != Outcome::Ignored {
                        self.damage.add(n.damage_rect(n.rect));
                    }
                }
                // The caret may have left the visible part.
                match outcome {
                    Outcome::Ignored => {}
                    Outcome::Moved => self.layout_dirty = true,
                    Outcome::Changed => {
                        self.layout_dirty = true;
                        out.push(UiEvent::TextChanged(focus));
                    }
                    Outcome::Submit => out.push(UiEvent::Submitted(focus)),
                }
                if outcome != Outcome::Ignored {
                    return;
                }
            }
            Some(2) => {
                let page = match find(&self.root, focus) {
                    Some(n) => match &n.kind {
                        Kind::List(l) => ((n.rect.h / l.row_height) as i32 - 1).max(1),
                        _ => 1,
                    },
                    None => 1,
                };
                let delta = match k.key {
                    Key::Up => Some(-1),
                    Key::Down => Some(1),
                    Key::PageUp => Some(-page),
                    Key::PageDown => Some(page),
                    Key::Home => Some(i32::MIN / 2),
                    Key::End => Some(i32::MAX / 2),
                    _ => None,
                };
                if let Some(d) = delta {
                    let before = self.list_selected(focus);
                    if let Some(index) = self.list_move(focus, d)
                        && before != Some(index)
                    {
                        out.push(UiEvent::Selected { list: focus, index });
                    }
                    return;
                }
                if k.key == Key::Enter {
                    if let Some(index) = self.list_selected(focus) {
                        out.push(UiEvent::Activated { list: focus, index });
                    }
                    return;
                }
            }
            _ => {}
        }
        if k.key == Key::Tab && !k.mods.ctrl && !k.mods.alt {
            if let Some(next) = self.focus_step(k.mods.shift) {
                self.set_focus(Some(next));
                out.push(UiEvent::FocusChanged(Some(next)));
            }
            return;
        }
        out.push(UiEvent::Key(k));
    }

    fn focus_step(&self, backwards: bool) -> Option<Id> {
        let mut ids = Vec::new();
        collect_focusable(&self.root, &mut ids);
        if ids.is_empty() {
            return None;
        }
        let pos = ids.iter().position(|i| *i == self.focus);
        let n = ids.len();
        Some(match (pos, backwards) {
            (Some(p), false) => ids[(p + 1) % n],
            (Some(p), true) => ids[(p + n - 1) % n],
            (None, false) => ids[0],
            (None, true) => ids[n - 1],
        })
    }
}

fn find(n: &Node, id: Id) -> Option<&Node> {
    if id == Id::NONE {
        return None;
    }
    if n.id == id {
        return Some(n);
    }
    n.children.iter().find_map(|c| find(c, id))
}

fn find_mut(n: &mut Node, id: Id) -> Option<&mut Node> {
    if id == Id::NONE {
        return None;
    }
    if n.id == id {
        return Some(n);
    }
    n.children.iter_mut().find_map(|c| find_mut(c, id))
}

fn collect_focusable(n: &Node, out: &mut Vec<Id>) {
    if !n.style.visible {
        return;
    }
    if n.is_focusable() {
        out.push(n.id);
    }
    n.children.iter().for_each(|c| collect_focusable(c, out));
}

fn interactive(n: &Node) -> bool {
    n.id != Id::NONE && (n.clickable || matches!(n.kind, Kind::List(_) | Kind::Input(_)))
}

fn hit(n: &Node, p: Point) -> Option<&Node> {
    if !n.style.visible || (n.style.clip && !n.rect.contains(p)) {
        return None;
    }
    if let Some(h) = n.children.iter().rev().find_map(|c| hit(c, p)) {
        return Some(h);
    }
    (interactive(n) && n.rect.contains(p)).then_some(n)
}

fn collect_damage(n: &mut Node, d: &mut Damage) {
    let now = n.damage_rect(n.rect);
    if n.dirty || n.prev != now {
        d.add(now);
        d.add(n.prev);
        n.prev = now;
        n.dirty = false;
    }
    n.children.iter_mut().for_each(|c| collect_damage(c, d));
}

// Text input editing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ignored,
    /// Caret or selection moved, text unchanged.
    Moved,
    Changed,
    Submit,
}

fn text_hit(i: &TextInput, shaped: &crate::text::ShapedText, x: f32) -> usize {
    i.text_index(shaped.hit(x)).min(i.text.len())
}

fn prev_char(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map_or(0, |(p, _)| p)
}

fn next_char(s: &str, i: usize) -> usize {
    s[i..].chars().next().map_or(s.len(), |c| i + c.len_utf8())
}

fn prev_word(s: &str, mut i: usize) -> usize {
    while i > 0 && s[..i].chars().next_back().is_some_and(char::is_whitespace) {
        i = prev_char(s, i);
    }
    while i > 0
        && s[..i]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace())
    {
        i = prev_char(s, i);
    }
    i
}

fn next_word(s: &str, mut i: usize) -> usize {
    while i < s.len() && s[i..].chars().next().is_some_and(|c| !c.is_whitespace()) {
        i = next_char(s, i);
    }
    while i < s.len() && s[i..].chars().next().is_some_and(char::is_whitespace) {
        i = next_char(s, i);
    }
    i
}

fn selection(i: &TextInput) -> Option<(usize, usize)> {
    i.anchor
        .filter(|a| *a != i.cursor)
        .map(|a| (a.min(i.cursor), a.max(i.cursor)))
}

fn delete_selection(i: &mut TextInput) -> bool {
    match selection(i) {
        Some((a, b)) => {
            i.text.replace_range(a..b, "");
            i.cursor = a;
            i.anchor = None;
            true
        }
        None => false,
    }
}

/// Apply a key to a text input.
fn edit_input(i: &mut TextInput, k: &KeyEvent) -> Outcome {
    let m = k.mods;
    let move_to = |i: &mut TextInput, to: usize| {
        if m.shift {
            i.anchor.get_or_insert(i.cursor);
        } else {
            i.anchor = None;
        }
        i.cursor = to;
        Outcome::Moved
    };
    match k.key {
        Key::Enter => Outcome::Submit,
        Key::Left => {
            if !m.shift
                && let Some((a, _)) = selection(i)
            {
                i.anchor = None;
                i.cursor = a;
                return Outcome::Moved;
            }
            let to = if m.ctrl {
                prev_word(&i.text, i.cursor)
            } else {
                prev_char(&i.text, i.cursor)
            };
            move_to(i, to)
        }
        Key::Right => {
            if !m.shift
                && let Some((_, b)) = selection(i)
            {
                i.anchor = None;
                i.cursor = b;
                return Outcome::Moved;
            }
            let to = if m.ctrl {
                next_word(&i.text, i.cursor)
            } else {
                next_char(&i.text, i.cursor)
            };
            move_to(i, to)
        }
        Key::Home => move_to(i, 0),
        Key::End => move_to(i, i.text.len()),
        Key::Backspace => {
            if delete_selection(i) {
                return Outcome::Changed;
            }
            if i.cursor == 0 {
                return Outcome::Moved;
            }
            let from = if m.ctrl {
                prev_word(&i.text, i.cursor)
            } else {
                prev_char(&i.text, i.cursor)
            };
            i.text.replace_range(from..i.cursor, "");
            i.cursor = from;
            Outcome::Changed
        }
        Key::Delete => {
            if delete_selection(i) {
                return Outcome::Changed;
            }
            if i.cursor >= i.text.len() {
                return Outcome::Moved;
            }
            let to = if m.ctrl {
                next_word(&i.text, i.cursor)
            } else {
                next_char(&i.text, i.cursor)
            };
            i.text.replace_range(i.cursor..to, "");
            Outcome::Changed
        }
        Key::Char(c) if m.ctrl && !m.alt => match c.to_ascii_lowercase() {
            'a' => {
                i.anchor = Some(0);
                i.cursor = i.text.len();
                Outcome::Moved
            }
            'u' => {
                i.text.replace_range(..i.cursor, "");
                i.cursor = 0;
                i.anchor = None;
                Outcome::Changed
            }
            'k' => {
                i.text.truncate(i.cursor);
                i.anchor = None;
                Outcome::Changed
            }
            'w' => {
                delete_selection(i);
                let from = prev_word(&i.text, i.cursor);
                i.text.replace_range(from..i.cursor, "");
                i.cursor = from;
                Outcome::Changed
            }
            _ => Outcome::Ignored,
        },
        _ => match &k.text {
            Some(t)
                if !m.ctrl
                    && !m.alt
                    && !m.logo
                    && !t.is_empty()
                    && t.chars().all(|c| !c.is_control()) =>
            {
                delete_selection(i);
                i.text.insert_str(i.cursor, t);
                i.cursor += t.len();
                i.anchor = None;
                Outcome::Changed
            }
            _ => Outcome::Ignored,
        },
    }
}
