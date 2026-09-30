//! Close ghosts: a window that goes away leaves a snapshot of its last frame that fades out
//! and shrinks. The snapshot is rendered into a texture while the window's surface still
//! holds its buffer (right before it is unmapped or destroyed), then drawn from that texture
//! only, so ghosts take no input and no frame callbacks. They live in the user data of the
//! output they were on.
use std::{cell::RefCell, time::Duration};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Offscreen, Renderer,
            damage::OutputDamageTracker,
            element::{Id, Kind, texture::TextureRenderElement},
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::space::SpaceElement,
    output::Output,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};

use super::{
    visual::{self, Visual},
    window::{WindowElement, WindowRenderElement},
};
use crate::anim::Curve;

pub struct Ghost {
    tex: GlesTexture,
    id: Id,
    /// Top-left of the snapshot in global logical coordinates, and its logical size.
    loc: Point<f64, Logical>,
    size: Size<i32, Logical>,
    start: Duration,
    dur: Duration,
    curve: Curve,
    alpha: f32,
    /// Stacking of the window it came from.
    pub z: u8,
}

impl Ghost {
    fn active(&self, now: Duration) -> bool {
        now < self.start + self.dur
    }

    fn progress(&self, now: Duration) -> f32 {
        if self.dur.is_zero() {
            return 1.0;
        }
        let t = now.saturating_sub(self.start).as_secs_f32() / self.dur.as_secs_f32();
        self.curve.value(t)
    }

    /// Logical top-left, logical size and opacity at `now`.
    fn state(&self, now: Duration) -> (Point<f64, Logical>, Size<i32, Logical>, f32) {
        let (scale, alpha) = visual::close_state(self.progress(now), self.alpha);
        let (w, h) = (
            (f64::from(self.size.w) * f64::from(scale)).round().max(1.0),
            (f64::from(self.size.h) * f64::from(scale)).round().max(1.0),
        );
        let centre = (
            self.loc.x + f64::from(self.size.w) / 2.0,
            self.loc.y + f64::from(self.size.h) / 2.0,
        );
        (
            Point::from((centre.0 - w / 2.0, centre.1 - h / 2.0)),
            Size::from((w as i32, h as i32)),
            alpha,
        )
    }
}

/// The ghosts of one output.
#[derive(Default)]
struct Ghosts(RefCell<Vec<Ghost>>);

fn store(output: &Output) -> &Ghosts {
    output.user_data().insert_if_missing(Ghosts::default);
    output
        .user_data()
        .get::<Ghosts>()
        .expect("inserted just above")
}

/// Drops the ghosts that finished and reports whether any is still fading.
pub fn prune(output: &Output, now: Duration) -> bool {
    let Some(ghosts) = output.user_data().get::<Ghosts>() else {
        return false;
    };
    let mut list = ghosts.0.borrow_mut();
    list.retain(|g| g.active(now));
    !list.is_empty()
}

pub fn clear(output: &Output) {
    if let Some(ghosts) = output.user_data().get::<Ghosts>() {
        ghosts.0.borrow_mut().clear();
    }
}

/// The ghosts of `output` as render elements, with their stacking, in no particular order.
/// `geo` is the output's rectangle in global coordinates.
pub fn elements(
    renderer: &GlesRenderer,
    output: &Output,
    geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
    now: Duration,
) -> Vec<(u8, TextureRenderElement<GlesTexture>)> {
    let Some(ghosts) = output.user_data().get::<Ghosts>() else {
        return Vec::new();
    };
    ghosts
        .0
        .borrow()
        .iter()
        .filter(|g| g.active(now))
        .map(|g| {
            let (loc, size, alpha) = g.state(now);
            let at: Point<f64, Physical> = (loc - geo.loc.to_f64()).to_physical(scale);
            let element = TextureRenderElement::from_static_texture(
                g.id.clone(),
                renderer.context_id(),
                at,
                g.tex.clone(),
                1,
                Transform::Normal,
                Some(alpha),
                None,
                Some(size),
                None,
                Kind::Unspecified,
            );
            (g.z, element)
        })
        .collect()
}

/// Renders `window` as it looks right now into a texture. Returns the texture with the
/// rectangle it covers, in coordinates relative to the surface origin.
fn snapshot(
    renderer: &mut GlesRenderer,
    window: &WindowElement,
    scale: f64,
) -> Option<(GlesTexture, Rectangle<i32, Logical>)> {
    let bbox = SpaceElement::bbox(window);
    if bbox.is_empty() {
        return None;
    }
    let scale = Scale::from(scale);
    let size: Size<i32, Physical> = bbox.size.to_physical_precise_ceil(scale);
    let elements: Vec<WindowRenderElement> = window.render_plain(
        renderer,
        Point::<i32, Logical>::from((-bbox.loc.x, -bbox.loc.y)).to_physical_precise_round(scale),
        scale,
        1.0,
    );
    if elements.is_empty() {
        return None;
    }
    let mut texture: GlesTexture = renderer
        .create_buffer(
            Fourcc::Abgr8888,
            Size::<i32, Buffer>::from((size.w, size.h)),
        )
        .ok()?;
    {
        let mut target = renderer.bind(&mut texture).ok()?;
        // Flipped180 is the vertical flip that stores the image top row first, which is how
        // a texture is sampled later.
        let mut tracker = OutputDamageTracker::new(size, scale, Transform::Flipped180);
        let result = tracker
            .render_output(renderer, &mut target, 0, &elements, [0.0; 4])
            .ok()?;
        let _ = result.sync.wait();
    }
    Some((texture, bbox))
}

/// Everything `leave` needs to know about the closing window.
pub struct Closing<'a> {
    pub window: &'a WindowElement,
    /// Where the window's content sits, globally, at its target.
    pub target: Rectangle<i32, Logical>,
    pub visual: Option<Visual>,
    pub z: u8,
    pub start: Duration,
    pub dur: Duration,
    pub curve: Curve,
}

/// Snapshots the window and leaves the ghost on `output`. Does nothing when the render
/// fails; the window then just vanishes.
pub fn leave(renderer: &mut GlesRenderer, output: &Output, closing: Closing) -> bool {
    let scale = output.current_scale().fractional_scale();
    let Some((tex, bbox)) = snapshot(renderer, closing.window, scale) else {
        return false;
    };
    let geo = SpaceElement::geometry(closing.window);
    // bbox is relative to the surface origin, target to the content corner (geometry origin).
    let loc = closing.target.loc.to_f64() + (bbox.loc - geo.loc).to_f64();
    let (shift, alpha) = match closing.visual.and_then(|v| v.transform().map(|t| (v, t))) {
        Some((v, ((dx, dy), _))) => ((f64::from(dx), f64::from(dy)), v.alpha),
        None => ((0.0, 0.0), 1.0),
    };
    let ghost = Ghost {
        tex,
        id: Id::new(),
        loc: Point::from((loc.x + shift.0, loc.y + shift.1)),
        size: bbox.size,
        start: closing.start,
        dur: closing.dur,
        curve: closing.curve,
        alpha,
        z: closing.z,
    };
    store(output).0.borrow_mut().push(ghost);
    true
}
