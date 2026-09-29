use crate::{Aurora, state::ClientState};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
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
};

use super::xdg_shell;

impl CompositorHandler for Aurora {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        crate::syncobj::install_blocker_hook(surface);
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.backend.early_import(surface);
        let mut outputs = Vec::new();
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().unwrap().wl_surface() == &root)
                .cloned();
            if let Some(window) = window {
                window.on_commit();
                outputs = self.space.outputs_for_element(&window);
            }
        };

        xdg_shell::handle_commit(&mut self.popups, &self.space, surface);

        // Popups, cursors and new windows are on no output yet; repaint everything then.
        if outputs.is_empty() {
            self.queue_redraw_all();
        } else {
            for output in &outputs {
                self.queue_redraw_output(output);
            }
        }
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
