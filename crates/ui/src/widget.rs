//! Retained widget tree: nodes, styling, layout and painting.
//!
//! A [`Node`] is a container (`row`, `column`, `stack`) or a leaf (`label`, `icon`,
//! `list`, `input`, `spacer`) plus a [`Style`] (size rules, padding, background, border,
//! shadow, states). Build a tree with the constructors and chained setters, hand it to a
//! [`crate::Ui`], and mutate it later through [`crate::Ui::edit`] so layout and damage
//! stay correct. There is no separate "button" widget: any node marked
//! [`Node::clickable`] reports [`crate::UiEvent::Clicked`] and uses its hover/press
//! backgrounds; [`Node::button`] builds the usual label-in-a-box.
//!
//! Layout is a plain two-pass flexbox subset: a node's [`Dim`] is `Auto` (content size),
//! `Px` (fixed) or `Fill(weight)` (shares the space left after fixed children). Rows and
//! columns place children along the main axis with `gap` and [`Justify`], and across it
//! with [`Align`]. A stack overlaps its children, `justify` aligning horizontally and
//! `align` vertically. Wrapping labels get the width their parent offers.

use crate::geom::{Color, Insets, Point, Rect, Size};
use crate::input::Id;
use crate::painter::{Image, Painter};
use crate::text::{TextStyle, TextSystem, TextWrap};

/// Size rule along one axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dim {
    /// Size of the content plus padding.
    Auto,
    /// Fixed logical pixels (including padding).
    Px(f32),
    /// Share of the space left in the parent's main axis, or the full cross axis.
    Fill(f32),
}

/// Cross-axis placement (vertical for a row, horizontal for a column, vertical for a stack).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
    /// Auto-sized children take the full cross size.
    Stretch,
}

/// Main-axis placement of the children that remain after `Fill` ones took their share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Justify {
    Start,
    Center,
    End,
    SpaceBetween,
}

/// Horizontal placement of text inside a label or input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub blur: f32,
    pub offset: Point,
    pub color: Color,
}

/// Visual and layout properties shared by every node.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    pub width: Dim,
    pub height: Dim,
    pub padding: Insets,
    pub gap: f32,
    pub align: Align,
    pub justify: Justify,
    pub bg: Option<Color>,
    pub hover_bg: Option<Color>,
    pub press_bg: Option<Color>,
    pub radius: f32,
    pub border: Option<(f32, Color)>,
    /// Border color while the node has keyboard focus (drawn instead of `border`).
    pub focus_border: Option<Color>,
    pub shadow: Option<Shadow>,
    /// Clip children (and own content) to the rect.
    pub clip: bool,
    pub visible: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            width: Dim::Auto,
            height: Dim::Auto,
            padding: Insets::default(),
            gap: 0.0,
            align: Align::Start,
            justify: Justify::Start,
            bg: None,
            hover_bg: None,
            press_bg: None,
            radius: 0.0,
            border: None,
            focus_border: None,
            shadow: None,
            clip: false,
            visible: true,
        }
    }
}

/// Static or editable text leaf.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub text: String,
    pub style: TextStyle,
    pub color: Color,
    pub align: TextAlign,
}

/// An image leaf.
#[derive(Clone)]
pub struct Icon {
    pub image: Option<Image>,
    pub size: Size,
    pub tint: Option<Color>,
}

/// One row of a [`List`].
#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub title: String,
    /// Dimmed text right-aligned in the row.
    pub subtitle: Option<String>,
    /// Caller-defined payload (an index into the app's own data, say).
    pub tag: u64,
}

impl ListItem {
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), subtitle: None, tag: 0 }
    }

    pub fn subtitle(mut self, s: impl Into<String>) -> Self {
        self.subtitle = Some(s.into());
        self
    }

    pub fn tag(mut self, t: u64) -> Self {
        self.tag = t;
        self
    }
}

/// Vertical list with a selection, hover row and wheel/keyboard scrolling. Rows have a
/// fixed height; only visible rows are painted.
#[derive(Clone)]
pub struct List {
    pub items: Vec<ListItem>,
    /// Icons parallel to `items`; missing or `None` entries draw no icon.
    pub icons: Vec<Option<Image>>,
    pub selected: Option<usize>,
    pub row_height: f32,
    /// Natural height is capped at this many rows (0 = all rows).
    pub max_rows: usize,
    pub text: TextStyle,
    pub subtitle_text: TextStyle,
    pub fg: Color,
    pub subtitle_fg: Color,
    pub selected_fg: Color,
    pub selected_bg: Color,
    pub hover_bg: Color,
    pub row_radius: f32,
    pub row_padding: f32,
    pub icon_size: f32,
    pub(crate) hover: Option<usize>,
    pub(crate) scroll: f32,
}

