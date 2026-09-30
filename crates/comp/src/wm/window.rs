//! A mapped window as the scene sees it: the Smithay `Window` plus retained decoration
//! state (border quads, focus flag, stacking layer).
use std::{
    cell::{Cell, RefCell},
    ops::Deref,
    rc::Rc,
};

use smithay::wayland::seat::WaylandFocus;
use smithay::{
    backend::renderer::{
        element::{
            AsRenderElements, Kind, render_elements,
            solid::{SolidColorBuffer, SolidColorRenderElement},
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
        gles::{GlesRenderer, element::PixelShaderElement},
    },
    desktop::{PopupManager, Window, WindowSurface, space::SpaceElement},
    output::Output,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale},
};

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

use aurora_layout::WinId;

use crate::{
    effects::{corners::RoundedSurface, shadow::DecoCache},
    focus::FocusTarget,
    scene::SceneFx,
};

use super::visual::Visual;

/// Stacking layers, between the Bottom (20) and Top (40) layer-shell layers.
pub const Z_TILED: u8 = 30;
pub const Z_FLOATING: u8 = 31;
pub const Z_FULLSCREEN: u8 = 50;

type Rgba = [f32; 4];

/// Retained decoration state. Buffers are touched only when size or colour changes, so an
/// unchanged border adds no damage.
pub struct Deco {
    /// Left, right, top, bottom.
    borders: RefCell<[SolidColorBuffer; 4]>,
    focused: Cell<bool>,
    z: Cell<u8>,
    width: Cell<i32>,
    colors: Cell<[Rgba; 2]>,
    /// Retained shadow and rounded border elements.
    cache: RefCell<DecoCache>,
    /// How animation draws the window this frame, `None` when it sits at its target.
    visual: Cell<Option<Visual>>,
    /// Drawn but not touchable: the workspace it is on is sliding out.
    inert: Cell<bool>,
}

impl Deco {
    fn new() -> Self {
        Self {
            borders: RefCell::new(std::array::from_fn(|_| {
                SolidColorBuffer::new((0, 0), [0.0; 4])
            })),
            focused: Cell::new(false),
            z: Cell::new(Z_TILED),
            width: Cell::new(0),
            colors: Cell::new([[0.0; 4]; 2]),
            cache: RefCell::new(DecoCache::default()),
            visual: Cell::new(None),
            inert: Cell::new(false),
        }
    }

    pub fn set_visual(&self, visual: Option<Visual>) {
        self.visual.set(visual);
    }

    pub fn visual(&self) -> Option<Visual> {
        self.visual.get()
    }

    pub fn set_inert(&self, inert: bool) {
        self.inert.set(inert);
    }

    pub fn set_z(&self, z: u8) {
        self.z.set(z);
    }

    /// Border width in logical pixels; `colors` is `[focused, unfocused]`.
    pub fn set_border(&self, width: i32, colors: [Rgba; 2]) {
        self.width.set(width.max(0));
        self.colors.set(colors);
    }

    fn color(&self) -> Rgba {
        let [focused, unfocused] = self.colors.get();
        if self.focused.get() {
            focused
        } else {
            unfocused
        }
    }
}

#[derive(Clone)]
pub struct WindowElement {
    id: WinId,
    window: Window,
    deco: Rc<Deco>,
}

impl WindowElement {
    pub fn new(id: WinId, window: Window) -> Self {
        Self {
            id,
            window,
            deco: Rc::new(Deco::new()),
        }
    }

    pub fn id(&self) -> WinId {
        self.id
    }

    pub fn deco(&self) -> &Deco {
        &self.deco
    }

    /// What keyboard focus goes to when this window is focused.
    pub fn focus_target(&self) -> Option<FocusTarget> {
        match self.window.underlying_surface() {
            WindowSurface::Wayland(toplevel) => {
                Some(FocusTarget::Wl(toplevel.wl_surface().clone()))
            }
            WindowSurface::X11(x11) => Some(FocusTarget::X11(x11.clone())),
        }
    }

    pub fn send_pending_configure(&self) {
        if let Some(toplevel) = self.window.toplevel() {
            toplevel.send_pending_configure();
        }
    }

