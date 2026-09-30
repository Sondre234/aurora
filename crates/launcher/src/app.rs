//! The launcher daemon: one Overlay layer surface built at startup and shown/hidden, fed by
//! worker threads (index rescans, icon decoding, IPC, the control socket) through a single
//! calloop channel, so the loop thread only ever paints and updates small state.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aurora_ipc::{ThemeSnapshot, WindowId};
use aurora_ui::runtime::calloop::channel::{self, Channel, Sender};
use aurora_ui::runtime::{
    Anchor, App, Client, Event, KeyboardInteractivity, Layer, LayerConfig, Runtime, SurfaceId,
};
use aurora_ui::{Id, Insets, Key, KeyEvent, TextSystem, Ui, UiEvent};

use crate::control::{self, Command};
use crate::entry::AppEntry;
use crate::frecency::{self, Frecency};
use crate::icons::{IconCache, IconLoader, Loaded, default_theme};
use crate::index::{Hit, Index};
use crate::ipc::{IpcLink, IpcMsg};
use crate::system;
use crate::view::{self, ICON_PX, INPUT, Look, MAX_ROWS, RESULTS, ROOT, ROW_BASE, Row};
use crate::watch;

/// Icon cache budget in bytes (52x52 RGBA is ~11 KiB, so roughly 1500 icons).
const ICON_BUDGET: usize = 16 * 1024 * 1024;
/// A focus change this soon after showing is the compositor settling, not the user leaving.
const FOCUS_GRACE: Duration = Duration::from_millis(250);
/// How many of the default (empty query) results get their icons decoded at startup.
const PREWARM: usize = 24;

/// Everything worker threads tell the loop.
pub enum Msg {
    Index(Index),
    Icon(Loaded),
    Ipc(IpcMsg),
    Control(Command),
}

/// What a key that the widgets did not consume means for the launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Hide,
    Next,
    Prev,
    Launch,
}

pub fn key_action(k: &KeyEvent) -> Option<KeyAction> {
    let ctrl_only = k.mods.ctrl && !k.mods.alt && !k.mods.logo;
    match k.key {
        Key::Escape => Some(KeyAction::Hide),
        Key::Down => Some(KeyAction::Next),
        Key::Up => Some(KeyAction::Prev),
        Key::Enter => Some(KeyAction::Launch),
        Key::Char(c) if ctrl_only => match c.to_ascii_lowercase() {
            'n' | 'j' => Some(KeyAction::Next),
            'p' | 'k' => Some(KeyAction::Prev),
            _ => None,
        },
        _ => None,
    }
}

/// Moves a selection by `delta` with wraparound. 0 when there are no rows.
pub fn step(selected: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as i64 + delta as i64).rem_euclid(len as i64) as usize
}

