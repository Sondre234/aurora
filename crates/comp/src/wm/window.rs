//! A mapped window as the scene sees it: the Smithay `Window` plus retained decoration
//! state (border quads, focus flag, stacking layer).
use std::{
    cell::{Cell, RefCell},
    ops::Deref,
    rc::Rc,
};

use smithay::{backend::renderer::element::AsRenderElements, wayland::seat::WaylandFocus};
use smithay::{
    backend::renderer::{
        ImportAll, Renderer,
        element::{
            Kind, render_elements,
            solid::{SolidColorBuffer, SolidColorRenderElement},
            surface::WaylandSurfaceRenderElement,
        },
    },
    desktop::{Window, WindowSurface, space::SpaceElement},
    output::Output,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale},
};

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

use aurora_layout::WinId;

use crate::focus::FocusTarget;

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
    pub WindowRenderElement<R> where R: ImportAll;
    Surface=WaylandSurfaceRenderElement<R>,
    Border=SolidColorRenderElement,
}

impl WindowElement {
    /// The window at its target, unanimated: surfaces first, then the four borders.
    /// `location` is where the surface origin goes.
    pub(crate) fn render_plain<R, C>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C>
    where
        R: Renderer + ImportAll,
        R::TextureId: Clone + 'static,
        C: From<WindowRenderElement<R>>,
    {
        let mut out: Vec<C> = self
            .window
            .render_elements::<WaylandSurfaceRenderElement<R>>(renderer, location, scale, alpha)
            .into_iter()
            .map(|e| C::from(WindowRenderElement::Surface(e)))
            .collect();
        let geo = SpaceElement::geometry(&self.window);
        self.push_borders(&mut out, geo, location, scale, alpha);
        out
    }

    /// Borders around `geo` (surface coordinates) for a surface origin at `location`.
    fn push_borders<R, C>(
        &self,
        out: &mut Vec<C>,
        geo: Rectangle<i32, Logical>,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) where
        R: Renderer + ImportAll,
        C: From<WindowRenderElement<R>>,
    {
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
            out.push(C::from(WindowRenderElement::Border(
                SolidColorRenderElement::from_buffer(buffer, at, scale, alpha, Kind::Unspecified),
            )));
        }
    }
}

impl<R> AsRenderElements<R> for WindowElement
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    type RenderElement = WindowRenderElement<R>;

    /// Draws the window where animation has it: the surface tree scaled and moved by
    /// `drawn - target` (the Space keeps the window at its target, so input is unaffected)
    /// and faded. A window at rest takes the untouched path.
    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        let Some(visual) = self.deco.visual() else {
            return self.render_plain(renderer, location, scale, alpha);
        };
        let alpha = alpha * visual.alpha;
        let moved = match visual.transform() {
            Some(t) if visual.drawn != visual.target => t,
            _ => return self.render_plain(renderer, location, scale, alpha),
        };
        let ((dx, dy), (sx, sy)) = moved;

        let geo = SpaceElement::geometry(&self.window);
        // Where the content's top-left corner is drawn: its place at the target, moved by
        // how far the animation has taken it.
        let shift: Point<f64, Logical> = (f64::from(dx), f64::from(dy)).into();
        let origin = location
            + geo.loc.to_physical_precise_round(scale)
            + shift.to_physical(scale).to_i32_round();
        // The surface tree scales about that corner: sizes by the factors, the origin of
        // the surface moves so the geometry corner stays put.
        let zoom = Scale::from((scale.x * f64::from(sx), scale.y * f64::from(sy)));
        let surface_at = origin - geo.loc.to_physical_precise_round(zoom);

        let mut out: Vec<C> = self
            .window
            .render_elements::<WaylandSurfaceRenderElement<R>>(renderer, surface_at, zoom, alpha)
            .into_iter()
            .map(|e| C::from(WindowRenderElement::Surface(e)))
            .collect();
        let content = Rectangle::new(
            (0, 0).into(),
            (
                visual.drawn.w.round().max(1.0) as i32,
                visual.drawn.h.round().max(1.0) as i32,
            )
                .into(),
        );
        self.push_borders(&mut out, content, origin, scale, alpha);
        out
    }
}
