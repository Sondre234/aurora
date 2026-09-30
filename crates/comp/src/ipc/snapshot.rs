//! The observable state as IPC snapshots, and the diff between two of them.
//!
//! `collect` reads the window manager into plain `Inputs`; `assemble` (pure) turns those
//! into an `aurora_ipc::Snapshot`; `diff` (pure) gives the `Event`s that take one snapshot
//! to the next. The server runs collect, assemble and diff once per loop turn while someone
//! is subscribed, so nothing in the window manager has to know IPC exists.
//!
//! Workspaces: Aurora's are global and numbered, each output shows exactly one. The IPC
//! lists a workspace under the output that shows it; a hidden one is listed only while it
//! holds windows, under the output it was last shown on (or the one a workspace rule pins it
//! to, else the first output). `index` is the global number.
use std::collections::HashMap;

use aurora_ipc::{Event, OutputInfo, Snapshot, WindowInfo, WorkspaceInfo};

use crate::{Aurora, wm::Phase};

pub struct OutputIn {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub scale: f64,
    /// The workspace this output shows.
    pub shown: Option<u32>,
}

pub struct WindowIn {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub ws: u32,
    pub floating: bool,
    pub fullscreen: bool,
    pub urgent: bool,
}

/// Everything `assemble` needs, in plain values.
#[derive(Default)]
pub struct Inputs {
    pub outputs: Vec<OutputIn>,
    pub windows: Vec<WindowIn>,
    pub focused: Option<u64>,
    pub active_output: Option<String>,
    /// Workspace rules that pin a workspace to an output.
    pub pinned: Vec<(u32, String)>,
}

/// Where hidden workspaces were last shown, kept between snapshots so their output does
/// not jump around.
pub type Homes = HashMap<u32, String>;

pub fn collect(a: &Aurora) -> Inputs {
    let outputs =
        a.wm.outputs
            .iter()
            .map(|o| {
                let geo = a.space.output_geometry(o).unwrap_or_default();
                OutputIn {
                    name: o.name(),
                    x: geo.loc.x,
                    y: geo.loc.y,
                    w: geo.size.w,
                    h: geo.size.h,
                    scale: o.current_scale().fractional_scale(),
                    shown: a.wm.active_ws.get(o).copied(),
                }
            })
            .collect();
    let windows: Vec<WindowIn> =
        a.wm.windows
            .values()
            .filter(|w| w.phase == Phase::Mapped && w.ws != 0)
            .map(|w| WindowIn {
                id: w.id.0,
                app_id: w.app_id.clone(),
                title: w.title.clone(),
                ws: w.ws,
                floating: w.floating,
                fullscreen: w.fs,
                urgent: w.urgent,
            })
            .collect();
    let focused =
        a.wm.focused
            .map(|f| f.0)
            .filter(|f| windows.iter().any(|w| w.id == *f));
    Inputs {
        outputs,
        windows,
        focused,
        active_output: a.wm.active_output.as_ref().map(|o| o.name()),
        pinned: a
            .config
            .workspace_rules
            .iter()
            .filter_map(|r| Some((r.id, r.output.clone()?)))
            .collect(),
    }
}

