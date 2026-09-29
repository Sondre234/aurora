//! The window registry. `Wm` owns every window and workspace; the `Space` is only a
//! projection of it that `apply` keeps in sync. Rectangles here are plain retained state.
use std::collections::HashMap;

use aurora_layout::{self as layout, Constraints, Edges, FsMode, Gaps, LayoutParams, Rect, WinId};
use smithay::{
    output::Output,
    reexports::wayland_server::{backend::ObjectId, protocol::wl_surface::WlSurface},
};

use crate::config::Config;
use window::WindowElement;

pub mod apply;
pub mod focus;
pub mod grabs;
pub mod modes;
pub mod outputs;
pub mod rules;
pub mod window;
pub mod workspaces;
pub mod x11;

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
    pub sent_flags: (bool, bool, bool),
    pub constraints: Constraints,
    pub app_id: String,
    /// Outer rectangle it had when last floating and the output frame it was in, restored by
    /// toggle-floating.
    pub float_rect: Option<(Rect, Rect)>,
    /// Set during a floating resize: the edges that stay put while the client catches up.
    pub resize_anchor: Option<Anchor>,
    /// Fullscreen or maximize asked for before the window was placed.
    pub want_mode: Option<FsMode>,
    /// Frame callbacks sent to this window, for the QA dump.
    pub frames_sent: u64,
    /// An activation request arrived while the window was not focusable; cleared on focus.
    pub urgent: bool,
    /// Set when a removed output pushed the window onto another one, so the output can take
    /// it back if it returns before the window was moved by hand.
    pub rescued_from: Option<Rescue>,
}

impl WinData {
    pub fn new(id: WinId, element: WindowElement) -> Self {
        Self {
            id,
            element,
            ws: 0,
            floating: false,
            fs: false,
            parent: None,
            phase: Phase::Pending,
            placed: false,
            target: Rect::default(),
            current: Rect::default(),
            sent_size: None,
            sent_flags: (false, false, false),
            constraints: Constraints::default(),
            app_id: String::new(),
            float_rect: None,
            resize_anchor: None,
            want_mode: None,
            frames_sent: 0,
            urgent: false,
            rescued_from: None,
        }
    }
}

/// Where a rescued window came from and where it was put.
#[derive(Clone)]
pub struct Rescue {
    pub output: String,
    pub to_ws: u32,
}

/// The far edges of a window being resized from its left or top, in content coordinates.
#[derive(Clone, Copy)]
pub struct Anchor {
    pub edges: Edges,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Default)]
pub struct Wm {
    pub windows: HashMap<WinId, WinData>,
    pub by_surface: HashMap<ObjectId, WinId>,
    /// Managed X11 windows by their X window id; override-redirect ones are not in Wm.
    pub by_x11: HashMap<u32, WinId>,
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
    /// What each output showed before its current workspace, for back-and-forth.
    pub previous: HashMap<Output, u32>,
    /// Last logged `ws: visible` line.
    pub last_visible: String,
    /// Last logged layout line per workspace, so only changes are logged.
    pub last_layout: HashMap<u32, String>,
    /// Full rectangle of the output each workspace was last laid out on, so floating windows
    /// follow when the output moves, resizes or another output takes the workspace.
    pub last_full: HashMap<u32, Rect>,
    /// The workspace each output name showed when it went away.
    pub last_ws_of: HashMap<String, u32>,
    /// The workspace the last removed output showed, for a different output that replaces it.
    pub orphan_ws: Option<u32>,
    /// Positions the QA `debug-add-output` asked for, standing in for a config entry.
    pub debug_positions: HashMap<String, (i32, i32)>,
    /// The running interactive grab, if any. Layout logging is quiet and configures wait
    /// for acks while it runs.
    pub drag: Option<&'static str>,
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