impl Default for List {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            icons: Vec::new(),
            selected: None,
            row_height: 32.0,
            max_rows: 0,
            text: TextStyle::default(),
            subtitle_text: TextStyle::sized(12.0),
            fg: Color::rgb(230, 230, 235),
            subtitle_fg: Color::rgb(140, 140, 150),
            selected_fg: Color::WHITE,
            selected_bg: Color::rgb(70, 90, 160),
            hover_bg: Color::rgba(255, 255, 255, 20),
            row_radius: 6.0,
            row_padding: 8.0,
            icon_size: 20.0,
            hover: None,
            scroll: 0.0,
        }
    }
}

impl List {
    fn content_height(&self) -> f32 {
        self.items.len() as f32 * self.row_height
    }

    fn max_scroll(&self, view_h: f32) -> f32 {
        (self.content_height() - view_h).max(0.0)
    }

    pub(crate) fn clamp_scroll(&mut self, view_h: f32) {
        self.scroll = self.scroll.clamp(0.0, self.max_scroll(view_h));
    }

    /// Scroll just enough to show row `i`.
    pub(crate) fn ensure_visible(&mut self, i: usize, view_h: f32) {
        let top = i as f32 * self.row_height;
        if top < self.scroll {
            self.scroll = top;
        } else if top + self.row_height > self.scroll + view_h {
            self.scroll = top + self.row_height - view_h;
        }
        self.clamp_scroll(view_h);
    }

    /// Row under a y offset relative to the list's top, if any.
    pub(crate) fn row_at(&self, y: f32) -> Option<usize> {
        if y < 0.0 {
            return None;
        }
        let i = ((y + self.scroll) / self.row_height) as usize;
        (i < self.items.len()).then_some(i)
    }

    /// Current scroll offset in logical pixels.
    pub fn scroll(&self) -> f32 {
        self.scroll
    }
}

/// Single-line text input with cursor and selection editing (no IME).
#[derive(Debug, Clone, PartialEq)]
pub struct TextInput {
    pub text: String,
    /// Caret as a byte index on a char boundary.
    pub cursor: usize,
    /// Other end of the selection, if any.
    pub anchor: Option<usize>,
    pub placeholder: String,
    /// Show bullets instead of the text.
    pub password: bool,
    pub style: TextStyle,
    pub color: Color,
    pub placeholder_color: Color,
    pub selection_color: Color,
    pub caret_color: Color,
    /// Natural width when the node's width is `Auto`.
    pub min_width: f32,
    pub(crate) scroll_x: f32,
}

impl Default for TextInput {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            anchor: None,
            placeholder: String::new(),
            password: false,
            style: TextStyle::default(),
            color: Color::rgb(235, 235, 240),
            placeholder_color: Color::rgb(130, 130, 140),
            selection_color: Color::rgba(90, 120, 220, 120),
            caret_color: Color::rgb(235, 235, 240),
            min_width: 160.0,
            scroll_x: 0.0,
        }
    }
}

/// What a node is.
#[derive(Clone)]
pub enum Kind {
    Row,
    Column,
    Stack,
    Spacer,
    Label(Label),
    Icon(Icon),
    List(Box<List>),
    Input(Box<TextInput>),
}

/// A widget tree node. Fields are public for reading; mutate a live tree through
/// [`crate::Ui::edit`].
#[derive(Clone)]
pub struct Node {
    pub id: Id,
    pub kind: Kind,
    pub style: Style,
    pub children: Vec<Node>,
    pub clickable: bool,
    pub(crate) rect: Rect,
    /// Damage rect at the last layout, to detect moves and resizes.
    pub(crate) prev: Rect,
    pub(crate) dirty: bool,
    pub(crate) hover: bool,
    pub(crate) pressed: bool,
    pub(crate) focused: bool,
}

/// Measurement context.
pub(crate) struct Cx<'a> {
    pub text: &'a TextSystem,
    pub scale: f32,
}