/// New visibility after `cmd`.
pub fn visibility_after(visible: bool, cmd: Command) -> bool {
    match cmd {
        Command::Toggle => !visible,
        Command::Show => true,
        Command::Hide => false,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub struct Launcher {
    index: Index,
    frecency: Frecency,
    frecency_path: Option<std::path::PathBuf>,
    icons: IconCache,
    loader: IconLoader,
    look: Look,
    theme_rev: Option<u64>,
    surface: Option<SurfaceId>,
    visible: bool,
    started: bool,
    query: String,
    hits: Vec<Hit>,
    selected: usize,
    ipc: IpcLink,
    pending_spawns: HashMap<u64, String>,
    focus: Option<WindowId>,
    shown_at: Instant,
}

impl Launcher {
    fn new(tx: &Sender<Msg>) -> Self {
        let frecency_path = frecency::state_path();
        let frecency = frecency_path
            .as_deref()
            .map(Frecency::load)
            .unwrap_or_default();
        let icon_tx = tx.clone();
        let loader = IconLoader::spawn(default_theme(), ICON_PX, move |l| {
            let _ = icon_tx.send(Msg::Icon(l));
        });
        let ipc_tx = tx.clone();
        let ipc = IpcLink::start(move |m| {
            let _ = ipc_tx.send(Msg::Ipc(m));
        });
        Self {
            index: Index::default(),
            frecency,
            frecency_path,
            icons: IconCache::new(ICON_BUDGET),
            loader,
            look: Look::default(),
            theme_rev: None,
            surface: None,
            visible: false,
            started: false,
            query: String::new(),
            hits: Vec::new(),
            selected: 0,
            ipc,
            pending_spawns: HashMap::new(),
            focus: None,
            shown_at: Instant::now(),
        }
    }

    // ---- results ----

    /// Re-runs the search for the current query and redraws the rows.
    fn refresh(&mut self, rt: &mut Runtime<Self>) {
        self.hits = self
            .index
            .search(&self.query, &self.frecency, now_secs(), MAX_ROWS);
        self.selected = 0;
        self.render_rows(rt);
    }

    fn rows(&mut self) -> Vec<Row> {
        let mut rows = Vec::with_capacity(self.hits.len());
        for hit in &self.hits {
            let Some(app) = self.index.get(hit.app) else {
                continue;
            };
            let icon = match app.icon.as_deref() {
                Some(name) => match self.icons.get(name) {
                    Some(found) => found,
                    None => {
                        if self.icons.should_request(name) {
                            self.loader.request(name.to_string());
                        }
                        None
                    }
                },
                None => None,
            };
            rows.push(Row {
                title: app.name.clone(),
                positions: hit.positions.clone(),
                subtitle: app.generic_name.clone().or_else(|| app.comment.clone()),
                icon,
            });
        }
        rows
    }

    fn render_rows(&mut self, rt: &mut Runtime<Self>) {
        let Some(sid) = self.surface else { return };
        let rows = self.rows();
        let nodes = view::result_nodes(&self.look, &rows, self.selected, !self.query.is_empty());
        if let Some(ui) = rt.ui(sid) {
            ui.edit(Id(RESULTS), |n| n.children = nodes);
        }
    }

    fn select(&mut self, rt: &mut Runtime<Self>, delta: i32) {
        let next = step(self.selected, self.hits.len(), delta);
        if next != self.selected {
            self.selected = next;
            self.render_rows(rt);
        }
    }

    // ---- visibility ----

    fn show(&mut self, rt: &mut Runtime<Self>) {
        let Some(sid) = self.surface else { return };
        if self.visible {
            return;
        }
        self.visible = true;
        self.shown_at = Instant::now();
        self.query.clear();
        if let Some(ui) = rt.ui(sid) {
            ui.edit(Id(INPUT), |n| n.set_text(""));
            ui.set_focus(Some(Id(INPUT)));
        }
        self.refresh(rt);
        rt.show(sid);
        tracing::info!("launcher: show");
    }

    fn hide(&mut self, rt: &mut Runtime<Self>) {
        let Some(sid) = self.surface else { return };
        if !self.visible {
            return;
        }
        self.visible = false;
        rt.hide(sid);
        tracing::info!("launcher: hide");
    }

    fn command(&mut self, rt: &mut Runtime<Self>, cmd: Command) {
        if visibility_after(self.visible, cmd) {
            self.show(rt);
        } else {
            self.hide(rt);
        }
    }

    // ---- launching ----

    fn launch_row(&mut self, rt: &mut Runtime<Self>, row: usize) {
        let Some(app) = self
            .hits
            .get(row)
            .and_then(|h| self.index.get(h.app))
            .cloned()
        else {
            return;
        };
        self.launch(rt, &app);
    }

    fn launch(&mut self, rt: &mut Runtime<Self>, app: &AppEntry) {
        let now = now_secs();
        self.frecency.record_launch(&app.id, now);
        self.frecency.prune(now);
        if let Some(path) = &self.frecency_path
            && let Err(err) = self.frecency.save(path)
        {
            tracing::warn!("launcher: cannot save frecency: {err}");
        }
        let argv = if app.terminal {
            system::terminal_argv(std::env::var("TERMINAL").ok().as_deref(), &app.argv)
        } else {
            app.argv.clone()
        };
        tracing::info!("launcher: launch {}", app.id);
        self.hide(rt);
        match self.ipc.spawn(argv.clone()) {
            Some(id) => {
                self.pending_spawns.insert(id, app.id.clone());
            }
            None => {
                tracing::info!("launcher: ipc unavailable, spawning {} directly", app.id);
                system::spawn_direct(&argv);
            }
        }
    }

    // ---- messages from worker threads ----

    fn on_msg(&mut self, rt: &mut Runtime<Self>, msg: Msg) {
        match msg {
            Msg::Index(new) => {
                let diff = self.index.diff(&new);
                if diff.is_empty() {
                    return;
                }
                tracing::info!(
                    "launcher: index updated apps={} added={} removed={} changed={}",
                    new.len(),
                    diff.added.len(),
                    diff.removed.len(),
                    diff.changed.len()
                );
                self.index = new;
                if self.visible {
                    self.refresh(rt);
                } else {
                    self.hits.clear();
                }
            }
            Msg::Icon((name, raw)) => {
                self.icons.store(name, raw);
                if self.visible {
                    self.render_rows(rt);
                }
            }
            Msg::Control(cmd) => self.command(rt, cmd),
            Msg::Ipc(m) => self.on_ipc(rt, m),
        }
    }

    fn on_ipc(&mut self, rt: &mut Runtime<Self>, msg: IpcMsg) {
        match msg {
            IpcMsg::Connected => tracing::info!("launcher: ipc connected"),
            IpcMsg::Disconnected => {
                if !self.pending_spawns.is_empty() {
                    tracing::warn!(
                        "launcher: ipc closed with {} unanswered spawns",
                        self.pending_spawns.len()
                    );
                    self.pending_spawns.clear();
                }
            }
            IpcMsg::Theme(snap) => self.apply_theme(rt, snap),
            IpcMsg::Focus(window) => {
                let moved = window.is_some() && window != self.focus;
                self.focus = window;
                // Another window took focus while we are open: the user moved on.
                if moved && self.visible && self.shown_at.elapsed() > FOCUS_GRACE {
                    self.hide(rt);
                }
            }
            IpcMsg::Reply { id, ok, message } => {
                if let Some(app) = self.pending_spawns.remove(&id)
                    && !ok
                {
                    // Not retried directly: a denial (locked session) must stay a denial.
                    tracing::warn!("launcher: compositor refused to spawn {app}: {message}");
                }
            }
        }
    }

    fn apply_theme(&mut self, rt: &mut Runtime<Self>, snap: ThemeSnapshot) {
        if self.theme_rev.is_some_and(|r| snap.rev <= r) {
            return;
        }
        self.theme_rev = Some(snap.rev);
        let look = Look::from_theme(&snap.theme);
        if look == self.look {
            return;
        }
        self.look = look;
        let Some(sid) = self.surface else { return };
        let rows = self.rows();
        let tree = view::build(
            &self.look,
            &rows,
            self.selected,
            &self.query,
            !self.query.is_empty(),
        );
        let top = rt.size(sid).map_or(0.0, |s| top_padding(s.h));
        if let Some(ui) = rt.ui(sid) {
            *ui.root_mut() = tree;
            set_top_padding(ui, top);
        }
    }

    fn configured(&mut self, rt: &mut Runtime<Self>, sid: SurfaceId, height: f32) {
        if let Some(ui) = rt.ui(sid) {
            set_top_padding(ui, top_padding(height));
        }
        if !self.started {
            self.started = true;
            if !self.visible {
                // First configure: keep the surface warm but unmapped.
                rt.hide(sid);
            }
        }
    }
}

fn top_padding(height: f32) -> f32 {
    (height * 0.18).round()
}

fn set_top_padding(ui: &mut Ui, top: f32) {
    ui.edit(Id(ROOT), |n| {
        n.style.padding = Insets {
            top,
            ..Insets::default()
        }
    });
}

impl App for Launcher {
    fn event(&mut self, rt: &mut Runtime<Self>, event: Event) {
        match event {
            Event::Configured { surface, size } => self.configured(rt, surface, size.h),
            Event::Closed { surface } => {
                if Some(surface) == self.surface {
                    tracing::warn!("launcher: the compositor closed the surface");
                    self.visible = false;
                    rt.quit();
                }
            }
            Event::Ui { event, .. } => self.ui_event(rt, event),
            _ => {}
        }
    }
}

impl Launcher {
    fn ui_event(&mut self, rt: &mut Runtime<Self>, event: UiEvent) {
        if !self.visible {
            return;
        }
        match event {
            UiEvent::TextChanged(id) if id == Id(INPUT) => {
                let text = self
                    .surface
                    .and_then(|s| rt.ui(s))
                    .and_then(|ui| ui.text_of(Id(INPUT)).map(str::to_string))
                    .unwrap_or_default();
                self.query = text;
                self.refresh(rt);
            }
            UiEvent::Submitted(id) if id == Id(INPUT) => self.launch_row(rt, self.selected),
            UiEvent::Clicked(id) if id == Id(ROOT) => self.hide(rt),
            UiEvent::Clicked(Id(n)) if n >= ROW_BASE => {
                self.launch_row(rt, (n - ROW_BASE) as usize)
            }
            UiEvent::Key(k) => match key_action(&k) {
                Some(KeyAction::Hide) => self.hide(rt),
                Some(KeyAction::Next) => self.select(rt, 1),
                Some(KeyAction::Prev) => self.select(rt, -1),
                Some(KeyAction::Launch) => self.launch_row(rt, self.selected),
                None => {}
            },
            _ => {}
        }
    }
}

/// Errors that stop the daemon from starting.
#[derive(Debug)]
pub enum RunError {
    Ui(aurora_ui::runtime::Error),
    Control(std::io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Ui(e) => write!(f, "{e}"),
            RunError::Control(e) => write!(f, "control socket: {e}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Runs the daemon until the compositor closes its surface. Starts hidden.
pub fn run() -> Result<(), RunError> {
    let (tx, rx): (Sender<Msg>, Channel<Msg>) = channel::channel();

    // Claim the control socket first: a second daemon must fail before doing any work.
    let ctl_tx = tx.clone();
    let sock = control::listen(move |cmd| {
        let _ = ctl_tx.send(Msg::Control(cmd));
    })
    .map_err(RunError::Control)?;
    tracing::info!("launcher: control socket {}", sock.display());

    let mut launcher = Launcher::new(&tx);
    launcher.index = system::scan_system();

    let mut client = Client::connect(TextSystem::new(), launcher).map_err(RunError::Ui)?;
    let look = client.app().look.clone();
    let ui = Ui::new(
        client.runtime().text().clone(),
        view::build(&look, &[], 0, "", false),
    );
    let surface = client
        .runtime()
        .create_layer(
            LayerConfig {
                layer: Layer::Overlay,
                namespace: "aurora-launcher".into(),
                anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                size: (0, 0),
                exclusive_zone: -1,
                keyboard: KeyboardInteractivity::Exclusive,
                ..LayerConfig::default()
            },
            ui,
        )
        .map_err(RunError::Ui)?;
    client.app().surface = Some(surface);

    client
        .handle()
        .insert_source(rx, |event, _, state| {
            if let channel::Event::Msg(msg) = event {
                state.app.on_msg(&mut state.rt, msg);
            }
        })
        .map_err(|e| RunError::Ui(aurora_ui::runtime::Error::Loop(e.to_string())))?;

    // Rescan on directory changes, off the loop thread.
    let scan_tx = tx.clone();
    watch::start(system::dirs(), watch::DEBOUNCE, move || {
        let _ = scan_tx.send(Msg::Index(system::scan_system()));
    });

    // Warm the icons of the default results.
    {
        let app = client.app();
        let hits = app.index.search("", &app.frecency, now_secs(), PREWARM);
        for h in hits {
            if let Some(name) = app.index.get(h.app).and_then(|a| a.icon.clone())
                && app.icons.should_request(&name)
            {
                app.loader.request(name);
            }
        }
        tracing::info!("launcher: ready apps={}", app.index.len());
    }

    client.run().map_err(RunError::Ui)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_ui::Mods;

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::typed(c).with_mods(Mods {
            ctrl: true,
            ..Mods::default()
        })
    }

    #[test]
    fn keys_map_to_actions() {
        assert_eq!(
            key_action(&KeyEvent::new(Key::Escape)),
            Some(KeyAction::Hide)
        );
        assert_eq!(key_action(&KeyEvent::new(Key::Down)), Some(KeyAction::Next));
        assert_eq!(key_action(&KeyEvent::new(Key::Up)), Some(KeyAction::Prev));
        assert_eq!(
            key_action(&KeyEvent::new(Key::Enter)),
            Some(KeyAction::Launch)
        );
        assert_eq!(key_action(&ctrl('n')), Some(KeyAction::Next));
        assert_eq!(key_action(&ctrl('P')), Some(KeyAction::Prev));
        assert_eq!(key_action(&KeyEvent::typed('n')), None);
        assert_eq!(key_action(&ctrl('x')), None);
        let alt = KeyEvent::typed('n').with_mods(Mods {
            ctrl: true,
            alt: true,
            ..Mods::default()
        });
        assert_eq!(key_action(&alt), None);
    }

    #[test]
    fn selection_wraps_both_ways() {
        assert_eq!(step(0, 5, 1), 1);
        assert_eq!(step(4, 5, 1), 0);
        assert_eq!(step(0, 5, -1), 4);
        assert_eq!(step(2, 1, 1), 0);
        assert_eq!(step(0, 0, 1), 0);
        assert_eq!(step(3, 5, -8), 0);
    }

    #[test]
    fn commands_drive_visibility() {
        assert!(visibility_after(false, Command::Toggle));
        assert!(!visibility_after(true, Command::Toggle));
        assert!(visibility_after(true, Command::Show));
        assert!(!visibility_after(false, Command::Hide));
    }

    #[test]
    fn overlay_padding_scales_with_the_output() {
        assert_eq!(top_padding(1000.0), 180.0);
        assert_eq!(top_padding(0.0), 0.0);
    }
}
