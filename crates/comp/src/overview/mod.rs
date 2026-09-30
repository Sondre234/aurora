//! Live workspace overview. `Aurora.overview` is `Some` from the moment the `overview`
//! action opens it until its close animation has finished. While it is open (not closing)
//! it takes the keyboard and pointer (see `input`), draws a grid of workspace panels with live
//! window thumbnails on every output (see `render`) and leaves everything else running, so
//! clients keep getting frame callbacks.
//!
//! The state here is self contained (window elements are cloned in and kept in step with the
//! `Wm` by `tick`), because the render path only sees the `Space` and the renderer.
use std::{cell::RefCell, time::Duration};

use aurora_layout::{Rect, WinId};

use crate::{
    anim::Animated,
    config::AnimSpec,
    wm::{Phase, Wm, window::WindowElement},
};
use layout::{PanelLayout, WsInput};
use render::RenderState;

pub mod input;
pub mod layout;
mod render;

pub use render::{OverviewElement, area, push};

/// One window the overview shows.
pub struct Entry {
    pub id: WinId,
    pub ws: u32,
    pub element: WindowElement,
    /// Size the thumbnail takes its aspect ratio from.
    pub size: (i32, i32),
    /// Where the window is on screen when the overview opens (global logical), so its
    /// thumbnail grows out of it and shrinks back into it. `None` for windows on hidden
    /// workspaces and after the window moved.
    pub from: Option<Rect>,
}

pub struct Overview {
    /// The output that shows the workspaces no output shows.
    pub home: String,
    pub entries: Vec<Entry>,
    /// Workspace each output showed at the last sync.
    pub shown: Vec<(String, u32)>,
    pub workspaces: u32,
    pub selected: Option<WinId>,
    /// Workspace panel under the pointer, for highlighting.
    pub hover_panel: Option<u32>,
    /// What the left button went down on, until it comes up.
    pub press: Option<input::Press>,
    anim: Animated<f32>,
    closing: bool,
    render: RefCell<RenderState>,
}

impl Overview {
    /// Snapshot of `wm`. Starts closed (progress 0); `set_open` starts the animation.
    pub fn new(wm: &Wm, home: String, workspaces: u32) -> Self {
        let mut overview = Self {
            home,
            entries: Vec::new(),
            shown: Vec::new(),
            workspaces,
            selected: None,
            hover_panel: None,
            press: None,
            anim: Animated::new(0.0),
            closing: false,
            render: RefCell::default(),
        };
        overview.sync(wm);
        for entry in &mut overview.entries {
            let win = wm.windows.get(&entry.id);
            entry.from = win
                .filter(|w| wm.is_visible(w.id) && wm.ws_output.contains_key(&w.ws))
                .map(|w| w.current);
        }
        overview
    }

    /// Starts the open or close animation (a snap without a spec).
    pub fn set_open(&mut self, open: bool, now: Duration, spec: Option<AnimSpec>) {
        self.closing = !open;
        let to = if open { 1.0 } else { 0.0 };
        match spec {
            Some(s) => self.anim.retarget(
                to,
                now,
                Duration::from_millis(u64::from(s.duration_ms)),
                s.curve,
            ),
            None => self.anim.set(to),
        }
    }

    /// Whether the overview owns the input: open, not on its way out.
    pub fn grabs_input(&self) -> bool {
        !self.closing
    }

    pub fn is_closing(&self) -> bool {
        self.closing
    }

    /// 0 (gone) to 1 (fully open), curve applied.
    pub fn progress(&self, now: Duration) -> f32 {
        self.anim.value(now)
    }

    /// Re-reads windows and what each output shows from `wm`. Selection and entry origins
    /// of windows that are still there survive.
    pub fn sync(&mut self, wm: &Wm) {
        let mut entries: Vec<Entry> = wm
            .windows
            .values()
            .filter(|w| w.phase == Phase::Mapped && (1..=self.workspaces).contains(&w.ws))
            .map(|w| {
                let old = self.entries.iter().find(|e| e.id == w.id);
                Entry {
                    id: w.id,
                    ws: w.ws,
                    element: w.element.clone(),
                    size: if w.target.w > 0 && w.target.h > 0 {
                        (w.target.w, w.target.h)
                    } else {
                        (16, 10)
                    },
                    from: old.filter(|e| e.ws == w.ws).and_then(|e| e.from),
                }
            })
            .collect();
        entries.sort_by_key(|e| e.id);
        self.entries = entries;
        self.shown = wm
            .outputs
            .iter()
            .filter_map(|o| Some((o.name(), *wm.active_ws.get(o)?)))
            .collect();
        if !wm.outputs.iter().any(|o| o.name() == self.home)
            && let Some(first) = wm.outputs.first()
        {
            self.home = first.name();
        }
        if self
            .selected
            .is_some_and(|s| !self.entries.iter().any(|e| e.id == s))
        {
            self.selected = None;
        }
        let entries = &self.entries;
        self.render
            .get_mut()
            .retain(|id| entries.iter().any(|e| e.id == id));
    }

    pub fn entry(&self, id: WinId) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// The workspace `output` shows.
    pub fn shown_on(&self, output: &str) -> Option<u32> {
        self.shown
            .iter()
            .find(|(n, _)| n == output)
            .map(|(_, w)| *w)
    }

    /// Workspaces drawn on `output`: the home output shows every workspace no other output
    /// shows, any other output just its own.
    fn workspaces_on(&self, output: &str) -> Vec<u32> {
        if output == self.home {
            (1..=self.workspaces)
                .filter(|w| {
                    self.shown
                        .iter()
                        .all(|(name, shown)| name == output || shown != w)
                })
                .collect()
        } else {
            self.shown_on(output).into_iter().collect()
        }
    }

    /// Panels of `output` laid out inside `area` (the coordinates of `area` decide the
    /// coordinates of the result).
    pub fn panels_for(&self, output: &str, area: Rect) -> Vec<PanelLayout> {
        let inputs: Vec<WsInput> = self
            .workspaces_on(output)
            .into_iter()
            .map(|ws| WsInput {
                ws,
                windows: self
                    .entries
                    .iter()
                    .filter(|e| e.ws == ws)
                    .map(|e| (e.id, e.size.0, e.size.1))
                    .collect(),
            })
            .collect();
        layout::layout(area, &inputs)
    }

    /// A window committed: its thumbnail is repainted at the next frame.
    pub fn mark_dirty(&mut self, id: WinId) {
        self.render.get_mut().mark_dirty(id);
    }

    /// Follows the window manager and ends the overview once its close animation is over.
    /// Returns whether another frame is needed. A free function over the slot (not an
    /// `Aurora` method) because the DRM render loop holds the backend borrowed.
    pub fn tick(slot: &mut Option<Overview>, wm: &Wm, now: Duration) -> bool {
        let Some(overview) = slot else {
            return false;
        };
        overview.sync(wm);
        if overview.closing && !overview.anim.is_active(now) {
            *slot = None;
            // One more frame to draw the scene without it.
            return true;
        }
        overview.anim.is_active(now)
    }
}
