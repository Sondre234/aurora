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

use crate::focus::FocusTarget;

/// Stacking layers, between the Bottom (20) and Top (40) layer-shell layers.
pub const Z_TILED: u8 = 30;

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
        }
    }

    #[allow(dead_code)] // the tiling step sets the stacking layer
    pub fn set_z(&self, z: u8) {
        self.z.set(z);
    }

    /// Border width in logical pixels; `colors` is `[focused, unfocused]`.
    #[allow(dead_code)] // the tiling step sets these from the config
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
    window: Window,
    deco: Rc<Deco>,
}

impl WindowElement {
    pub fn new(window: Window) -> Self {
        Self {
            window,
            deco: Rc::new(Deco::new()),
        }
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
        self.window.is_in_input_region(point)
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

impl<R> AsRenderElements<R> for WindowElement
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    type RenderElement = WindowRenderElement<R>;

    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        let mut out: Vec<C> = self
            .window
            .render_elements::<WaylandSurfaceRenderElement<R>>(renderer, location, scale, alpha)
            .into_iter()
            .map(|e| C::from(WindowRenderElement::Surface(e)))
            .collect();

        let bw = self.border();
        if bw > 0 {
            let color = self.deco.color();
            let geo = SpaceElement::geometry(&self.window);
            let rects = Self::border_rects(geo, bw);
            let mut buffers = self.deco.borders.borrow_mut();
            for (buffer, rect) in buffers.iter_mut().zip(rects) {
                buffer.update(rect.size, color);
                let at = location + rect.loc.to_physical_precise_round(scale);
                out.push(C::from(WindowRenderElement::Border(
                    SolidColorRenderElement::from_buffer(
                        buffer,
                        at,
                        scale,
                        alpha,
                        Kind::Unspecified,
                    ),
                )));
            }
        }
        out
    }
}

/// The window whose toplevel or X11 surface is `surface`.
pub fn window_for_surface<'a>(
    space: &'a smithay::desktop::Space<WindowElement>,
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Option<&'a WindowElement> {
    space
        .elements()
        .find(|w| w.wl_surface().is_some_and(|s| s.as_ref() == surface))
}