    /// Whether `id` can be seen: a fullscreen or maximized window hides every other window
    /// of its workspace except its own transient descendants.
    pub fn is_visible(&self, id: WinId) -> bool {
        let Some(win) = self.windows.get(&id) else {
            return false;
        };
        let Some((front, _)) = self.workspaces.get(&win.ws).and_then(|w| w.fullscreen()) else {
            return true;
        };
        let mut cur = Some(id);
        // Bounded so a parent cycle from a client cannot hang the walk.
        for _ in 0..16 {
            match cur {
                Some(c) if c == front => return true,
                Some(c) => cur = self.windows.get(&c).and_then(|w| w.parent),
                None => break,
            }
        }
        false
    }

    /// The fullscreen or maximized window of `ws`, if any.
    pub fn front_window(&self, ws: u32) -> Option<WinId> {
        self.workspaces.get(&ws)?.fullscreen().map(|(f, _)| f)
    }

    /// Whether a fullscreen window (not merely maximized) covers what `output` shows.
    pub fn output_fullscreen(&self, output: &Output) -> bool {
        self.active_ws
            .get(output)
            .and_then(|ws| self.workspaces.get(ws))
            .and_then(|w| w.fullscreen())
            .is_some_and(|(_, mode)| mode == FsMode::Fullscreen)
    }

    pub fn output_for_ws(&self, ws: u32) -> Option<Output> {
        self.ws_output.get(&ws).cloned()
    }

    /// Registers an output. It shows the workspace pinned to it by a rule (the `default` one
    /// first), else the one it showed before, else the lowest workspace that no output shows
    /// and no rule pins elsewhere. A pinned workspace another output is showing moves back.
    pub fn output_added(&mut self, output: &Output, config: &Config) {
        if self.outputs.contains(output) {
            return;
        }
        let name = output.name();
        let count = config.general.workspaces;
        let rules = &config.workspace_rules;
        let pins_elsewhere = |w: &u32| {
            rules
                .iter()
                .any(|r| r.id == *w && r.output.as_deref().is_some_and(|o| o != name))
        };
        let pinned_here = rules
            .iter()
            .filter(|r| r.output.as_deref() == Some(name.as_str()) && r.id <= count)
            .max_by_key(|r| r.default)
            .map(|r| r.id);
        let shown = |this: &Self, w: &u32| this.ws_output.contains_key(w);
        let unpinned = |this: &Self, w: &u32| {
            !shown(this, w) && !rules.iter().any(|r| r.id == *w && r.output.is_some())
        };

        // A pinned workspace shown on another output goes home; that output gets another.
        if let Some(ws) = pinned_here.filter(|w| shown(self, w))
            && let Some(other) = self.ws_output.get(&ws).cloned()
        {
            let spare = (1..=count)
                .find(|w| unpinned(self, w))
                .or_else(|| (1..=count).find(|w| !shown(self, w)));
            if let Some(spare) = spare {
                self.ws_output.remove(&ws);
                self.active_ws.insert(other.clone(), spare);
                self.ws_output.insert(spare, other.clone());
                self.previous.remove(&other);
            }
        }
        let remembered = self
            .last_ws_of
            .get(&name)
            .copied()
            .or(self.outputs.is_empty().then_some(self.orphan_ws).flatten())
            .filter(|w| (1..=count).contains(w) && !shown(self, w) && !pins_elsewhere(w));
        let ws = pinned_here
            .filter(|w| !shown(self, w))
            .or(remembered)
            .or_else(|| (1..=count).find(|w| unpinned(self, w)))
            .or_else(|| (1..=count).find(|w| !shown(self, w)))
            .unwrap_or(1);
        self.outputs.push(output.clone());
        self.active_ws.insert(output.clone(), ws);
        self.ws_output.insert(ws, output.clone());
        if self.active_output.is_none() {
            self.active_output = Some(output.clone());
        }
    }

    /// Drops the output from the bookkeeping. Windows are rescued before this (see
    /// `Aurora::wm_output_removed`); whatever is left stays on its hidden workspace.
    pub fn output_removed(&mut self, output: &Output) {
        if let Some(ws) = self.active_ws.get(output).copied() {
            self.last_ws_of.insert(output.name(), ws);
            self.orphan_ws = Some(ws);
        }
        self.outputs.retain(|o| o != output);
        self.active_ws.remove(output);
        self.ws_output.retain(|_, o| o != output);
        self.previous.remove(output);
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