    fn is_fullscreen(&self) -> bool {
        match self.window.underlying_surface() {
            WindowSurface::Wayland(toplevel) => toplevel.with_committed_state(|state| {
                state.is_some_and(|s| s.states.contains(xdg_toplevel::State::Fullscreen))
            }),
            WindowSurface::X11(x11) => x11.is_fullscreen(),
        }
    }

    /// Border width actually drawn.
    fn border(&self) -> i32 {
        if self.is_fullscreen() {
            0
        } else {
            self.deco.width.get()
        }
    }

    /// The four border rectangles around `geo`, in the same space as `geo`.
    fn border_rects(geo: Rectangle<i32, Logical>, bw: i32) -> [Rectangle<i32, Logical>; 4] {
        let (x, y, w, h) = (geo.loc.x, geo.loc.y, geo.size.w, geo.size.h);
        [
            Rectangle::new((x - bw, y - bw).into(), (bw, h + 2 * bw).into()),
            Rectangle::new((x + w, y - bw).into(), (bw, h + 2 * bw).into()),
            Rectangle::new((x, y - bw).into(), (w, bw).into()),
            Rectangle::new((x, y + h).into(), (w, bw).into()),
        ]
    }
}

impl Deref for WindowElement {
    type Target = Window;
    fn deref(&self) -> &Window {
        &self.window
    }
}

impl PartialEq for WindowElement {
    fn eq(&self, other: &Self) -> bool {
        self.window == other.window
    }
}

impl IsAlive for WindowElement {
    fn alive(&self) -> bool {
        self.window.alive()
    }
}

impl WaylandFocus for WindowElement {
    fn wl_surface(
        &self,
    ) -> Option<
        std::borrow::Cow<'_, smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
    > {
        self.window.wl_surface()
    }
}

impl SpaceElement for WindowElement {
    fn geometry(&self) -> Rectangle<i32, Logical> {
        SpaceElement::geometry(&self.window)
    }

    fn bbox(&self) -> Rectangle<i32, Logical> {
        let bbox = SpaceElement::bbox(&self.window);
        let bw = self.border();
        if bw == 0 {
            return bbox;
        }
        let mut geo = SpaceElement::geometry(&self.window);
        geo.loc -= (bw, bw).into();
        geo.size += (2 * bw, 2 * bw).into();
        bbox.merge(geo)
    }

    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        !self.deco.inert.get() && self.window.is_in_input_region(point)
    }

    fn z_index(&self) -> u8 {
        self.deco.z.get()
    }

    fn set_activate(&self, activated: bool) {
        self.deco.focused.set(activated);
        self.window.set_activate(activated);
    }

    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        self.window.output_enter(output, overlap)
    }

    fn output_leave(&self, output: &Output) {
        self.window.output_leave(output)
    }

    fn refresh(&self) {
        self.window.refresh()
    }
}

render_elements! {
    pub WindowRenderElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Rounded=RoundedSurface,
    Border=SolidColorRenderElement,
    Ring=PixelShaderElement,
}

impl WindowElement {
    /// How animation draws the window: `(shift, scale, alpha)` relative to its target, where
    /// the shift is in logical pixels. A window at rest (or one that cannot be scaled) is
    /// `((0, 0), (1, 1), 1)` apart from its fade.
    fn motion(&self) -> ((f64, f64), (f64, f64), f32) {
        let Some(visual) = self.deco.visual() else {
            return ((0.0, 0.0), (1.0, 1.0), 1.0);
        };
        match visual.transform() {
            Some(((dx, dy), (sx, sy))) => (
                (f64::from(dx), f64::from(dy)),
                (f64::from(sx), f64::from(sy)),
                visual.alpha,
            ),
            None => ((0.0, 0.0), (1.0, 1.0), visual.alpha),
        }
    }

