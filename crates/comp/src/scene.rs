//! What an output draws. One builder feeds every backend so stacking is decided in one place.
use std::{
    cmp::Reverse,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use smithay::{
    backend::renderer::{
        element::{
            AsRenderElements, Kind, Wrap,
            memory::MemoryRenderBufferRenderElement,
            render_elements,
            solid::SolidColorRenderElement,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            texture::TextureRenderElement,
        },
        gles::{GlesRenderer, GlesTexture, element::PixelShaderElement},
    },
    desktop::{Space, Window, space::SpaceElement},
    output::Output,
    reexports::wayland_server::Resource,
    utils::{Logical, Physical, Point, Rectangle, Scale, Size},
    wayland::shell::wlr_layer::Layer,
};

use crate::{
    config::Decoration,
    effects::{
        self, Programs,
        blur::{self, BlurElement, Owner},
        shadow,
    },
    layers::layers_front_to_back,
    wm::{
        ghost,
        window::{WindowElement, WindowRenderElement},
    },
};

render_elements! {
    /// Everything one output draws. The cursor is first so DrmCompositor can put it on the
    /// cursor plane. Effect streams add one variant each here (`Shadow`, `Blur`, `Overview`,
    /// `Ghost`), with the element type defined in their own module, and push it from the
    /// matching hook in `output_elements`.
    pub OutputElement<=GlesRenderer>;
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
    CursorSurface=WaylandSurfaceRenderElement<GlesRenderer>,
    Window=WindowRenderElement,
    Layer=Wrap<WaylandSurfaceRenderElement<GlesRenderer>>,
    Shadow=PixelShaderElement,
    Blur=BlurElement,
    Overview=crate::overview::OverviewElement,
    Ghost=TextureRenderElement<GlesTexture>,
}

/// Per-frame inputs of the effects, built once per render and passed down to the builder.
/// Shadows, corners, blur and animated elements read their settings here instead of reaching
/// into the config or the renderer.
#[allow(dead_code)] // read by the M3 step 2 streams
pub struct SceneFx<'a> {
    pub decoration: &'a Decoration,
    /// Compiled shader programs; `None` only before `effects::init` ran.
    pub programs: Option<Programs>,
    /// Frame time on the animation clock.
    pub now: Duration,
    /// The live overview, when one is open or closing.
    pub overview: Option<&'a crate::overview::Overview>,
}

impl<'a> SceneFx<'a> {
    pub fn new(renderer: &GlesRenderer, decoration: &'a Decoration, now: Duration) -> Self {
        Self {
            decoration,
            programs: effects::programs(renderer),
            now,
            overview: None,
        }
    }

    pub fn with_overview(mut self, overview: Option<&'a crate::overview::Overview>) -> Self {
        self.overview = overview;
        self
    }

    /// Whether effects may draw on `output`: not over a fullscreen window, which must stay
    /// eligible for direct scanout.
    #[allow(dead_code)]
    pub fn enabled_on(&self, output: &Output) -> bool {
        !top_hidden(output)
    }
}

/// Retained per output: set by the layer code when a fullscreen window covers the output, so
/// the render loop needs nothing from `Wm`.
#[derive(Default)]
struct TopHidden(AtomicBool);

/// Returns whether the value changed.
pub fn set_top_hidden(output: &Output, hidden: bool) -> bool {
    let flag = output
        .user_data()
        .get_or_insert_threadsafe(TopHidden::default);
    flag.0.swap(hidden, Ordering::Relaxed) != hidden
}

pub fn top_hidden(output: &Output) -> bool {
    output
        .user_data()
        .get::<TopHidden>()
        .is_some_and(|f| f.0.load(Ordering::Relaxed))
}

/// What a locked `output` draws, front to back: its lock surface, then opaque black. With no
/// lock surface (client dead, output new) only the black.
fn lock_elements(
    renderer: &mut GlesRenderer,
    output: &Output,
    geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> Vec<OutputElement> {
    let (surface, black) = crate::lock::view(output, geo.size);
    let origin: Point<i32, Physical> = Point::from((0, 0));
    let mut out = Vec::new();
    if let Some(surface) = surface {
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                renderer,
                &surface,
                origin,
                scale,
                1.0,
                Kind::Unspecified,
            );
        out.extend(
            elements
                .into_iter()
                .map(|e| OutputElement::Layer(Wrap::from(e))),
        );
    }
    out.push(OutputElement::Window(WindowRenderElement::Border(
        SolidColorRenderElement::from_buffer(&black, origin, scale, 1.0, Kind::Unspecified),
    )));
    out
}

fn push_layers(
    out: &mut Vec<OutputElement>,
    renderer: &mut GlesRenderer,
    output: &Output,
    kind: Layer,
    scale: Scale<f64>,
    mut blur: Option<&mut Vec<blur::Request>>,
) {
    for (layer, at) in layers_front_to_back(output, kind) {
        let start = out.len();
        let elements: Vec<Wrap<WaylandSurfaceRenderElement<GlesRenderer>>> =
            layer.render_elements(renderer, at.to_physical_precise_round(scale), scale, 1.0);
        out.extend(elements.into_iter().map(OutputElement::Layer));
        if let Some(blur) = blur.as_deref_mut() {
            let geo = layer.geometry();
            let rect = Rectangle::new(at + geo.loc, geo.size);
            let owner = Owner::Layer(layer.wl_surface().id());
            blur.extend(blur::want(out, start, owner, rect, scale));
        }
    }
}

