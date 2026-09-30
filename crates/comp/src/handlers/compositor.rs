use crate::{Aurora, state::ClientState};
use std::sync::OnceLock;

use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    desktop::{PopupKind, utils::surface_primary_scanout_output},
    input::pointer::CursorImageStatus,
    output::Output,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface, with_states,
        },
        fractional_scale::{FractionalScaleHandler, with_fractional_scale},
        seat::WaylandFocus,
        shm::{ShmHandler, ShmState},
    },
    xwayland::XWaylandClientData,
};

use super::xdg_shell;

impl CompositorHandler for Aurora {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(data) = client.get_data::<XWaylandClientData>() {
            return &data.compositor_state;
        }
        if let Some(data) = client.get_data::<ClientState>() {
            return &data.compositor_state;
        }
        // Unknown client data never happens by construction; a shared default keeps a
        // misregistered client from taking the compositor down.
        static FALLBACK: OnceLock<CompositorClientState> = OnceLock::new();
        FALLBACK.get_or_init(CompositorClientState::default)
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        crate::syncobj::install_blocker_hook(surface);
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.backend.early_import(surface);
        // A sync subsurface shows nothing until its parent commits.
        let sync_subsurface = is_sync_subsurface(surface);
        let mut outputs = Vec::new();
        // A window known to Wm but not in the Space (hidden workspace, not yet mapped) draws
        // nothing, so its commits repaint nothing.
        let mut offscreen = false;
        if !sync_subsurface {
            let root = root_surface(surface);
            let window = self.wm.window_of(&root).map(|w| w.element.clone());
            if let Some(window) = window {
                window.on_commit();
                outputs = self.space.outputs_for_element(&window);
                offscreen = self.space.element_location(&window).is_none();
                // The overview's thumbnail repaints on the commit only; it also shows windows
                // on hidden workspaces, which repaint no output of their own.
                if let Some(overview) = self.overview.as_mut() {
                    overview.mark_dirty(window.id());
                    self.queue_redraw_all();
                }
            } else if let Some(window) = self
                .xwayland
                .unmanaged
                .elements()
                .find(|w| w.wl_surface().as_deref() == Some(&root))
            {
                window.on_commit();
                outputs = self.xwayland.unmanaged.outputs_for_element(window);
            }
        };

        if let Some(output) = self.layer_commit(surface) {
            self.queue_redraw_output(&output);
            return;
        }
        if self.wm.id_of(surface).is_some() {
            self.toplevel_commit(surface);
        }
        xdg_shell::handle_commit(&mut self.popups, surface);

        if sync_subsurface || offscreen {
            return;
        }
        if outputs.is_empty() {
            match self.outputs_for_unmapped(surface) {
                Some(found) => outputs = found,
                // Not a window, cursor or popup we can place; repaint everything.
                None => return self.queue_redraw_all(),
            }
        }
        for output in &outputs {
            self.queue_redraw_output(output);
        }
    }
}

fn root_surface(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

impl Aurora {
    /// Outputs a commit from a surface outside the space can change: the pointer's for the
    /// cursor surface, the parent window's for a popup.
    fn outputs_for_unmapped(&self, surface: &WlSurface) -> Option<Vec<Output>> {
        let root = root_surface(surface);
        if let CursorImageStatus::Surface(cursor) = &self.cursor_status
            && cursor == &root
        {
            let pointer = self.pointer.current_location();
            return Some(
                self.space
                    .outputs()
                    .filter(|o| {
                        self.space
                            .output_geometry(o)
                            .is_some_and(|g| g.to_f64().contains(pointer))
                    })
                    .cloned()
                    .collect(),
            );
        }
        if self.popups.find_popup(&root).is_some() {
            // Walk up through nested popups to the window or layer that owns the menu. One
            // whose owner is not on any output has nothing to repaint.
            let mut owner = root;
            for _ in 0..16 {
                let Some(PopupKind::Xdg(xdg)) = self.popups.find_popup(&owner) else {
                    break;
                };
                let Some(parent) = xdg.get_parent_surface() else {
                    return Some(Vec::new());
                };
                owner = root_surface(&parent);
            }
            if let Some(window) = self.wm.window_of(&owner).map(|w| &w.element) {
                return Some(self.space.outputs_for_element(window));
            }
            return Some(self.layer_of(&owner).map(|(o, _)| o).into_iter().collect());
        }
        None
    }
}

impl BufferHandler for Aurora {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Aurora {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl FractionalScaleHandler for Aurora {
    /// Seeds the preferred scale before the surface has been presented anywhere: the output it
    /// was last presented on, else the one its window or layer sits on, else the primary one.
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let root = root_surface(&surface);
        let presented =
            |s: &WlSurface| with_states(s, |data| surface_primary_scanout_output(s, data));
        let output = presented(&surface)
            .or_else(|| presented(&root))
            .or_else(|| {
                let win = self.wm.window_of(&root)?;
                self.wm.output_for_ws(win.ws).or_else(|| {
                    self.space
                        .outputs_for_element(&win.element)
                        .first()
                        .cloned()
                })
            })
            .or_else(|| self.layer_of(&root).map(|(output, _)| output))
            .or_else(|| self.primary_output());
        let Some(output) = output else { return };
        let scale = output.current_scale().fractional_scale();
        with_states(&surface, |data| {
            with_fractional_scale(data, |fs| fs.set_preferred_scale(scale));
        });
    }
}
