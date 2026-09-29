//! X11 windows on top of the same registry as xdg toplevels. Managed windows enter `Wm` at
//! their map request and go through rules, layout and focus like any other; override-redirect
//! windows (menus, tooltips) live in the unmanaged space and are never tiled.
use aurora_layout::{Constraints, Edges, FsMode, Size, WinId};
use smithay::{
    desktop::Window,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Rectangle, SERIAL_COUNTER},
    xwayland::{
        X11Surface,
        xwm::{ResizeEdge, WmWindowType},
    },
};

use super::{
    Phase, WinData,
    apply::{has_buffer, rect_of},
    grabs::DragKind,
    window::WindowElement,
};
use crate::{Aurora, focus::FocusTarget};

fn constraints(x11: &X11Surface) -> Constraints {
    let size = |s: Option<smithay::utils::Size<i32, Logical>>| {
        s.map_or(Size::default(), |s| Size {
            w: s.w.max(0),
            h: s.h.max(0),
        })
    };
    Constraints {
        min: size(x11.min_size()),
        max: size(x11.max_size()),
    }
    .clamped()
}

/// Everything but a plain top-level window is a helper the user does not want in the tree.
fn floats_by_type(x11: &X11Surface) -> bool {
    x11.window_type().is_some_and(|t| t != WmWindowType::Normal) || x11.is_modal()
}

fn edges(edge: ResizeEdge) -> Edges {
    match edge {
        ResizeEdge::Top => Edges::TOP,
        ResizeEdge::Bottom => Edges::BOTTOM,
        ResizeEdge::Left => Edges::LEFT,
        ResizeEdge::Right => Edges::RIGHT,
        ResizeEdge::TopLeft => Edges::TOP | Edges::LEFT,
        ResizeEdge::TopRight => Edges::TOP | Edges::RIGHT,
        ResizeEdge::BottomLeft => Edges::BOTTOM | Edges::LEFT,
        ResizeEdge::BottomRight => Edges::BOTTOM | Edges::RIGHT,
    }
}

impl Aurora {
    pub fn x11_id(&self, x11: &X11Surface) -> Option<WinId> {
        self.wm.by_x11.get(&x11.window_id()).copied()
    }

    /// A managed window wants to be shown: register it and place it in the layout.
    pub fn x11_map_request(&mut self, x11: X11Surface) {
        tracing::info!(
            "x11: map request xid={} class={:?} title={:?}",
            x11.window_id(),
            x11.class(),
            x11.title()
        );
        if let Err(err) = x11.set_mapped(true) {
            return tracing::warn!("x11: cannot map the window: {err}");
        }
        if self.x11_id(&x11).is_some() {
            return;
        }
        let id = self.wm.alloc_id();
        let element = WindowElement::new(id, Window::new_x11_window(x11.clone()));
        let parent = x11
            .is_transient_for()
            .and_then(|p| self.wm.by_x11.get(&p).copied());
        let mut win = WinData::new(id, element);
        win.app_id = x11.class();
        win.constraints = constraints(&x11);
        win.parent = parent;
        win.want_mode = if x11.is_fullscreen() {
            Some(FsMode::Fullscreen)
        } else {
            x11.is_maximized().then_some(FsMode::Maximized)
        };
        self.wm.windows.insert(id, win);
        self.wm.by_x11.insert(x11.window_id(), id);
        if let Some(surface) = x11.wl_surface() {
            self.wm.by_surface.insert(surface.id(), id);
        }
        let float = x11.is_transient_for().is_some() || floats_by_type(&x11);
        // Its own size, set by a configure request before the map, is the default.
        let asked = x11.geometry().size;
        let size = (asked.w > 1 && asked.h > 1).then_some((asked.w, asked.h));
        self.place(id, float, &x11.title(), size);
    }

    /// Xwayland paired the window with its wl_surface, possibly after the first buffer.
    pub fn x11_surface_associated(&mut self, surface: &WlSurface, x11: &X11Surface) {
        let Some(id) = self.x11_id(x11) else {
            return;
        };
        self.wm.by_surface.insert(surface.id(), id);
        if has_buffer(surface) {
            self.toplevel_commit(surface);
        }
    }

    pub fn x11_map_override_redirect(&mut self, x11: X11Surface) {
        let geo = x11.last_configure();
        let window = Window::new_x11_window(x11.clone());
        self.xwayland.unmanaged.map_element(window, geo.loc, true);
        self.queue_redraw_all();
        // Full-output override-redirect windows are how some games go fullscreen without
        // asking the window manager: they get the keyboard.
        let covers = self
            .wm
            .outputs
            .iter()
            .any(|o| self.space.output_geometry(o) == Some(geo));
        if covers {
            tracing::info!("x11: override-redirect window covers an output, focusing it");
            let serial = SERIAL_COUNTER.next_serial();
            let keyboard = self.keyboard.clone();
            keyboard.set_focus(self, Some(FocusTarget::X11(x11)), serial);
        }
    }

