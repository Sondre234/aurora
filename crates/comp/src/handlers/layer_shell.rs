use smithay::{
    desktop::{LayerSurface, layer_map_for_output},
    output::Output,
    reexports::wayland_server::protocol::wl_output::WlOutput,
    wayland::shell::{
        wlr_layer::{
            Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState,
        },
        xdg::PopupSurface,
    },
};

use crate::Aurora;

impl WlrLayerShellHandler for Aurora {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.protocols.layer_shell
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        layer: Layer,
        namespace: String,
    ) {
        let output = output
            .as_ref()
            .and_then(Output::from_resource)
            .filter(|o| self.wm.outputs.contains(o))
            .or_else(|| self.wm.active_output.clone())
            .or_else(|| self.wm.outputs.first().cloned());
        let Some(output) = output else {
            tracing::warn!(
                ns = namespace,
                "layer surface with no output to go on, closing"
            );
            surface.send_close();
            return;
        };
        let mapped = layer_map_for_output(&output)
            .map_layer(&LayerSurface::new(surface.clone(), namespace.clone()));
        if let Err(err) = mapped {
            tracing::warn!(%err, ns = namespace, "layer surface could not be mapped, closing");
            surface.send_close();
            return;
        }
        tracing::info!(
            "layer: new out={} ns={namespace:?} layer={layer:?}",
            output.name()
        );
        self.refresh_layer_focus();
    }

    fn new_popup(&mut self, parent: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_layer_popup(&popup, parent.wl_surface());
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        self.layer_removed(&surface);
    }
}
