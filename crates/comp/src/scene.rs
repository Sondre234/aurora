//! What an output draws. One builder feeds every backend so stacking is decided in one place.
use smithay::{
    backend::renderer::{
        element::{
            memory::MemoryRenderBufferRenderElement, render_elements,
            surface::WaylandSurfaceRenderElement,
        },
        gles::GlesRenderer,
    },
    desktop::{Space, space::SpaceRenderElements},
    output::Output,
};

use crate::wm::window::{WindowElement, WindowRenderElement};

render_elements! {
    /// Everything one output draws. The cursor is first so DrmCompositor can put it on the
    /// cursor plane.
    pub OutputElement<=GlesRenderer>;
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
    CursorSurface=WaylandSurfaceRenderElement<GlesRenderer>,
    Space=SpaceRenderElements<GlesRenderer, WindowRenderElement<GlesRenderer>>,
}

/// Windows and layers of `output`, front to back by z-index. `None` when the output is not
/// mapped in the space. Free function rather than an `Aurora` method: the DRM render loop
/// holds the backend borrowed while it builds the scene.
pub fn output_elements(
    space: &Space<WindowElement>,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Option<Vec<OutputElement>> {
    let elements = space
        .render_elements_for_output(renderer, output, 1.0)
        .ok()?;
    Some(elements.into_iter().map(OutputElement::from).collect())
}