    /// The window was unmapped or destroyed (both end its life here; a remap comes back as
    /// a new map request).
    pub fn x11_gone(&mut self, x11: &X11Surface) {
        if let Some(id) = self.x11_id(x11) {
            self.wm_remove_window(id);
        }
        let unmanaged = self
            .xwayland
            .unmanaged
            .elements()
            .find(|w| w.x11_surface() == Some(x11))
            .cloned();
        if let Some(window) = unmanaged {
            self.xwayland.unmanaged.unmap_elem(&window);
            self.queue_redraw_all();
            // It may have held the keyboard; hand it back to the focused window.
            if self.keyboard.current_focus() == Some(FocusTarget::X11(x11.clone())) {
                self.focus_window(self.wm.focused, false);
            }
        }
    }

    /// Everything X11 disappears with the server.
    pub fn x11_forget_all(&mut self) {
        let ids: Vec<WinId> = self
            .wm
            .windows
            .values()
            .filter(|w| w.element.x11_surface().is_some())
            .map(|w| w.id)
            .collect();
        for id in ids {
            self.wm_remove_window(id);
        }
        let windows: Vec<Window> = self.xwayland.unmanaged.elements().cloned().collect();
        for window in windows {
            self.xwayland.unmanaged.unmap_elem(&window);
        }
        if matches!(self.keyboard.current_focus(), Some(FocusTarget::X11(_))) {
            self.focus_window(self.wm.focused, false);
        }
        self.queue_redraw_all();
    }

    pub fn x11_configure_request(
        &mut self,
        x11: &X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
    ) {
        let mut geo = x11.last_configure();
        geo.loc.x = x.unwrap_or(geo.loc.x);
        geo.loc.y = y.unwrap_or(geo.loc.y);
        geo.size.w = w.and_then(|w| i32::try_from(w).ok()).unwrap_or(geo.size.w);
        geo.size.h = h.and_then(|h| i32::try_from(h).ok()).unwrap_or(geo.size.h);

        let known = self
            .x11_id(x11)
            .and_then(|id| Some((id, self.wm.windows.get(&id)?.ws)));
        let Some((id, ws)) = known else {
            // Not managed yet: a client sizing its own unmapped window.
            if let Err(err) = x11.configure(geo) {
                tracing::debug!("x11: cannot configure the window: {err}");
            }
            return;
        };
        let Some(workspace) = self.wm.workspaces.get_mut(&ws) else {
            return;
        };
        let fullscreen = workspace.fullscreen().is_some_and(|(f, _)| f == id);
        if !workspace.is_floating(id) || fullscreen {
            // The layout decides; the answer is the geometry it gave.
            if let Err(err) = x11.configure(x11.last_configure()) {
                tracing::debug!("x11: cannot configure the window: {err}");
            }
            return;
        }
        let border = self.config.general.border_width.max(0);
        workspace.set_floating_rect(id, rect_of(geo).shrink(-border));
        self.relayout_ws(ws);
    }

    /// Override-redirect windows move and resize themselves.
    pub fn x11_configure_notify(&mut self, x11: &X11Surface, geo: Rectangle<i32, Logical>) {
        let window = self
            .xwayland
            .unmanaged
            .elements()
            .find(|w| w.x11_surface() == Some(x11))
            .cloned();
        if let Some(window) = window {
            self.xwayland.unmanaged.map_element(window, geo.loc, false);
            self.queue_redraw_all();
        }
    }

    pub fn x11_mode_request(&mut self, x11: &X11Surface, mode: FsMode, set: bool) {
        if let Some(id) = self.x11_id(x11) {
            self.request_mode_id(id, mode, set);
        }
    }

    /// The client asked to be moved or resized while it holds a button grab.
    pub fn x11_drag(&mut self, x11: &X11Surface, kind: DragKind, edge: Option<ResizeEdge>) {
        let Some(id) = self.x11_id(x11) else {
            return;
        };
        let pointer = self.pointer.clone();
        let Some(start) = pointer.grab_start_data() else {
            return;
        };
        self.start_drag(
            id,
            kind,
            edge.map(edges),
            start.button,
            SERIAL_COUNTER.next_serial(),
        );
    }

    /// `_NET_ACTIVE_WINDOW`: focus it when it can be seen. There is no urgency marker yet, so a
    /// window on a hidden workspace just stays where it is.
    pub fn x11_activate(&mut self, x11: &X11Surface) {
        let Some(id) = self.x11_id(x11) else {
            return;
        };
        // A window still waiting for its first buffer gets focus when it maps.
        let visible = self
            .wm
            .windows
            .get(&id)
            .is_some_and(|w| w.phase == Phase::Mapped && self.wm.ws_output.contains_key(&w.ws));
        if visible {
            self.focus_window(Some(id), true);
        }
    }

    /// Size hints changed.
    pub fn x11_hints_changed(&mut self, x11: &X11Surface) {
        if let Some(id) = self.x11_id(x11) {
            self.set_constraints(id, constraints(x11));
        }
    }
}
