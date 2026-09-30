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
        }
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
    pub WindowRenderElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Rounded=RoundedSurface,
    Border=SolidColorRenderElement,
    Ring=PixelShaderElement,
}

impl WindowElement {
    /// The shadow behind this window, `None` when effects are off, the window is fullscreen or
    /// the program did not compile. `at` is the surface origin, output-relative.
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
        let geo = self.output_geometry(at);
        cache.shadow(program, geo, d.rounding, d.shadow_radius, d.shadow_color.0)
    }

    /// Window geometry in output-relative logical pixels for a surface origin `at`.
    fn output_geometry(&self, at: Point<i32, Logical>) -> Rectangle<i32, Logical> {
        let geo = SpaceElement::geometry(&self.window);
        Rectangle::new(at + geo.loc, geo.size)
    }

    /// Elements of the window at surface origin `at` (output-relative), front to back: popups,
    /// the surface (rounded when `fx` is given and the window is not fullscreen), the border.
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
        let geo = self.output_geometry(at);
        let geo_phys: Rectangle<i32, Physical> = geo.to_physical_precise_round(scale);
        let radius_phys = (f64::from(rounding) * scale.x).round() as i32;
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
                    let offset = (SpaceElement::geometry(&self.window).loc + offset
                        - popup.geometry().loc)
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
                        location,
                        scale,
                        1.0,
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
                            renderer, location, scale, 1.0,
                        )
                        .into_iter()
                        .map(round),
                );
            }
        }

        let bw = self.border();
        if bw > 0 {
            let color = self.deco.color();
            let ring = programs
                .and_then(|p| p.border.as_ref())
                .filter(|_| rounding > 0)
                .and_then(|program| {
                    let aa = (1.0 / scale.x) as f32;
                    self.deco
                        .cache
                        .borrow_mut()
                        .border(program, geo, bw, rounding, color, aa)
                });
            if let Some(ring) = ring {
                out.push(WindowRenderElement::Ring(ring));
            } else {
                let rects = Self::border_rects(SpaceElement::geometry(&self.window), bw);
                let mut buffers = self.deco.borders.borrow_mut();
                for (buffer, rect) in buffers.iter_mut().zip(rects) {
                    buffer.update(rect.size, color);
                    let at = location + rect.loc.to_physical_precise_round(scale);
                    out.push(WindowRenderElement::Border(
                        SolidColorRenderElement::from_buffer(
                            buffer,
                            at,
                            scale,
                            1.0,
                            Kind::Unspecified,
                        ),
                    ));
                }
            }
        }
        out
    }
}
