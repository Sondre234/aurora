//! The window registry. `Wm` owns every window and workspace; the `Space` is only a
//! projection of it that `apply` keeps in sync. Rectangles here are plain retained state.
use std::collections::HashMap;

use aurora_layout::{self as layout, Constraints, Gaps, LayoutParams, Rect, WinId};
use smithay::{
    output::Output,
    reexports::wayland_server::{backend::ObjectId, protocol::wl_surface::WlSurface},
};

use crate::config::Config;
use window::WindowElement;

pub mod apply;
pub mod focus;
pub mod rules;
pub mod window;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Created, not yet shown: waiting for its first buffer.
    Pending,
    Mapped,
}

pub struct WinData {
    pub id: WinId,
    pub element: WindowElement,
    pub ws: u32,
    pub floating: bool,
    pub fs: bool,
    pub parent: Option<WinId>,
    pub phase: Phase,
    /// Whether the window was inserted into a workspace layout yet.
    pub placed: bool,
    /// Content rectangle (border excluded) from the last layout. M3 animates `current`
    /// toward `target`; for now they are equal.
    pub target: Rect,
    pub current: Rect,
    pub sent_size: Option<(i32, i32)>,
    pub sent_flags: (bool, bool),
    pub constraints: Constraints,
    pub app_id: String,
    /// Frame callbacks sent to this window, for the QA dump.
    pub frames_sent: u64,
}

#[derive(Default)]
pub struct Wm {
    pub windows: HashMap<WinId, WinData>,
    pub by_surface: HashMap<ObjectId, WinId>,
    /// Created on first use; an empty workspace is just a default value.
    pub workspaces: HashMap<u32, layout::Workspace>,
    pub outputs: Vec<Output>,
    /// Which output a workspace is shown on.
    pub ws_output: HashMap<u32, Output>,
    pub active_ws: HashMap<Output, u32>,
    pub focused: Option<WinId>,
    pub active_output: Option<Output>,
    /// Window under the pointer at the last motion, for focus-follows-mouse.
    pub hover: Option<WinId>,
    /// Last logged layout line per workspace, so only changes are logged.
    pub last_layout: HashMap<u32, String>,
    next_id: u64,
}

impl Wm {
    pub fn alloc_id(&mut self) -> WinId {
        self.next_id += 1;
        WinId(self.next_id)
    }

    pub fn id_of(&self, surface: &WlSurface) -> Option<WinId> {
        use smithay::reexports::wayland_server::Resource;
        self.by_surface.get(&surface.id()).copied()
    }

    pub fn window_of(&self, surface: &WlSurface) -> Option<&WinData> {
        self.windows.get(&self.id_of(surface)?)
    }

    pub fn output_for_ws(&self, ws: u32) -> Option<Output> {
        self.ws_output.get(&ws).cloned()
    }

    /// Registers an output; it shows the lowest workspace that no output shows yet.
    pub fn output_added(&mut self, output: &Output) {
        if self.outputs.contains(output) {
            return;
        }
        let ws = (1..).find(|w| !self.ws_output.contains_key(w)).unwrap_or(1);
        self.outputs.push(output.clone());
        self.active_ws.insert(output.clone(), ws);
        self.ws_output.insert(ws, output.clone());
        if self.active_output.is_none() {
            self.active_output = Some(output.clone());
        }
    }

    /// Windows stay on their workspace, which is hidden until an output shows it again.
    pub fn output_removed(&mut self, output: &Output) {
        self.outputs.retain(|o| o != output);
        self.active_ws.remove(output);
        self.ws_output.retain(|_, o| o != output);
        if self.active_output.as_ref() == Some(output) {
            self.active_output = self.outputs.first().cloned();
        }
    }
}

pub fn layout_params(config: &Config) -> LayoutParams {
    let g = &config.general;
    LayoutParams {
        gaps: Gaps {
            inner: g.gaps_in,
            outer: g.gaps_out,
        },
        border: g.border_width,
        ..Default::default()
    }
}
