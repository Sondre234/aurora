//! Drawing the overview: a dimmed backdrop, workspace panels, and live window thumbnails.
//!
//! Every window has one retained texture (`TextureRenderBuffer`) that is painted again only
//! when the window committed since (or the thumbnail size changed), so an idle overview costs
//! nothing and a busy window costs one offscreen render per frame at most. Rectangles are
//! solid-colour buffers pooled by slot so their damage tracking stays stable.
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use aurora_layout::{Rect, WinId};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Offscreen,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, Kind, render_elements,
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::WaylandSurfaceRenderElement,
                texture::{TextureRenderBuffer, TextureRenderElement},
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::{Window, layer_map_for_output, space::SpaceElement},
    output::Output,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};

use super::Overview;
use crate::{scene::OutputElement, wm::window::WindowElement};

render_elements! {
    pub OverviewElement<=GlesRenderer>;
    Solid=SolidColorRenderElement,
    Thumb=TextureRenderElement<GlesTexture>,
}

/// Thumbnails are painted at this fraction of the output scale: they are shown small, and
/// the retained textures are the overview's whole VRAM cost.
const THUMB_FACTOR: f64 = 0.5;
/// Longest thumbnail texture edge in pixels.
const MAX_EDGE: f64 = 2048.0;
const SELECT_WIDTH: i32 = 3;

type Rgba = [f32; 4];
const BACKDROP: Rgba = [0.03, 0.035, 0.05, 0.93];
const PANEL: Rgba = [0.10, 0.11, 0.14, 1.0];
const PANEL_SHOWN: Rgba = [0.14, 0.16, 0.22, 1.0];
const PANEL_HOVER: Rgba = [0.19, 0.21, 0.28, 1.0];
const PLACEHOLDER: Rgba = [0.22, 0.23, 0.28, 1.0];
const ACCENT: Rgba = [0.38, 0.62, 1.0, 1.0];

struct Thumb {
    buf: TextureRenderBuffer<GlesTexture>,
    px: Size<i32, Buffer>,
    dirty: bool,
}

/// GPU side of the overview: lives and dies with it.
#[derive(Default)]
pub struct RenderState {
    thumbs: Thumbs,
    solids: Vec<SolidColorBuffer>,
}

impl RenderState {
    pub fn mark_dirty(&mut self, id: WinId) {
        self.thumbs.mark_dirty(id);
    }

    pub fn retain(&mut self, alive: impl Fn(WinId) -> bool) {
        self.thumbs.retain(alive);
    }
}

/// The retained window textures.
#[derive(Default)]
struct Thumbs {
    map: HashMap<WinId, Thumb>,
    /// Windows whose last repaint failed; retried after their next commit.
    failed: HashSet<WinId>,
}

impl Thumbs {
    fn mark_dirty(&mut self, id: WinId) {
        self.failed.remove(&id);
        if let Some(thumb) = self.map.get_mut(&id) {
            thumb.dirty = true;
        }
    }

    fn retain(&mut self, alive: impl Fn(WinId) -> bool) {
        self.map.retain(|id, _| alive(*id));
        self.failed.retain(|id| alive(*id));
    }

    /// Brings the thumbnail of `id` up to date; false when there is nothing to show.
    fn refresh(
        &mut self,
        renderer: &mut GlesRenderer,
        id: WinId,
        element: &WindowElement,
        output_scale: f64,
    ) -> bool {
        let geo = SpaceElement::geometry(element);
        if geo.size.w <= 0 || geo.size.h <= 0 {
            return false;
        }
        let edge = f64::from(geo.size.w.max(geo.size.h));
        let scale = (output_scale * THUMB_FACTOR).min(MAX_EDGE / edge);
        let px = Size::<i32, Buffer>::from((
            ((f64::from(geo.size.w) * scale).ceil() as i32).max(1),
            ((f64::from(geo.size.h) * scale).ceil() as i32).max(1),
        ));
        let current = self.map.get(&id).map(|t| (t.px, t.dirty));
        if current == Some((px, false)) {
            return true;
        }
        if self.failed.contains(&id) {
            return false;
        }
        match self.paint(renderer, id, element, geo.loc, scale, px) {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!("overview: thumbnail of window {} failed: {err}", id.0);
                self.failed.insert(id);
                self.map.remove(&id);
                false
            }
        }
    }

    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        id: WinId,
        element: &WindowElement,
        origin: Point<i32, Logical>,
        scale: f64,
        px: Size<i32, Buffer>,
    ) -> Result<(), String> {
        if self.map.get(&id).is_none_or(|t| t.px != px) {
            let texture: GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, px)
                .map_err(|err| format!("cannot allocate: {err}"))?;
            match self.map.get_mut(&id) {
                Some(thumb) => {
                    thumb
                        .buf
                        .update_from_texture(renderer, texture, 1, Transform::Normal, None);
                    thumb.px = px;
                    thumb.dirty = true;
                }
                None => {
                    self.map.insert(
                        id,
                        Thumb {
                            buf: TextureRenderBuffer::from_texture(
                                renderer,
                                texture,
                                1,
                                Transform::Normal,
                                None,
                            ),
                            px,
                            dirty: true,
                        },
                    );
                }
            }
        }
        let Some(thumb) = self.map.get_mut(&id) else {
            return Ok(());
        };
        let scale = Scale::from(scale);
        // The window's geometry origin goes to the texture's corner.
        let at: Point<i32, Physical> =
            Point::<i32, Logical>::from((-origin.x, -origin.y)).to_physical_precise_round(scale);
        let window: &Window = element;
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            window.render_elements(renderer, at, scale, 1.0);
        thumb.buf.render().draw(|texture| {
            let size: Size<i32, Physical> = (px.w, px.h).into();
            let mut tracker = OutputDamageTracker::new(size, scale.x, Transform::Normal);
            let mut target = renderer
                .bind(texture)
                .map_err(|err| format!("cannot bind: {err}"))?;
            tracker
                .render_output(renderer, &mut target, 0, &elements, [0.0; 4])
                .map_err(|err| format!("{err:?}"))?;
            Ok::<_, String>(vec![Rectangle::from_size(px)])
        })?;
        thumb.dirty = false;
        Ok(())
    }
}

