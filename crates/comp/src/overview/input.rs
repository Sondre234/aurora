//! The overview's side of the `Aurora` state: opening and closing it, and taking over the
//! keyboard and pointer while it is open. The input primitives (`on_key`, `on_pointer_*`)
//! call in here first, so virtual keyboards and the QA hooks are covered too.
use std::time::Duration;

use aurora_layout::{Dir, Point as LPoint, Rect, WinId, neighbor};
use smithay::{
    backend::input::{ButtonState, InputTime},
    input::{
        keyboard::{Keysym, keysyms},
        pointer::MotionEvent,
    },
    output::Output,
    utils::{Logical, Point, SERIAL_COUNTER},
};

use super::{
    Overview,
    layout::{self, PanelLayout, Target},
};
use crate::{action::WsTarget, config::AnimKind, layers::Hit, state::Aurora};

/// evdev BTN_LEFT / BTN_RIGHT.
const BTN_LEFT: u32 = 272;
const BTN_RIGHT: u32 = 273;

/// What the left button went down on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Tile(WinId),
    Panel(u32),
}

/// A key the overview acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverviewKey {
    Close,
    Activate,
    Nav(Dir),
    /// Next (true) or previous selection in layout order.
    Cycle(bool),
}

/// The overview's meaning of a key press, `None` for keys it just swallows.
pub fn key_for(syms: &[Keysym], shift: bool) -> Option<OverviewKey> {
    syms.iter().find_map(|sym| match sym.raw() {
        keysyms::KEY_Escape => Some(OverviewKey::Close),
        keysyms::KEY_Return | keysyms::KEY_KP_Enter => Some(OverviewKey::Activate),
        keysyms::KEY_Left => Some(OverviewKey::Nav(Dir::Left)),
        keysyms::KEY_Right => Some(OverviewKey::Nav(Dir::Right)),
        keysyms::KEY_Up => Some(OverviewKey::Nav(Dir::Up)),
        keysyms::KEY_Down => Some(OverviewKey::Nav(Dir::Down)),
        keysyms::KEY_Tab => Some(OverviewKey::Cycle(!shift)),
        keysyms::KEY_ISO_Left_Tab => Some(OverviewKey::Cycle(false)),
        _ => None,
    })
}

impl Aurora {
    /// Whether the overview currently owns keyboard and pointer.
    pub fn overview_grabs_input(&self) -> bool {
        !self.is_locked() && self.overview.as_ref().is_some_and(Overview::grabs_input)
    }

    fn now_duration(&self) -> Duration {
        Duration::from(self.clock.now())
    }

    /// The `overview` action.
    pub fn toggle_overview(&mut self) {
        if self.overview_grabs_input() {
            self.close_overview(false);
        } else {
            self.open_overview();
        }
    }

    fn open_overview(&mut self) {
        if self.is_locked() {
            return;
        }
        if self.pointer.is_grabbed() {
            tracing::info!("overview: not opening during a pointer grab");
            return;
        }
        let now = self.now_duration();
        let spec = self.config.animations.spec(AnimKind::Fade);
        if let Some(overview) = self.overview.as_mut() {
            // Reopened while closing: turn around from wherever it is.
            overview.set_open(true, now, spec);
        } else {
            let home = self
                .wm
                .active_output
                .as_ref()
                .or(self.wm.outputs.first())
                .map(Output::name);
            let Some(home) = home else {
                tracing::info!("overview: no output");
                return;
            };
            let mut overview = Overview::new(&self.wm, home, self.config.general.workspaces);
            overview.selected = self.wm.focused.filter(|f| overview.entry(*f).is_some());
            overview.set_open(true, now, spec);
            self.overview = Some(overview);
        }
        tracing::info!("overview: open");
        self.cancel_repeat();
        // Clients lose the pointer (a held button ends with the leave).
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location: pointer.current_location(),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        pointer.frame(self);
        self.queue_redraw_all();
    }