/// The IPC view of `inputs`. Lists are sorted (outputs in connection order, workspaces by
/// output then number, windows by id) so equal states give equal snapshots.
pub fn assemble(inputs: &Inputs, homes: &mut Homes) -> Snapshot {
    let names: Vec<&str> = inputs.outputs.iter().map(|o| o.name.as_str()).collect();
    let mut shown: HashMap<u32, &str> = HashMap::new();
    for o in &inputs.outputs {
        if let Some(ws) = o.shown {
            shown.insert(ws, &o.name);
            homes.insert(ws, o.name.clone());
        }
    }
    let exists = |name: &str| names.contains(&name);
    let output_of = |ws: u32, homes: &Homes| -> String {
        if let Some(name) = shown.get(&ws) {
            return (*name).to_string();
        }
        inputs
            .pinned
            .iter()
            .find(|(id, out)| *id == ws && exists(out))
            .map(|(_, out)| out.clone())
            .or_else(|| homes.get(&ws).filter(|o| exists(o)).cloned())
            .or_else(|| names.first().map(|n| (*n).to_string()))
            .unwrap_or_default()
    };

    let outputs = inputs
        .outputs
        .iter()
        .map(|o| OutputInfo {
            name: o.name.clone(),
            x: o.x,
            y: o.y,
            width: o.w.max(0) as u32,
            height: o.h.max(0) as u32,
            scale: o.scale,
        })
        .collect();

    let mut windows: Vec<WindowInfo> = inputs
        .windows
        .iter()
        .map(|w| WindowInfo {
            id: w.id,
            app_id: w.app_id.clone(),
            title: w.title.clone(),
            workspace: w.ws,
            output: output_of(w.ws, homes),
            floating: w.floating,
            fullscreen: w.fullscreen,
            urgent: w.urgent,
        })
        .collect();
    windows.sort_by_key(|w| w.id);

    let mut numbers: Vec<u32> = shown.keys().copied().collect();
    numbers.extend(inputs.windows.iter().map(|w| w.ws));
    numbers.sort_unstable();
    numbers.dedup();
    let mut workspaces: Vec<WorkspaceInfo> = numbers
        .into_iter()
        .map(|ws| {
            let on: Vec<&WindowIn> = inputs.windows.iter().filter(|w| w.ws == ws).collect();
            WorkspaceInfo {
                output: output_of(ws, homes),
                index: ws,
                active: shown.contains_key(&ws),
                windows: on.len() as u32,
                urgent: on.iter().any(|w| w.urgent),
            }
        })
        .collect();
    let rank = |name: &str| names.iter().position(|n| *n == name).unwrap_or(usize::MAX);
    workspaces.sort_by_key(|w| (rank(&w.output), w.index));

    Snapshot {
        outputs,
        workspaces,
        windows,
        focused_window: inputs.focused,
        active_output: inputs.active_output.clone(),
    }
}