    /// Window geometry where it is drawn, output-relative logical pixels, for a surface
    /// origin `at`, with the scale factor (for radii) and alpha of the animation.
    pub(crate) fn drawn_geometry(
        &self,
        at: Point<i32, Logical>,
    ) -> (Rectangle<i32, Logical>, f64, f32) {
        let geo = self.output_geometry(at);
        let ((dx, dy), (sx, sy), alpha) = self.motion();
        if (dx, dy, sx, sy) == (0.0, 0.0, 1.0, 1.0) {
            return (geo, 1.0, alpha);
        }
        let loc = geo.loc + Point::<i32, Logical>::from((dx.round() as i32, dy.round() as i32));
        let size = (
            ((f64::from(geo.size.w) * sx).round() as i32).max(1),
            ((f64::from(geo.size.h) * sy).round() as i32).max(1),
        );
        (Rectangle::new(loc, size.into()), sx.min(sy), alpha)
    }

    /// The shadow behind this window, `None` when effects are off, the window is fullscreen or
    /// the program did not compile. `at` is the surface origin, output-relative. It follows the
    /// drawn rectangle and fades with the window.
    pub fn shadow_element(
        &self,
        at: Point<i32, Logical>,
        fx: &SceneFx,
    ) -> Option<PixelShaderElement> {
        let d = fx.decoration;
        let program = fx.programs.as_ref().and_then(|p| p.shadow.as_ref());
        let mut cache = self.deco.cache.borrow_mut();
        let (Some(program), true, false) = (program, d.shadow, self.is_fullscreen()) else {
            cache.clear_shadow();
            return None;
        };
        let (geo, factor, alpha) = self.drawn_geometry(at);
        let rounding = (f64::from(d.rounding) * factor).round() as i32;
        let mut color = d.shadow_color.0;
        color[3] *= alpha;
        cache.shadow(program, geo, rounding, d.shadow_radius, color)
    }

    /// Window geometry in output-relative logical pixels for a surface origin `at`.
    fn output_geometry(&self, at: Point<i32, Logical>) -> Rectangle<i32, Logical> {
        let geo = SpaceElement::geometry(&self.window);
        Rectangle::new(at + geo.loc, geo.size)
    }

    /// The window at its target, unanimated and without effects: surfaces first, then the four
    /// borders. `location` is where the surface origin goes (used for ghost snapshots).
    pub(crate) fn render_plain(
        &self,
        renderer: &mut GlesRenderer,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<WindowRenderElement> {
        let mut out: Vec<WindowRenderElement> = self
            .window
            .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer, location, scale, alpha,
            )
            .into_iter()
            .map(WindowRenderElement::Surface)
            .collect();
        let geo = SpaceElement::geometry(&self.window);
        self.push_plain_borders(&mut out, geo, location, scale, alpha);
        out
    }

    /// Square borders around `geo` (surface coordinates) for a surface origin at `location`.
    fn push_plain_borders(
        &self,
        out: &mut Vec<WindowRenderElement>,
        geo: Rectangle<i32, Logical>,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) {
        let bw = self.border();
        if bw == 0 {
            return;
        }
        let color = self.deco.color();
        let rects = Self::border_rects(geo, bw);
        let mut buffers = self.deco.borders.borrow_mut();
        for (buffer, rect) in buffers.iter_mut().zip(rects) {
            buffer.update(rect.size, color);
            let at = location + rect.loc.to_physical_precise_round(scale);
            out.push(WindowRenderElement::Border(
                SolidColorRenderElement::from_buffer(buffer, at, scale, alpha, Kind::Unspecified),
            ));
        }
    }