    /// Starts the close animation. Input goes back to the windows at once. `activated` means
    /// the workspace or focus changed, so thumbnails just fade instead of flying back to
    /// where the windows were.
    pub fn close_overview(&mut self, activated: bool) {
        let now = self.now_duration();
        let spec = self.config.animations.spec(AnimKind::Fade);
        let Some(overview) = self.overview.as_mut().filter(|o| !o.is_closing()) else {
            return;
        };
        if activated {
            for entry in &mut overview.entries {
                entry.from = None;
            }
        }
        overview.press = None;
        overview.hover_panel = None;
        overview.set_open(false, now, spec);
        tracing::info!("overview: close");

        let pointer = self.pointer.clone();
        let pos = pointer.current_location();
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        pointer.frame(self);
        // Focus keeps following the keyboard until the pointer crosses a window.
        self.wm.hover = match self.hit_test(pos) {
            Hit::Window(window, _) => Some(window.id()),
            _ => None,
        };
        self.queue_redraw_all();
    }

    /// Every output's panels in global logical coordinates.
    fn overview_screens(&self) -> Vec<(Output, Vec<PanelLayout>)> {
        let Some(overview) = &self.overview else {
            return Vec::new();
        };
        self.space
            .outputs()
            .filter_map(|output| {
                let geo = self.space.output_geometry(output)?;
                let area = super::area(output, geo);
                Some((output.clone(), overview.panels_for(&output.name(), area)))
            })
            .collect()
    }

    fn overview_hit(&self, pos: Point<f64, Logical>) -> Option<(Output, Target)> {
        let p = LPoint {
            x: pos.x.floor() as i32,
            y: pos.y.floor() as i32,
        };
        self.overview_screens()
            .into_iter()
            .find_map(|(output, panels)| layout::hit(&panels, p).map(|t| (output, t)))
    }

    pub fn overview_key(&mut self, key: OverviewKey) {
        match key {
            OverviewKey::Close => self.close_overview(false),
            OverviewKey::Activate => {
                let selected = self.overview.as_ref().and_then(|o| o.selected);
                if let Some(id) = selected {
                    let output = self.wm.active_output.clone();
                    self.overview_activate(id, output);
                }
            }
            OverviewKey::Nav(dir) => {
                self.overview_select(|tiles, from| neighbor(tiles, from, dir, &[]).or(Some(from)))
            }
            OverviewKey::Cycle(forward) => self.overview_select(|tiles, from| {
                let i = tiles.iter().position(|(id, _)| *id == from)?;
                let n = tiles.len();
                Some(
                    tiles[if forward {
                        (i + 1) % n
                    } else {
                        (i + n - 1) % n
                    }]
                    .0,
                )
            }),
        }
    }

    /// Moves the selection with `pick(tiles, selected)`; starts at the first tile when
    /// nothing is selected.
    fn overview_select(&mut self, pick: impl Fn(&[(WinId, Rect)], WinId) -> Option<WinId>) {
        let tiles: Vec<(WinId, Rect)> = self
            .overview_screens()
            .iter()
            .flat_map(|(_, panels)| panels.iter())
            .flat_map(|panel| panel.tiles.iter().copied())
            .collect();
        let Some(overview) = self.overview.as_mut() else {
            return;
        };
        let current = overview
            .selected
            .filter(|s| tiles.iter().any(|(id, _)| id == s));
        overview.selected = match current {
            Some(from) => pick(&tiles, from),
            None => tiles.first().map(|(id, _)| *id),
        };
        self.queue_redraw_all();
    }

    /// Pointer movement while the overview owns the pointer: the cursor moves, clients see
    /// nothing, the thumbnail or panel under it is highlighted.
    pub fn overview_motion(&mut self, pos: Point<f64, Logical>, time: InputTime) {
        let pos = self.clamp_pointer(pos);
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        let hit = self.overview_hit(pos);
        if let Some(overview) = self.overview.as_mut() {
            match hit {
                Some((_, Target::Tile(ws, id))) => {
                    overview.selected = Some(id);
                    overview.hover_panel = Some(ws);
                }
                Some((_, Target::Panel(ws))) => overview.hover_panel = Some(ws),
                None => overview.hover_panel = None,
            }
        }
        self.queue_redraw_all();
    }

