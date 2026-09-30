//! What an output draws. One builder feeds every backend so stacking is decided in one place.
use std::{
    cmp::Reverse,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use smithay::{
    backend::renderer::{
        element::{
            AsRenderElements, Wrap, memory::MemoryRenderBufferRenderElement, render_elements,
            surface::WaylandSurfaceRenderElement,
        },
        gles::GlesRenderer,
    },
    desktop::{Space, Window, space::SpaceElement},
    output::Output,
    utils::{Logical, Point, Scale},
    wayland::shell::wlr_layer::Layer,
};

use crate::{
    config::Decoration,
    effects::{self, Programs},
    layers::layers_front_to_back,
    wm::window::{WindowElement, WindowRenderElement},
};

render_elements! {
    /// Everything one output draws. The cursor is first so DrmCompositor can put it on the
    /// cursor plane. Effect streams add one variant each here (`Shadow`, `Blur`, `Overview`,
    /// `Ghost`), with the element type defined in their own module, and push it from the
    /// matching hook in `output_elements`.
    pub OutputElement<=GlesRenderer>;
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
    CursorSurface=WaylandSurfaceRenderElement<GlesRenderer>,
    Window=WindowRenderElement<GlesRenderer>,
    Layer=Wrap<WaylandSurfaceRenderElement<GlesRenderer>>,
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
}

impl<'a> SceneFx<'a> {
    pub fn new(renderer: &GlesRenderer, decoration: &'a Decoration, now: Duration) -> Self {
        Self {
            decoration,
            programs: effects::programs(renderer),
            now,
        }
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

fn push_layers(
    out: &mut Vec<OutputElement>,
    renderer: &mut GlesRenderer,
    output: &Output,
    kind: Layer,
    scale: Scale<f64>,
) {
    for (layer, at) in layers_front_to_back(output, kind) {
        let elements: Vec<Wrap<WaylandSurfaceRenderElement<GlesRenderer>>> =
            layer.render_elements(renderer, at.to_physical_precise_round(scale), scale, 1.0);
        out.extend(elements.into_iter().map(OutputElement::Layer));
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
    _fx: &SceneFx,
) -> Option<Vec<OutputElement>> {
    let geo = space.output_geometry(output)?;
    let scale = Scale::from(output.current_scale().fractional_scale());
    let mut out = Vec::new();

    push_layers(&mut out, renderer, output, Layer::Overlay, scale);
    if !top_hidden(output) {
        push_layers(&mut out, renderer, output, Layer::Top, scale);
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
    let mut windows: Vec<&WindowElement> = space.elements().rev().collect();
    windows.sort_by_key(|w| Reverse(SpaceElement::z_index(*w)));
    for window in windows {
        let (Some(bbox), Some(loc)) = (space.element_bbox(window), space.element_location(window))
        else {
            continue;
        };
        if !geo.overlaps(bbox) {
            continue;
        }
        let at: Point<i32, Logical> = loc - SpaceElement::geometry(window).loc - geo.loc;
        // Hooks, in front-to-back order for this window: (1) the window's own elements below
        // (the corner program and `current - target` offset/scale apply there), (2) its
        // shadow, (3) blur of what is behind it. Streams push their variants here.
        out.extend(window.render_elements::<OutputElement>(
            renderer,
            at.to_physical_precise_round(scale),
            scale,
            1.0,
        ));
    }

    push_layers(&mut out, renderer, output, Layer::Bottom, scale);
    push_layers(&mut out, renderer, output, Layer::Background, scale);
    Some(out)
}