/// Layers and windows of `output`, front to back: Overlay, Top (not over a fullscreen
/// window), unmanaged X11 windows, windows by z-index, Bottom, Background.
/// `None` when the output is not mapped in the space. Free function rather than an `Aurora`
/// method: the DRM render loop holds the backend borrowed while it builds the scene.
pub fn output_elements(
    space: &Space<WindowElement>,
    unmanaged: &Space<Window>,
    renderer: &mut GlesRenderer,
    output: &Output,
    fx: &SceneFx,
) -> Option<Vec<OutputElement>> {
    let geo = space.output_geometry(output)?;
    let scale = Scale::from(output.current_scale().fractional_scale());
    // A locked session shows the output's lock surface over black and nothing else: no
    // layer, window, blur or overview element is even built.
    if crate::lock::engaged() {
        return Some(lock_elements(renderer, output, geo, scale));
    }
    let mut out = Vec::new();
    // Blur requests, front to back; `blur::apply` inserts the elements once the list is done.
    let blurring = blur::active(fx, output);
    let mut requests = Vec::new();
    if !blurring {
        blur::release(renderer, output);
    }

    push_layers(
        &mut out,
        renderer,
        output,
        Layer::Overlay,
        scale,
        blurring.then_some(&mut requests),
    );
    if !top_hidden(output) {
        push_layers(
            &mut out,
            renderer,
            output,
            Layer::Top,
            scale,
            blurring.then_some(&mut requests),
        );
    }

    // The overview covers the windows and everything below them; Overlay and Top layers
    // (launcher, bar) stay on top of it.
    if let Some(overview) = fx.overview {
        crate::overview::push(&mut out, renderer, overview, output, geo, scale, fx.now);
    }

    // Menus and tooltips belong to no workspace: they stay above every window.
    for window in unmanaged.elements().rev() {
        let (Some(bbox), Some(loc)) = (
            unmanaged.element_bbox(window),
            unmanaged.element_location(window),
        ) else {
            continue;
        };
        if !geo.overlaps(bbox) {
            continue;
        }
        let at: Point<i32, Logical> = loc - SpaceElement::geometry(window).loc - geo.loc;
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            window.render_elements(renderer, at.to_physical_precise_round(scale), scale, 1.0);
        out.extend(
            elements
                .into_iter()
                .map(|e| OutputElement::Window(WindowRenderElement::Surface(e))),
        );
    }

    // The space stacks bottom to top; the stable sort keeps that order within a z-index.
    let fx_on = fx.enabled_on(output).then_some(fx);
    let shadow_grow = fx_on
        .filter(|f| f.decoration.shadow)
        .map_or(0, |f| shadow::shadow_reach(f.decoration.shadow_radius));
    let mut windows: Vec<&WindowElement> = space.elements().rev().collect();
    windows.sort_by_key(|w| Reverse(SpaceElement::z_index(*w)));
    // Closing windows fade out at the stacking of the window they were: each one goes in
    // front of the first window that is not above it.
    let mut ghosts = ghost::elements(renderer, output, geo, scale, fx.now);
    ghosts.sort_by_key(|(z, _)| Reverse(*z));
    let mut ghosts = ghosts.into_iter().peekable();
    for window in windows {
        let z = SpaceElement::z_index(window);
        while let Some((_, element)) = ghosts.next_if(|(gz, _)| *gz >= z) {
            out.push(OutputElement::Ghost(element));
        }
        let (Some(bbox), Some(loc)) = (space.element_bbox(window), space.element_location(window))
        else {
            continue;
        };
        let reach = Rectangle::new(
            bbox.loc - Point::from((shadow_grow, shadow_grow)),
            bbox.size + Size::from((2 * shadow_grow, 2 * shadow_grow)),
        );
        if !geo.overlaps(reach) {
            continue;
        }
        let at: Point<i32, Logical> = loc - SpaceElement::geometry(window).loc - geo.loc;
        // Hooks, in front-to-back order for this window: (1) the window's own elements below
        // (the corner program and `current - target` offset/scale apply there), (2) its
        // shadow, (3) blur of what is behind it. Streams push their variants here.
        let start = out.len();
        out.extend(
            window
                .render_elements_fx(renderer, at, scale, fx_on)
                .into_iter()
                .map(OutputElement::Window),
        );
        if blurring {
            let rect = window.drawn_geometry(at).0;
            requests.extend(blur::want(
                &out,
                start,
                Owner::Win(window.id()),
                rect,
                scale,
            ));
        }
        if let Some(shadow) = fx_on.and_then(|fx| window.shadow_element(at, fx)) {
            out.push(OutputElement::Shadow(shadow));
        }
    }

    out.extend(ghosts.map(|(_, element)| OutputElement::Ghost(element)));

    push_layers(&mut out, renderer, output, Layer::Bottom, scale, None);
    push_layers(&mut out, renderer, output, Layer::Background, scale, None);
    if blurring {
        blur::apply(renderer, output, fx, &mut out, &requests);
    }
    Some(out)
}