/// The events that take `old` to `new`, removals first within each kind. Empty when equal.
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Event> {
    let mut events = Vec::new();

    for o in &old.outputs {
        if !new.outputs.iter().any(|n| n.name == o.name) {
            events.push(Event::OutputRemoved {
                name: o.name.clone(),
            });
        }
    }
    for n in &new.outputs {
        if old.outputs.iter().find(|o| o.name == n.name) != Some(n) {
            events.push(Event::OutputChanged(n.clone()));
        }
    }

    let ws_key = |w: &WorkspaceInfo| (w.output.clone(), w.index);
    let old_ws: HashMap<_, _> = old.workspaces.iter().map(|w| (ws_key(w), w)).collect();
    let new_ws: HashMap<_, _> = new.workspaces.iter().map(|w| (ws_key(w), w)).collect();
    for o in &old.workspaces {
        if !new_ws.contains_key(&ws_key(o)) {
            events.push(Event::WorkspaceRemoved {
                output: o.output.clone(),
                index: o.index,
            });
        }
    }
    for n in &new.workspaces {
        if old_ws.get(&ws_key(n)) != Some(&n) {
            events.push(Event::WorkspaceChanged(n.clone()));
        }
    }

    let old_win: HashMap<u64, &WindowInfo> = old.windows.iter().map(|w| (w.id, w)).collect();
    let new_ids: std::collections::HashSet<u64> = new.windows.iter().map(|w| w.id).collect();
    for o in &old.windows {
        if !new_ids.contains(&o.id) {
            events.push(Event::WindowClosed { id: o.id });
        }
    }
    for n in &new.windows {
        if old_win.get(&n.id) != Some(&n) {
            events.push(Event::WindowChanged(n.clone()));
        }
    }

    if old.focused_window != new.focused_window || old.active_output != new.active_output {
        events.push(Event::FocusChanged {
            window: new.focused_window,
            output: new.active_output.clone(),
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &str, x: i32, shown: Option<u32>) -> OutputIn {
        OutputIn {
            name: name.into(),
            x,
            y: 0,
            w: 1920,
            h: 1080,
            scale: 1.0,
            shown,
        }
    }

    fn win(id: u64, ws: u32, title: &str) -> WindowIn {
        WindowIn {
            id,
            app_id: "app".into(),
            title: title.into(),
            ws,
            floating: false,
            fullscreen: false,
            urgent: false,
        }
    }

    fn inputs() -> Inputs {
        Inputs {
            outputs: vec![out("A", 0, Some(1)), out("B", 1920, Some(2))],
            windows: vec![win(2, 1, "two"), win(1, 1, "one"), win(3, 4, "hidden")],
            focused: Some(1),
            active_output: Some("A".into()),
            pinned: vec![],
        }
    }

    /// Lists sorted by identity so a patched snapshot compares to a built one.
    fn normal(mut s: Snapshot) -> Snapshot {
        s.outputs.sort_by(|a, b| a.name.cmp(&b.name));
        s.workspaces
            .sort_by(|a, b| (&a.output, a.index).cmp(&(&b.output, b.index)));
        s.windows.sort_by_key(|w| w.id);
        s
    }

    fn check_round_trip(old: &Snapshot, new: &Snapshot) {
        let mut patched = old.clone();
        for e in diff(old, new) {
            patched.apply(&e);
        }
        assert_eq!(normal(patched), normal(new.clone()));
    }

    #[test]
    fn assembles_outputs_workspaces_and_windows() {
        let mut homes = Homes::new();
        let s = assemble(&inputs(), &mut homes);
        assert_eq!(
            s.windows.iter().map(|w| w.id).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(s.focused_window, Some(1));
        assert_eq!(s.active_output.as_deref(), Some("A"));
        let ws: Vec<_> = s
            .workspaces
            .iter()
            .map(|w| (w.output.as_str(), w.index, w.active, w.windows))
            .collect();
        // ws 4 is hidden with one window and lands on the first output.
        assert_eq!(
            ws,
            [("A", 1, true, 2), ("A", 4, false, 1), ("B", 2, true, 0)]
        );
        assert_eq!(s.windows[2].output, "A");
        assert_eq!(s.windows[2].workspace, 4);
    }

    #[test]
    fn a_hidden_workspace_stays_on_the_output_it_was_last_shown_on() {
        let mut homes = Homes::new();
        let mut i = inputs();
        assemble(&i, &mut homes);
        // B now shows 5; ws 2 (empty before) gets a window and is hidden.
        i.outputs[1].shown = Some(5);
        i.windows.push(win(9, 2, "later"));
        let s = assemble(&i, &mut homes);
        let hidden = s.workspaces.iter().find(|w| w.index == 2).unwrap();
        assert_eq!(hidden.output, "B");
        assert!(!hidden.active);
    }

    #[test]
    fn a_rule_pins_a_hidden_workspace_to_its_output() {
        let mut homes = Homes::new();
        let mut i = inputs();
        i.pinned = vec![(4, "B".into()), (3, "GONE".into())];
        i.windows.push(win(7, 3, "x"));
        let s = assemble(&i, &mut homes);
        let find = |n: u32| s.workspaces.iter().find(|w| w.index == n).unwrap();
        assert_eq!(find(4).output, "B");
        // A pin to an output that is not connected is ignored.
        assert_eq!(find(3).output, "A");
    }

    #[test]
    fn urgent_windows_mark_their_workspace() {
        let mut homes = Homes::new();
        let mut i = inputs();
        i.windows[2].urgent = true;
        let s = assemble(&i, &mut homes);
        assert!(s.workspaces.iter().find(|w| w.index == 4).unwrap().urgent);
        assert!(!s.workspaces.iter().find(|w| w.index == 1).unwrap().urgent);
    }

    #[test]
    fn no_outputs_gives_an_empty_output_name_not_a_panic() {
        let mut homes = Homes::new();
        let i = Inputs {
            windows: vec![win(1, 3, "t")],
            ..Default::default()
        };
        let s = assemble(&i, &mut homes);
        assert_eq!(s.windows[0].output, "");
    }

    #[test]
    fn equal_snapshots_give_no_events() {
        let mut homes = Homes::new();
        let s = assemble(&inputs(), &mut homes);
        assert!(diff(&s, &s).is_empty());
        assert!(diff(&s, &assemble(&inputs(), &mut homes)).is_empty());
    }

    #[test]
    fn a_title_change_is_one_window_event() {
        let mut homes = Homes::new();
        let a = assemble(&inputs(), &mut homes);
        let mut i = inputs();
        i.windows[0].title = "renamed".into();
        let b = assemble(&i, &mut homes);
        let events = diff(&a, &b);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(&events[0], Event::WindowChanged(w) if w.id == 2 && w.title == "renamed"));
        check_round_trip(&a, &b);
    }

    #[test]
    fn a_switch_moves_active_and_occupancy_only_where_it_changed() {
        let mut homes = Homes::new();
        let a = assemble(&inputs(), &mut homes);
        let mut i = inputs();
        i.outputs[0].shown = Some(4); // A: 1 -> 4
        let b = assemble(&i, &mut homes);
        let events = diff(&a, &b);
        let topics: Vec<_> = events.iter().filter_map(Event::topic).collect();
        assert!(topics.iter().all(|t| *t == aurora_ipc::Topic::Workspaces));
        // ws 1 turns inactive and ws 4 active; ws 2 on B is untouched.
        assert_eq!(events.len(), 2, "{events:?}");
        check_round_trip(&a, &b);
    }

    #[test]
    fn a_workspace_changing_output_is_a_remove_and_a_change() {
        let mut homes = Homes::new();
        let a = assemble(&inputs(), &mut homes);
        let mut i = inputs();
        // B shows ws 4 now (it moves from A's hidden list to B).
        i.outputs[1].shown = Some(4);
        let b = assemble(&i, &mut homes);
        let events = diff(&a, &b);
        assert!(events.iter().any(
            |e| matches!(e, Event::WorkspaceRemoved { output, index } if output == "A" && *index == 4)
        ));
        check_round_trip(&a, &b);
    }

    #[test]
    fn close_focus_and_output_removal_round_trip() {
        let mut homes = Homes::new();
        let a = assemble(&inputs(), &mut homes);
        let mut i = inputs();
        i.windows.retain(|w| w.id != 1);
        i.focused = None;
        i.outputs.remove(1);
        i.active_output = Some("A".into());
        let b = assemble(&i, &mut homes);
        let events = diff(&a, &b);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::WindowClosed { id: 1 }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::OutputRemoved { name } if name == "B"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::FocusChanged { window: None, .. }))
        );
        check_round_trip(&a, &b);
    }

    #[test]
    fn new_window_focus_and_output_round_trip() {
        let mut homes = Homes::new();
        let a = assemble(&inputs(), &mut homes);
        let mut i = inputs();
        i.windows.push(win(10, 2, "new"));
        i.focused = Some(10);
        i.active_output = Some("B".into());
        i.outputs.push(out("C", 3840, Some(3)));
        let b = assemble(&i, &mut homes);
        check_round_trip(&a, &b);
        check_round_trip(&b, &a);
    }

    #[test]
    fn events_carry_their_topics() {
        let mut homes = Homes::new();
        let a = assemble(&inputs(), &mut homes);
        let mut i = inputs();
        i.windows[0].title = "t".into();
        i.focused = Some(2);
        let b = assemble(&i, &mut homes);
        let topics: Vec<_> = diff(&a, &b).iter().filter_map(Event::topic).collect();
        assert_eq!(
            topics,
            [aurora_ipc::Topic::Windows, aurora_ipc::Topic::Focus]
        );
    }
}