impl Node {
    fn new(kind: Kind, style: Style, children: Vec<Node>) -> Self {
        Self {
            id: Id::NONE,
            kind,
            style,
            children,
            clickable: false,
            rect: Rect::default(),
            prev: Rect::default(),
            dirty: true,
            hover: false,
            pressed: false,
            focused: false,
        }
    }

    pub fn row(children: Vec<Node>) -> Self {
        Self::new(Kind::Row, Style { align: Align::Center, ..Style::default() }, children)
    }

    pub fn column(children: Vec<Node>) -> Self {
        Self::new(Kind::Column, Style { align: Align::Stretch, ..Style::default() }, children)
    }

    pub fn stack(children: Vec<Node>) -> Self {
        Self::new(Kind::Stack, Style::default(), children)
    }

    /// Empty node; give it `Fill` to push siblings apart.
    pub fn spacer() -> Self {
        Self::new(Kind::Spacer, Style::default(), Vec::new())
    }

    pub fn label(text: impl Into<String>, style: TextStyle, color: Color) -> Self {
        let l = Label { text: text.into(), style, color, align: TextAlign::Left };
        Self::new(Kind::Label(l), Style::default(), Vec::new())
    }

    pub fn icon(image: Option<Image>, size: Size) -> Self {
        Self::new(Kind::Icon(Icon { image, size, tint: None }), Style::default(), Vec::new())
    }

    pub fn list(list: List) -> Self {
        Self::new(Kind::List(Box::new(list)), Style::default(), Vec::new())
    }

    pub fn input(input: TextInput) -> Self {
        let s = Style { padding: Insets::xy(8.0, 6.0), clip: true, ..Style::default() };
        Self::new(Kind::Input(Box::new(input)), s, Vec::new())
    }

    /// A clickable box with a centered label: padding 12x6, radius 6, hover/press
    /// backgrounds derived from `bg` by the caller via the chained setters.
    pub fn button(text: impl Into<String>, style: TextStyle, fg: Color) -> Self {
        let mut label = Self::label(text, style, fg);
        if let Kind::Label(l) = &mut label.kind {
            l.align = TextAlign::Center;
        }
        let mut n = Self::row(vec![label]);
        n.style.padding = Insets::xy(12.0, 6.0);
        n.style.radius = 6.0;
        n.style.justify = Justify::Center;
        n.clickable = true;
        n
    }

    // Chained setters.

    pub fn id(mut self, id: u32) -> Self {
        self.id = Id(id);
        self
    }

    pub fn padding(mut self, p: Insets) -> Self {
        self.style.padding = p;
        self
    }

    pub fn gap(mut self, g: f32) -> Self {
        self.style.gap = g;
        self
    }

    pub fn width(mut self, d: Dim) -> Self {
        self.style.width = d;
        self
    }

    pub fn height(mut self, d: Dim) -> Self {
        self.style.height = d;
        self
    }

    pub fn size(mut self, w: Dim, h: Dim) -> Self {
        self.style.width = w;
        self.style.height = h;
        self
    }

    pub fn align(mut self, a: Align) -> Self {
        self.style.align = a;
        self
    }

    pub fn justify(mut self, j: Justify) -> Self {
        self.style.justify = j;
        self
    }

    pub fn bg(mut self, c: Color) -> Self {
        self.style.bg = Some(c);
        self
    }

    pub fn hover_bg(mut self, c: Color) -> Self {
        self.style.hover_bg = Some(c);
        self
    }

    pub fn press_bg(mut self, c: Color) -> Self {
        self.style.press_bg = Some(c);
        self
    }

    pub fn radius(mut self, r: f32) -> Self {
        self.style.radius = r;
        self
    }

    pub fn border(mut self, width: f32, c: Color) -> Self {
        self.style.border = Some((width, c));
        self
    }

    pub fn focus_border(mut self, c: Color) -> Self {
        self.style.focus_border = Some(c);
        self
    }

    pub fn shadow(mut self, blur: f32, offset: Point, color: Color) -> Self {
        self.style.shadow = Some(Shadow { blur, offset, color });
        self
    }

    pub fn clip(mut self) -> Self {
        self.style.clip = true;
        self
    }

    pub fn clickable(mut self) -> Self {
        self.clickable = true;
        self
    }

    pub fn hidden(mut self) -> Self {
        self.style.visible = false;
        self
    }

    pub fn text_align(mut self, a: TextAlign) -> Self {
        if let Kind::Label(l) = &mut self.kind {
            l.align = a;
        }
        self
    }