    /// Elements of the window at surface origin `at` (output-relative), front to back: popups,
    /// the surface (rounded when `fx` is given and the window is not fullscreen), the border.
    ///
    /// While animating, the surface tree is moved and scaled by `drawn - target` and faded (the
    /// Space keeps the window at its target, so input is unaffected); the rounded clip, the
    /// border ring and the corner radius follow the drawn rectangle. Popups stay put.
    pub fn render_elements_fx(
        &self,
        renderer: &mut GlesRenderer,
        at: Point<i32, Logical>,
        scale: Scale<f64>,
        fx: Option<&SceneFx>,
    ) -> Vec<WindowRenderElement> {
        let location: Point<i32, Physical> = at.to_physical_precise_round(scale);
        let fx = fx.filter(|_| !self.is_fullscreen());
        let programs = fx.and_then(|f| f.programs.as_ref());
        let rounding = fx.map_or(0, |f| f.decoration.rounding);
        let geo_rel = SpaceElement::geometry(&self.window);
        let ((dx, dy), (sx, sy), alpha) = self.motion();
        let animated = (dx, dy, sx, sy) != (0.0, 0.0, 1.0, 1.0);
        let (drawn, factor, _) = self.drawn_geometry(at);

        // Where the surface origin goes and at what zoom. Geometry corner goes to the drawn
        // corner; the tree scales about it.
        let zoom = Scale::from((scale.x * sx, scale.y * sy));
        let surface_at: Point<i32, Physical> = if animated {
            drawn.loc.to_physical_precise_round(scale) - geo_rel.loc.to_physical_precise_round(zoom)
        } else {
            location
        };
        let geo_phys: Rectangle<i32, Physical> = if animated {
            Rectangle::new(
                drawn.loc.to_physical_precise_round(scale),
                drawn.size.to_physical_precise_round(scale),
            )
        } else {
            drawn.to_physical_precise_round(scale)
        };
        let radius_phys = (f64::from(rounding) * factor * scale.x).round() as i32;
        let corners = programs.and_then(|p| p.corners.clone());
        let round = |e: WaylandSurfaceRenderElement<GlesRenderer>| match &corners {
            Some(program) => match RoundedSurface::new(e, program.clone(), geo_phys, radius_phys) {
                Ok(rounded) => WindowRenderElement::Rounded(rounded),
                Err(plain) => WindowRenderElement::Surface(*plain),
            },
            None => WindowRenderElement::Surface(e),
        };

        let mut out: Vec<WindowRenderElement> = Vec::new();
        match self.window.underlying_surface() {
            WindowSurface::Wayland(toplevel) => {
                let surface = toplevel.wl_surface();
                for (popup, offset) in PopupManager::popups_for_surface(surface) {
                    let offset = (geo_rel.loc + offset - popup.geometry().loc)
                        .to_physical_precise_round(scale);
                    out.extend(
                        render_elements_from_surface_tree(
                            renderer,
                            popup.wl_surface(),
                            location + offset,
                            scale,
                            1.0,
                            Kind::Unspecified,
                        )
                        .into_iter()
                        .map(WindowRenderElement::Surface),
                    );
                }
                out.extend(
                    render_elements_from_surface_tree(
                        renderer,
                        surface,
                        surface_at,
                        zoom,
                        alpha,
                        Kind::Unspecified,
                    )
                    .into_iter()
                    .map(round),
                );
            }
            WindowSurface::X11(_) => {
                out.extend(
                    self.window
                        .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                            renderer, surface_at, zoom, alpha,
                        )
                        .into_iter()
                        .map(round),
                );
            }
        }

        let bw = self.border();
        if bw > 0 {
            let mut color = self.deco.color();
            color[3] *= alpha;
            let ring_rounding = (f64::from(rounding) * factor).round() as i32;
            let ring_bw = if animated {
                ((f64::from(bw) * factor).round() as i32).max(1)
            } else {
                bw
            };
            let ring = programs
                .and_then(|p| p.border.as_ref())
                .filter(|_| rounding > 0)
                .and_then(|program| {
                    let aa = (1.0 / scale.x) as f32;
                    self.deco.cache.borrow_mut().border(
                        program,
                        drawn,
                        ring_bw,
                        ring_rounding,
                        color,
                        aa,
                    )
                });
            if let Some(ring) = ring {
                out.push(WindowRenderElement::Ring(ring));
            } else if animated {
                // Square borders around the drawn content rectangle.
                let content = Rectangle::new((0, 0).into(), drawn.size);
                self.push_plain_borders(
                    &mut out,
                    content,
                    drawn.loc.to_physical_precise_round(scale),
                    scale,
                    alpha,
                );
            } else {
                self.push_plain_borders(&mut out, geo_rel, location, scale, alpha);
            }
        }
        out
    }
}
