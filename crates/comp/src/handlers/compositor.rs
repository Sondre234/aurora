use crate::{Aurora, state::ClientState};
use std::sync::OnceLock;

use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    desktop::PopupKind,
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
            is_sync_subsurface,
        },
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
            outputs = self.outputs_for_unmapped(surface);
        }
        if outputs.is_empty() {
            // Not a window, cursor or popup we can place; repaint everything.
            self.queue_redraw_all();
        } else {
            for output in &outputs {
                self.queue_redraw_output(output);
            }
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
    fn outputs_for_unmapped(&self, surface: &WlSurface) -> Vec<Output> {
        let root = root_surface(surface);
        if let CursorImageStatus::Surface(cursor) = &self.cursor_status
            && cursor == &root
        {
            let pointer = self.pointer.current_location();
            return self
                .space
                .outputs()
                .filter(|o| {
                    self.space
                        .output_geometry(o)
                        .is_some_and(|g| g.to_f64().contains(pointer))
                })
                .cloned()
                .collect();
        }
        if let Some(parent) = self.popups.find_popup(&root).and_then(|popup| match popup {
            PopupKind::Xdg(xdg) => xdg.get_parent_surface(),
            _ => None,
        }) {
            let parent = root_surface(&parent);
            if let Some(window) = self.wm.window_of(&parent).map(|w| &w.element) {
                return self.space.outputs_for_element(window);
            }
        }
        Vec::new()
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