    pub fn tint(mut self, c: Color) -> Self {
        if let Kind::Icon(i) = &mut self.kind {
            i.tint = Some(c);
        }
        self
    }

    /// Replace the text of a label or input (cursor goes to the end for inputs).
    pub fn set_text(&mut self, text: &str) {
        match &mut self.kind {
            Kind::Label(l) => l.text = text.to_string(),
            Kind::Input(i) => {
                i.text = text.to_string();
                i.cursor = i.text.len();
                i.anchor = None;
            }
            _ => {}
        }
    }

    /// Absolute laid-out rect (logical pixels); empty before the first layout.
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// Pointer is over this (interactive) node.
    pub fn is_hovered(&self) -> bool {
        self.hover
    }

    /// Primary button is held down on this node.
    pub fn is_pressed(&self) -> bool {
        self.pressed
    }

    /// Node has keyboard focus.
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub(crate) fn is_focusable(&self) -> bool {
        self.style.visible && matches!(self.kind, Kind::Input(_) | Kind::List(_)) && self.id != Id::NONE
    }

    /// Rect to repaint when this node changes: its box plus any shadow.
    pub(crate) fn damage_rect(&self, r: Rect) -> Rect {
        match self.style.shadow {
            Some(s) => {
                let e = s.blur + s.offset.x.abs().max(s.offset.y.abs());
                r.outset(e)
            }
            None => r,
        }
    }

    // Layout.

    pub(crate) fn measure(&self, cx: &Cx, avail_w: Option<f32>) -> Size {
        if !self.style.visible {
            return Size::default();
        }
        let pad = self.style.padding;
        let inner_w = match self.style.width {
            Dim::Px(w) => Some(w),
            _ => avail_w,
        }
        .map(|w| (w - pad.horizontal()).max(0.0));
        let content = match &self.kind {
            Kind::Spacer => Size::default(),
            Kind::Icon(i) => i.size,
            Kind::Label(l) => {
                let max = matches!(l.style.wrap, TextWrap::Word { .. }).then_some(inner_w).flatten();
                let s = cx.text.shape(&l.text, &l.style, cx.scale, max);
                Size::new(s.width(), s.height())
            }
            Kind::List(l) => {
                let rows = if l.max_rows > 0 { l.items.len().min(l.max_rows) } else { l.items.len() };
                Size::new(0.0, rows as f32 * l.row_height)
            }
            Kind::Input(i) => {
                let s = cx.text.shape("", &i.style, cx.scale, None);
                Size::new(i.min_width, s.height())
            }
            Kind::Row => {
                let ws = self.sizes(cx, true, None, inner_w);
                let h = self
                    .children
                    .iter()
                    .zip(&ws)
                    .filter(|(c, _)| c.style.visible)
                    .map(|(c, w)| c.measure(cx, Some(*w)).h)
                    .fold(0.0, f32::max);
                Size::new(ws.iter().sum::<f32>() + self.total_gap(), h)
            }
            Kind::Column => {
                let hs = self.sizes(cx, false, inner_w, None);
                let w = self.children.iter().filter(|c| c.style.visible).map(|c| c.measure(cx, inner_w).w).fold(0.0, f32::max);
                Size::new(w, hs.iter().sum::<f32>() + self.total_gap())
            }
            Kind::Stack => {
                let (mut w, mut h) = (0f32, 0f32);
                for c in self.children.iter().filter(|c| c.style.visible) {
                    let m = c.measure(cx, inner_w);
                    w = w.max(m.w);
                    h = h.max(m.h);
                }
                Size::new(w, h)
            }
        };
        let w = match self.style.width {
            Dim::Px(w) => w,
            _ => content.w + pad.horizontal(),
        };
        let h = match self.style.height {
            Dim::Px(h) => h,
            _ => content.h + pad.vertical(),
        };
        Size::new(w, h)
    }

    fn visible_children(&self) -> usize {
        self.children.iter().filter(|c| c.style.visible).count()
    }

    fn total_gap(&self) -> f32 {
        self.style.gap * self.visible_children().saturating_sub(1) as f32
    }