/// The part of `output` the overview uses, in global logical coordinates: what layer
/// surfaces (the bar) leave free.
pub fn area(output: &Output, geo: Rectangle<i32, Logical>) -> Rect {
    let zone = layer_map_for_output(output).non_exclusive_zone();
    if zone.is_empty() {
        Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h)
    } else {
        Rect::new(
            geo.loc.x + zone.loc.x,
            geo.loc.y + zone.loc.y,
            zone.size.w,
            zone.size.h,
        )
    }
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    let l = |a: i32, b: i32| a + ((b - a) as f32 * t).round() as i32;
    Rect::new(l(a.x, b.x), l(a.y, b.y), l(a.w, b.w), l(a.h, b.h))
}

/// Hands out pooled solid buffers as elements, one slot per rectangle of the frame.
struct Solids<'a> {
    pool: &'a mut Vec<SolidColorBuffer>,
    next: usize,
    scale: Scale<f64>,
    alpha: f32,
}

impl Solids<'_> {
    fn rect(&mut self, r: Rect, color: Rgba) -> OverviewElement {
        if self.next == self.pool.len() {
            self.pool.push(SolidColorBuffer::new((1, 1), color));
        }
        let buffer = &mut self.pool[self.next];
        self.next += 1;
        buffer.update((r.w.max(0), r.h.max(0)), color);
        let at: Point<i32, Physical> =
            Point::<i32, Logical>::from((r.x, r.y)).to_physical_precise_round(self.scale);
        SolidColorRenderElement::from_buffer(buffer, at, self.scale, self.alpha, Kind::Unspecified)
            .into()
    }
}

/// Pushes the overview of `output` (whose global rectangle is `geo`) onto `out`, in front of
/// whatever is pushed later. Does nothing when it is fully closed.
pub fn push(
    out: &mut Vec<OutputElement>,
    renderer: &mut GlesRenderer,
    overview: &Overview,
    output: &Output,
    geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
    now: Duration,
) {
    let alpha = overview.progress(now).clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return;
    }
    let name = output.name();
    let global = area(output, geo);
    let local = Rect::new(
        global.x - geo.loc.x,
        global.y - geo.loc.y,
        global.w,
        global.h,
    );
    let panels = overview.panels_for(&name, local);
    let shown = overview.shown_on(&name);

    let mut state = overview.render.borrow_mut();
    let state = &mut *state;
    let mut solids = Solids {
        pool: &mut state.solids,
        next: 0,
        scale,
        alpha,
    };
    let mut front: Vec<OverviewElement> = Vec::new();
    let mut thumbs: Vec<OverviewElement> = Vec::new();
    let mut back: Vec<OverviewElement> = Vec::new();

    for panel in &panels {
        let color = if overview.hover_panel == Some(panel.ws) {
            PANEL_HOVER
        } else if shown == Some(panel.ws) {
            PANEL_SHOWN
        } else {
            PANEL
        };
        back.push(solids.rect(panel.rect, color));
    }
    back.push(solids.rect(Rect::new(0, 0, geo.size.w, geo.size.h), BACKDROP));

    for panel in &panels {
        for &(id, tile) in &panel.tiles {
            let Some(entry) = overview.entry(id) else {
                continue;
            };
            let rect = match entry.from {
                Some(from) => {
                    let from = Rect::new(from.x - geo.loc.x, from.y - geo.loc.y, from.w, from.h);
                    lerp_rect(from, tile, alpha)
                }
                None => tile,
            };
            if rect.is_empty() {
                continue;
            }
            let ready = state.thumbs.refresh(renderer, id, &entry.element, scale.x);
            match state.thumbs.map.get(&id).filter(|_| ready) {
                Some(thumb) => {
                    let at = Point::<f64, Logical>::from((f64::from(rect.x), f64::from(rect.y)))
                        .to_physical(scale);
                    thumbs.push(
                        TextureRenderElement::from_texture_render_buffer(
                            at,
                            &thumb.buf,
                            Some(alpha),
                            None,
                            Some((rect.w, rect.h).into()),
                            Kind::Unspecified,
                        )
                        .into(),
                    );
                }
                None => thumbs.push(solids.rect(rect, PLACEHOLDER)),
            }
            if overview.selected == Some(id) {
                let b = SELECT_WIDTH;
                for edge in [
                    Rect::new(rect.x - b, rect.y - b, rect.w + 2 * b, b),
                    Rect::new(rect.x - b, rect.bottom(), rect.w + 2 * b, b),
                    Rect::new(rect.x - b, rect.y, b, rect.h),
                    Rect::new(rect.right(), rect.y, b, rect.h),
                ] {
                    front.push(solids.rect(edge, ACCENT));
                }
            }
        }
    }
    out.extend(
        front
            .into_iter()
            .chain(thumbs)
            .chain(back)
            .map(OutputElement::from),
    );
}