    /// A pointer button while the overview owns the pointer. Left press and release on the
    /// same thumbnail focuses it, released over another workspace's panel it moves the window
    /// there; a click on a panel shows that workspace; a click outside or the right button
    /// closes the overview.
    pub fn overview_button(&mut self, button: u32, state: ButtonState) {
        let pos = self.pointer.current_location();
        let hit = self.overview_hit(pos);
        match (button, state) {
            (BTN_LEFT, ButtonState::Pressed) => {
                let press = match hit {
                    Some((_, Target::Tile(_, id))) => Some(Press::Tile(id)),
                    Some((_, Target::Panel(ws))) => Some(Press::Panel(ws)),
                    None => {
                        self.close_overview(false);
                        return;
                    }
                };
                if let Some(overview) = self.overview.as_mut() {
                    overview.press = press;
                }
            }
            (BTN_LEFT, ButtonState::Released) => {
                let press = self.overview.as_mut().and_then(|o| o.press.take());
                match (press, hit) {
                    (Some(Press::Tile(id)), Some((output, Target::Tile(_, over))))
                        if over == id =>
                    {
                        self.overview_activate(id, Some(output));
                    }
                    (Some(Press::Tile(id)), Some((_, Target::Tile(ws, _) | Target::Panel(ws)))) => {
                        self.overview_move(id, ws)
                    }
                    (Some(Press::Panel(ws)), Some((output, Target::Panel(over)))) if over == ws => {
                        self.overview_show(ws, Some(output));
                        self.close_overview(true);
                    }
                    _ => {}
                }
            }
            (BTN_RIGHT, ButtonState::Pressed) => self.close_overview(false),
            _ => {}
        }
    }

    /// Shows workspace `ws`: on the output that already shows it, else on `output`.
    fn overview_show(&mut self, ws: u32, output: Option<Output>) {
        match self.wm.output_for_ws(ws) {
            Some(shown_on) => self.focus_output_ws(&shown_on),
            None => {
                if let Some(output) = output {
                    self.wm.active_output = Some(output);
                }
                self.switch_workspace(WsTarget::Num(ws));
            }
        }
    }

    /// Focuses window `id`, showing its workspace first, and closes the overview.
    fn overview_activate(&mut self, id: WinId, output: Option<Output>) {
        let Some(ws) = self.wm.windows.get(&id).map(|w| w.ws) else {
            return;
        };
        self.overview_show(ws, output);
        self.focus_window(Some(id), true);
        self.close_overview(true);
    }

    /// Moves window `id` (and its transients) to workspace `dst`; the overview stays open.
    fn overview_move(&mut self, id: WinId, dst: u32) {
        let Some(src) = self.wm.windows.get(&id).map(|w| w.ws) else {
            return;
        };
        if src == dst || dst == 0 || dst > self.config.general.workspaces {
            return;
        }
        let was_focused = self.wm.focused == Some(id);
        tracing::info!("overview: move win={} ws={src}->{dst}", id.0);
        self.relocate_with_children(id, dst);
        let next = self
            .wm
            .workspaces
            .get(&src)
            .and_then(|w| w.focus_after_close(id));
        self.relayout_ws(src);
        self.relayout_ws(dst);
        if was_focused {
            self.focus_window(next, true);
        }
        self.normalize();
        if let Some(overview) = self.overview.as_mut() {
            overview.sync(&self.wm);
            if let Some(entry) = overview.entries.iter_mut().find(|e| e.id == id) {
                entry.from = None;
            }
        }
        self.queue_redraw_all();
    }

    /// `dump: overview` for the SIGUSR2 state dump.
    pub fn dump_overview(&self) {
        let Some(overview) = &self.overview else {
            tracing::info!("dump: overview state=closed");
            return;
        };
        let state = if overview.is_closing() {
            "closing"
        } else {
            "open"
        };
        let progress = overview.progress(self.now_duration());
        let tiles: usize = self
            .overview_screens()
            .iter()
            .map(|(_, panels)| panels.iter().map(|p| p.tiles.len()).sum::<usize>())
            .sum();
        tracing::info!(
            "dump: overview state={state} progress={progress:.2} home={} windows={} tiles={tiles} selected={}",
            overview.home,
            overview.entries.len(),
            overview
                .selected
                .map_or_else(|| "none".to_string(), |s| s.0.to_string()),
        );
    }
}