    /// Main-axis sizes of the children (0 for hidden ones). Fixed and auto children take
    /// their natural size; `Fill` children split what is left of `main_avail`.
    fn sizes(&self, cx: &Cx, horizontal: bool, cross_avail: Option<f32>, main_avail: Option<f32>) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.children.len());
        let mut fixed = self.total_gap();
        let mut weight = 0.0;
        for c in &self.children {
            if !c.style.visible {
                out.push(0.0);
                continue;
            }
            let dim = if horizontal { c.style.width } else { c.style.height };
            let m = if horizontal { c.measure(cx, None).w } else { c.measure(cx, cross_avail).h };
            match (dim, main_avail) {
                (Dim::Fill(w), Some(_)) => {
                    weight += w.max(0.0);
                    out.push(-w.max(0.0) - 1.0); // marker, resolved below
                }
                _ => {
                    fixed += m;
                    out.push(m);
                }
            }
        }
        if let Some(avail) = main_avail {
            let share = if weight > 0.0 { (avail - fixed).max(0.0) / weight } else { 0.0 };
            for s in out.iter_mut().filter(|s| **s < 0.0) {
                *s = (-*s - 1.0) * share;
            }
        }
        out
    }

    /// Assign `rect` to this node and lay out its children inside it.
    pub(crate) fn layout(&mut self, cx: &Cx, rect: Rect) {
        self.rect = rect;
        let pad = self.style.padding;
        let inner = rect.inset(pad);
        match self.kind {
            Kind::Row => self.layout_row(cx, inner),
            Kind::Column => self.layout_column(cx, inner),
            Kind::Stack => self.layout_stack(cx, inner),
            Kind::List(ref mut l) => l.clamp_scroll(rect.h),
            Kind::Input(_) => self.fix_input_scroll(cx, inner),
            _ => {}
        }
    }

    fn start_offset(&self, free: f32) -> (f32, f32) {
        let n = self.visible_children();
        match self.style.justify {
            Justify::Start => (0.0, 0.0),
            Justify::Center => (free.max(0.0) / 2.0, 0.0),
            Justify::End => (free.max(0.0), 0.0),
            Justify::SpaceBetween if n > 1 => (0.0, free.max(0.0) / (n - 1) as f32),
            Justify::SpaceBetween => (0.0, 0.0),
        }
    }

    fn cross_pos(align: Align, start: f32, avail: f32, size: f32) -> f32 {
        match align {
            Align::Start | Align::Stretch => start,
            Align::Center => start + (avail - size) / 2.0,
            Align::End => start + avail - size,
        }
    }

    fn layout_row(&mut self, cx: &Cx, inner: Rect) {
        let ws = self.sizes(cx, true, Some(inner.w), Some(inner.w));
        let used: f32 = ws.iter().sum::<f32>() + self.total_gap();
        let (mut x, extra) = self.start_offset(inner.w - used);
        x += inner.x;
        let (gap, align) = (self.style.gap, self.style.align);
        for (c, w) in self.children.iter_mut().zip(ws) {
            if !c.style.visible {
                continue;
            }
            let h = match c.style.height {
                Dim::Fill(_) => inner.h,
                Dim::Px(h) => h,
                Dim::Auto if align == Align::Stretch => inner.h,
                Dim::Auto => c.measure(cx, Some(w)).h,
            };
            let y = Self::cross_pos(align, inner.y, inner.h, h);
            c.layout(cx, Rect::new(x, y, w, h));
            x += w + gap + extra;
        }
    }

    fn layout_column(&mut self, cx: &Cx, inner: Rect) {
        let hs = self.sizes(cx, false, Some(inner.w), Some(inner.h));
        let used: f32 = hs.iter().sum::<f32>() + self.total_gap();
        let (mut y, extra) = self.start_offset(inner.h - used);
        y += inner.y;
        let (gap, align) = (self.style.gap, self.style.align);
        for (c, h) in self.children.iter_mut().zip(hs) {
            if !c.style.visible {
                continue;
            }
            let w = match c.style.width {
                Dim::Fill(_) => inner.w,
                Dim::Px(w) => w,
                Dim::Auto if align == Align::Stretch => inner.w,
                Dim::Auto => c.measure(cx, Some(inner.w)).w,
            };
            let x = Self::cross_pos(align, inner.x, inner.w, w);
            c.layout(cx, Rect::new(x, y, w, h));
            y += h + gap + extra;
        }
    }

    fn layout_stack(&mut self, cx: &Cx, inner: Rect) {
        let (justify, align) = (self.style.justify, self.style.align);
        for c in self.children.iter_mut().filter(|c| c.style.visible) {
            let m = c.measure(cx, Some(inner.w));
            let w = match c.style.width {
                Dim::Fill(_) => inner.w,
                _ => m.w,
            };
            let h = match c.style.height {
                Dim::Fill(_) => inner.h,
                _ => m.h,
            };
            let x = match justify {
                Justify::Start | Justify::SpaceBetween => inner.x,
                Justify::Center => inner.x + (inner.w - w) / 2.0,
                Justify::End => inner.x + inner.w - w,
            };
            let y = Self::cross_pos(align, inner.y, inner.h, h);
            c.layout(cx, Rect::new(x, y, w, h));
        }
    }

    /// Keep the caret inside the visible part of a text input.
    fn fix_input_scroll(&mut self, cx: &Cx, inner: Rect) {
        let Kind::Input(i) = &mut self.kind else { return };
        let shaped = cx.text.shape(&i.display(), &i.style, cx.scale, None);
        let caret = shaped.cursor_x(i.display_index(i.cursor));
        let room = (inner.w - 2.0).max(0.0);
        let mut s = i.scroll_x;
        if caret - s > room {
            s = caret - room;
        }
        if caret < s {
            s = caret;
        }
        i.scroll_x = if shaped.width() <= room { 0.0 } else { s.max(0.0) };
    }

    // Painting.

    /// Paint this subtree where it intersects `region`.
    pub(crate) fn paint(&self, p: &mut dyn Painter, cx: &Cx, region: &Rect) {
        if !self.style.visible || !self.damage_rect(self.rect).intersects(region) {
            return;
        }
        let r = self.rect;
        let s = &self.style;
        if let Some(sh) = s.shadow {
            p.shadow(r, s.radius, sh.blur, sh.offset, sh.color);
        }
        let bg = if self.pressed {
            s.press_bg.or(s.hover_bg).or(s.bg)
        } else if self.hover {
            s.hover_bg.or(s.bg)
        } else {
            s.bg
        };
        if let Some(c) = bg {
            p.fill_rounded_rect(r, s.radius, c);
        }
        if s.clip {
            p.save();
            p.clip_rect(r);
        }
        match &self.kind {
            Kind::Label(l) => self.paint_label(p, cx, l),
            Kind::Icon(i) => {
                if let Some(img) = &i.image {
                    let inner = r.inset(s.padding);
                    let dest = Rect::new(
                        inner.x + (inner.w - i.size.w) / 2.0,
                        inner.y + (inner.h - i.size.h) / 2.0,
                        i.size.w,
                        i.size.h,
                    );
                    p.draw_image(img, dest, i.tint);
                }
            }
            Kind::List(l) => self.paint_list(p, cx, l),
            Kind::Input(i) => self.paint_input(p, cx, i),
            _ => {}
        }
        for c in &self.children {
            c.paint(p, cx, region);
        }
        if s.clip {
            p.restore();
        }
        let border = if self.focused { s.focus_border.map(|c| (s.border.map_or(1.5, |b| b.0), c)).or(s.border) } else { s.border };
        if let Some((w, c)) = border {
            p.stroke_rounded_rect(r, s.radius, w, c);
        }
    }

    fn paint_label(&self, p: &mut dyn Painter, cx: &Cx, l: &Label) {
        let inner = self.rect.inset(self.style.padding);
        let natural = cx.text.shape(&l.text, &l.style, cx.scale, None);
        let wrap = matches!(l.style.wrap, TextWrap::Word { .. });
        let shaped = if wrap || natural.width() > inner.w + 0.5 {
            cx.text.shape(&l.text, &l.style, cx.scale, Some(inner.w))
        } else {
            natural
        };
        let x = match l.align {
            TextAlign::Left => inner.x,
            TextAlign::Center => inner.x + (inner.w - shaped.width()) / 2.0,
            TextAlign::Right => inner.x + inner.w - shaped.width(),
        };
        let y = inner.y + (inner.h - shaped.height()) / 2.0;
        p.draw_text(&shaped, Point::new(x, y), l.color);
    }

    fn paint_list(&self, p: &mut dyn Painter, cx: &Cx, l: &List) {
        let r = self.rect;
        p.save();
        p.clip_rect(r);
        let first = (l.scroll / l.row_height) as usize;
        let last = (((l.scroll + r.h) / l.row_height).ceil() as usize).min(l.items.len());
        for i in first..last {
            let row = Rect::new(r.x, r.y + i as f32 * l.row_height - l.scroll, r.w, l.row_height);
            let selected = l.selected == Some(i);
            if selected {
                p.fill_rounded_rect(row, l.row_radius, l.selected_bg);
            } else if l.hover == Some(i) {
                p.fill_rounded_rect(row, l.row_radius, l.hover_bg);
            }
            let item = &l.items[i];
            let mut x = row.x + l.row_padding;
            if let Some(Some(img)) = l.icons.get(i) {
                let s = l.icon_size;
                p.draw_image(img, Rect::new(x, row.y + (row.h - s) / 2.0, s, s), None);
                x += s + l.row_padding;
            }
            let mut right = row.right() - l.row_padding;
            if let Some(sub) = &item.subtitle {
                let shaped = cx.text.shape(sub, &l.subtitle_text, cx.scale, None);
                let w = shaped.width().min((right - x) * 0.5);
                let shaped = if shaped.width() > w { cx.text.shape(sub, &l.subtitle_text, cx.scale, Some(w)) } else { shaped };
                p.draw_text(&shaped, Point::new(right - shaped.width(), row.y + (row.h - shaped.height()) / 2.0), l.subtitle_fg);
                right -= shaped.width() + l.row_padding;
            }
            let natural = cx.text.shape(&item.title, &l.text, cx.scale, None);
            let avail = (right - x).max(0.0);
            let shaped = if natural.width() > avail { cx.text.shape(&item.title, &l.text, cx.scale, Some(avail)) } else { natural };
            let fg = if selected { l.selected_fg } else { l.fg };
            p.draw_text(&shaped, Point::new(x, row.y + (row.h - shaped.height()) / 2.0), fg);
        }
        p.restore();
    }

    fn paint_input(&self, p: &mut dyn Painter, cx: &Cx, i: &TextInput) {
        let inner = self.rect.inset(self.style.padding);
        p.save();
        p.clip_rect(inner.outset(1.0).intersect(&self.rect).unwrap_or(inner));
        if i.text.is_empty() {
            if !i.placeholder.is_empty() {
                let s = cx.text.shape(&i.placeholder, &i.style, cx.scale, None);
                p.draw_text(&s, Point::new(inner.x, inner.y + (inner.h - s.height()) / 2.0), i.placeholder_color);
            }
            let line = cx.text.shape("", &i.style, cx.scale, None);
            if self.focused {
                self.paint_caret(p, i, inner, 0.0, line.height());
            }
            p.restore();
            return;
        }
        let shaped = cx.text.shape(&i.display(), &i.style, cx.scale, None);
        let y = inner.y + (inner.h - shaped.height()) / 2.0;
        let x0 = inner.x - i.scroll_x;
        if let Some(a) = i.anchor.filter(|a| *a != i.cursor) {
            let (lo, hi) = (a.min(i.cursor), a.max(i.cursor));
            let (xa, xb) = (shaped.cursor_x(i.display_index(lo)), shaped.cursor_x(i.display_index(hi)));
            p.fill_rect(Rect::new(x0 + xa, y, xb - xa, shaped.height()), i.selection_color);
        }
        p.draw_text(&shaped, Point::new(x0, y), i.color);
        if self.focused {
            let cx_ = shaped.cursor_x(i.display_index(i.cursor));
            self.paint_caret(p, i, inner, cx_ - i.scroll_x, shaped.height());
        }
        p.restore();
    }

    fn paint_caret(&self, p: &mut dyn Painter, i: &TextInput, inner: Rect, x: f32, h: f32) {
        let y = inner.y + (inner.h - h) / 2.0;
        p.fill_rect(Rect::new(inner.x + x, y, 1.5, h), i.caret_color);
    }
}

impl TextInput {
    /// The text as displayed (bullets for passwords).
    pub(crate) fn display(&self) -> String {
        if self.password { "\u{2022}".repeat(self.text.chars().count()) } else { self.text.clone() }
    }

    /// Map a byte index in `text` to the matching index in [`TextInput::display`].
    pub(crate) fn display_index(&self, idx: usize) -> usize {
        if self.password { self.text[..idx.min(self.text.len())].chars().count() * '\u{2022}'.len_utf8() } else { idx }
    }

    /// Byte index in `text` for a byte index in the displayed string.
    pub(crate) fn text_index(&self, disp: usize) -> usize {
        if !self.password {
            return disp;
        }
        let n = disp / '\u{2022}'.len_utf8();
        self.text.char_indices().nth(n).map_or(self.text.len(), |(i, _)| i)
    }
}
