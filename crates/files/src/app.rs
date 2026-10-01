//! The window: one xdg-toplevel whose whole surface is a canvas painted from a [`Scene`].
//! Worker threads (directory reads, the watcher, mount changes, icons, IPC, file operations)
//! reach the loop through one calloop channel, so the loop thread only updates state and
//! paints. Input handling lives in [`crate::input`], file actions in [`crate::actions`].

use std::cell::{RefCell, RefMut};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use aurora_ipc::ThemeSnapshot;
use aurora_launcher::icons::{IconLoader, Loaded, default_theme};
use aurora_ui::runtime::calloop::channel::{self, Channel, Sender};
use aurora_ui::runtime::calloop::timer::{TimeoutAction, Timer};
use aurora_ui::runtime::{App, Client, Event, Runtime, State, SurfaceId, ToplevelConfig};
use aurora_ui::{Dim, Node, Rect, Size, TextSystem, Ui};

use crate::actions::Running;
use crate::edit::LineEdit;
use crate::ipc::{IpcLink, IpcMsg};
use crate::listing::{INLINE_SORT_LIMIT, Job, Listed, Worker};
use crate::model::{Sort, SortKey, sort_entries};
use crate::ops::{Env, Op, Outcome, Progress, Undo};
use crate::places::{self, Mount, build_places, parse_mountinfo, parse_user_dirs};
use crate::scene::{Editing, Scene, Toast, icon_names};
use crate::system;
use crate::uri::ClipOp;
use crate::view::Look;

pub const APP_ID: &str = "aurora-files";
/// Byte budget of the pre-rasterized icon cache (a 40 px RGBA icon is ~6 KiB).
pub const ICON_BUDGET: usize = 8 << 20;
const TOAST_FOR: Duration = Duration::from_secs(4);
const ERROR_TOAST_FOR: Duration = Duration::from_secs(8);
/// How long a window being closed waits for a running operation to stop.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// Everything worker threads tell the loop.
pub enum Msg {
    Listed(Listed),
    /// The watched directory changed (debounced).
    DirChanged,
    /// New `/proc/self/mountinfo` text.
    Mounts(String),
    Icon(Loaded),
    Ipc(IpcMsg),
    Op(OpMsg),
}

/// A file operation worker's reports.
pub enum OpMsg {
    Progress { id: u64, progress: Progress },
    Conflict { id: u64, src: PathBuf, dst: PathBuf },
    Done { id: u64, outcome: Outcome },
}

/// What the open modal will do with its answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModalKind {
    #[default]
    None,
    /// Permanent delete awaiting confirmation.
    Confirm(Op),
    /// A running operation waits for a conflict answer.
    Conflict,
}

/// The clipboard as this window knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub op: ClipOp,
    pub paths: Vec<PathBuf>,
}

pub struct Files {
    pub scene: Rc<RefCell<Scene>>,
    pub surface: Option<SurfaceId>,
    pub tx: Sender<Msg>,
    pub worker: Worker,
    pub watcher: crate::watch::DirWatcher,
    pub ipc: IpcLink,
    pub loader: IconLoader,
    pub loader_scale: f32,
    pub theme_rev: Option<u64>,
    pub generation: u64,
    /// The newest listing request nobody answered yet.
    pub awaiting: Option<u64>,
    /// The directory changed while a read was in flight: read again when it lands.
    pub stale: bool,
    pub sorting: bool,
    pub requested_sort: Sort,
    pub announce: bool,
    pub ready_logged: bool,
    pub pending_select: Option<std::ffi::OsString>,
    pub home: Option<PathBuf>,
    pub user_dirs: Vec<(String, PathBuf)>,
    pub mounts: Vec<Mount>,
    pub clip: Option<Clip>,
    pub owns_clip: bool,
    pub env: Env,
    pub next_op: u64,
    pub running: Option<Running>,
    pub queue: VecDeque<Op>,
    pub modal_kind: ModalKind,
    pub last_undo: Option<Undo>,
    pub typeahead: String,
    pub typeahead_at: Instant,
    pub last_click: Option<(Instant, usize)>,
    pub pending_spawns: HashMap<u64, String>,
    pub toast_gen: u64,
    pub closing: bool,
    pub last_selected_logged: usize,
    pub last_activity: Instant,
    #[cfg(feature = "qa-hooks")]
    pub script: crate::testscript::Script,
}

impl Files {
    fn new(tx: &Sender<Msg>, start: PathBuf, text: TextSystem) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        let user_dirs = home
            .as_deref()
            .map(|h| {
                let base = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .unwrap_or_else(|| h.join(".config"));
                std::fs::read_to_string(base.join("user-dirs.dirs"))
                    .map(|t| parse_user_dirs(&t, h))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let look = Look::from_theme(&system::load_theme());
        let mut scene = Scene::new(look, text, start, ICON_BUDGET);
        scene.focused = false;

        let list_tx = tx.clone();
        let worker = Worker::spawn(move |l| {
            let _ = list_tx.send(Msg::Listed(l));
        });
        let watch_tx = tx.clone();
        let watcher = crate::watch::DirWatcher::start(move || {
            let _ = watch_tx.send(Msg::DirChanged);
        });
        let ipc_tx = tx.clone();
        let ipc = IpcLink::start(move |m| {
            let _ = ipc_tx.send(Msg::Ipc(m));
        });
        let mounts_tx = tx.clone();
        places::watch_mounts(move |text| {
            let _ = mounts_tx.send(Msg::Mounts(text));
        });
        let loader = Self::make_loader(tx, 1.0, &scene);

        let mut files = Self {
            scene: Rc::new(RefCell::new(scene)),
            surface: None,
            tx: tx.clone(),
            worker,
            watcher,
            ipc,
            loader,
            loader_scale: 1.0,
            theme_rev: None,
            generation: 0,
            awaiting: None,
            stale: false,
            sorting: false,
            requested_sort: Sort::default(),
            announce: false,
            ready_logged: false,
            pending_select: None,
            home,
            user_dirs,
            mounts: Vec::new(),
            clip: None,
            owns_clip: false,
            env: Env::from_process(),
            next_op: 1,
            running: None,
            queue: VecDeque::new(),
            modal_kind: ModalKind::None,
            last_undo: None,
            typeahead: String::new(),
            typeahead_at: Instant::now(),
            last_click: None,
            pending_spawns: HashMap::new(),
            toast_gen: 0,
            closing: false,
            last_selected_logged: 0,
            last_activity: Instant::now(),
            #[cfg(feature = "qa-hooks")]
            script: crate::testscript::Script::from_env(),
        };
        files.rebuild_places();
        files
    }

    fn make_loader(tx: &Sender<Msg>, scale: f32, scene: &Scene) -> IconLoader {
        let px = (scene.metrics().icon * scale).ceil().max(8.0) as u32;
        let icon_tx = tx.clone();
        IconLoader::spawn(default_theme(), px, move |l| {
            let _ = icon_tx.send(Msg::Icon(l));
        })
    }

    pub fn scene(&self) -> RefMut<'_, Scene> {
        self.scene.borrow_mut()
    }

    // ---- damage ----

    pub fn damage(&self, rt: &mut Runtime<Self>, r: Rect) {
        if let Some(ui) = self.surface.and_then(|s| rt.ui(s)) {
            ui.damage(r);
        }
    }

    pub fn damage_all(&self, rt: &mut Runtime<Self>) {
        let size = self.scene.borrow().size;
        self.damage(rt, Rect::new(0.0, 0.0, size.w, size.h));
    }

    pub fn damage_list(&self, rt: &mut Runtime<Self>) {
        let r = self.scene.borrow().metrics().list();
        self.damage(rt, r);
    }

    pub fn damage_status(&self, rt: &mut Runtime<Self>) {
        let r = self.scene.borrow().metrics().status();
        self.damage(rt, r);
    }

    pub fn damage_toolbar(&self, rt: &mut Runtime<Self>) {
        let r = self.scene.borrow().metrics().toolbar();
        self.damage(rt, r);
    }

    // ---- places and icons ----

    pub fn rebuild_places(&mut self) {
        let trash_files = self.env.trash_root.as_ref().map(|r| r.join("files"));
        let places = build_places(
            self.home.as_deref(),
            &self.user_dirs,
            &self.mounts,
            trash_files.as_deref(),
            &|p| p.exists(),
        );
        self.scene.borrow_mut().places = places;
        self.request_icons();
    }

    pub fn request_icons(&mut self) {
        let names = {
            let s = self.scene.borrow();
            icon_names(&s.browser.entries, &s.places)
        };
        let mut s = self.scene.borrow_mut();
        for n in names {
            if s.icons.should_request(n) {
                self.loader.request(n.to_string());
            }
        }
    }

    fn set_icon_scale(&mut self, scale: f32) {
        if (scale - self.loader_scale).abs() < f32::EPSILON {
            return;
        }
        self.loader_scale = scale;
        {
            let s = self.scene.borrow();
            self.loader = Self::make_loader(&self.tx, scale, &s);
        }
        self.scene.borrow_mut().icons.clear();
        self.request_icons();
    }

    // ---- toasts ----

    pub fn toast(&mut self, rt: &mut Runtime<Self>, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error {
            tracing::warn!("files: {text}");
        }
        self.toast_gen += 1;
        let gen_id = self.toast_gen;
        self.scene.borrow_mut().toast = Some(Toast { text, error });
        self.damage_status(rt);
        let after = if error { ERROR_TOAST_FOR } else { TOAST_FOR };
        let _ = rt.loop_handle().insert_source(
            Timer::from_duration(after),
            move |_, _, state: &mut State<Files>| {
                state.app.expire_toast(&mut state.rt, gen_id);
                TimeoutAction::Drop
            },
        );
    }

    fn expire_toast(&mut self, rt: &mut Runtime<Self>, gen_id: u64) {
        if self.toast_gen == gen_id && self.scene.borrow_mut().toast.take().is_some() {
            self.damage_status(rt);
        }
    }

    // ---- listing and navigation ----

    /// Go to `path`, remembering where we were.
    pub fn navigate(&mut self, rt: &mut Runtime<Self>, path: PathBuf) {
        let moved = self.scene.borrow_mut().browser.history.visit(path);
        if moved {
            self.scene.borrow_mut().editing = Editing::None;
            self.load(rt, true, true);
        }
    }

    pub fn go_back(&mut self, rt: &mut Runtime<Self>) {
        let went = self.scene.borrow_mut().browser.history.back().is_some();
        if went {
            self.load(rt, true, true);
        }
    }

    pub fn go_forward(&mut self, rt: &mut Runtime<Self>) {
        let went = self.scene.borrow_mut().browser.history.forward().is_some();
        if went {
            self.load(rt, true, true);
        }
    }

    pub fn go_up(&mut self, rt: &mut Runtime<Self>) {
        let parent = self
            .scene
            .borrow()
            .browser
            .path()
            .parent()
            .map(|p| p.to_path_buf());
        if let Some(p) = parent {
            self.navigate(rt, p);
        }
    }

    /// (Re)reads the current directory. `reset` clears the view first (a navigation);
    /// without it the old listing stays until the new one arrives (a refresh).
    pub fn load(&mut self, rt: &mut Runtime<Self>, reset: bool, announce: bool) {
        self.generation += 1;
        self.sorting = false;
        let (path, sort) = {
            let mut s = self.scene.borrow_mut();
            if reset {
                s.browser.navigate_reset();
                s.scroll = 0.0;
                s.loading = true;
            }
            (s.browser.path().to_path_buf(), s.browser.sort)
        };
        self.requested_sort = sort;
        self.awaiting = Some(self.generation);
        self.announce |= announce;
        self.worker.submit(Job::Load {
            generation: self.generation,
            path: path.clone(),
            sort,
        });
        self.watcher.watch(path.clone());
        if let Some(sid) = self.surface {
            rt.set_title(sid, &path.display().to_string());
        }
        self.damage_all(rt);
    }

    pub fn refresh(&mut self, rt: &mut Runtime<Self>) {
        self.load(rt, false, false);
    }

    fn on_listed(&mut self, rt: &mut Runtime<Self>, l: Listed) {
        if l.generation != self.generation {
            return; // for a directory (or sort) we have already left
        }
        self.awaiting = None;
        self.sorting = false;
        self.last_activity = Instant::now();
        let is_load = !l.path.as_os_str().is_empty();
        {
            let mut s = self.scene.borrow_mut();
            match l.result {
                Ok(mut entries) => {
                    if is_load && self.requested_sort != s.browser.sort {
                        sort_entries(&mut entries, s.browser.sort);
                    }
                    s.browser.set_entries(entries);
                }
                Err(e) => s.browser.set_error(e),
            }
            s.loading = false;
            let m = s.metrics();
            let rows = s.browser.len();
            s.scroll = m.clamp_scroll(s.scroll, rows);
        }
        if let Some(name) = self.pending_select.take() {
            let mut s = self.scene.borrow_mut();
            if let Some(row) = s.browser.row_of(&name) {
                s.browser.go_to_row(row, false);
                drop(s);
                self.ensure_cursor_visible();
            }
        }
        self.request_icons();
        let (path, total) = {
            let s = self.scene.borrow();
            (
                s.browser.path().display().to_string(),
                s.browser.entries.len(),
            )
        };
        if !self.ready_logged {
            self.ready_logged = true;
            self.announce = false;
            tracing::info!("files: ready path={path} entries={total}");
        } else if std::mem::take(&mut self.announce) {
            tracing::info!("files: navigate path={path} entries={total}");
        }
        self.log_selection();
        self.damage_all(rt);
        if std::mem::take(&mut self.stale) {
            self.refresh(rt);
        }
    }

    /// Re-sorts after a header click or Ctrl+1..4. Big directories go to the worker.
    pub fn set_sort(&mut self, rt: &mut Runtime<Self>, key: SortKey) {
        if self.sorting || self.awaiting.is_some() {
            return;
        }
        let new = self.scene.borrow().browser.sort.choose(key);
        let big = self.scene.borrow().browser.entries.len() > INLINE_SORT_LIMIT;
        if big {
            self.generation += 1;
            self.sorting = true;
            self.awaiting = Some(self.generation);
            self.requested_sort = new;
            let entries = {
                let mut s = self.scene.borrow_mut();
                s.browser.sort = new;
                s.loading = true;
                s.browser.visible.clear();
                std::mem::take(&mut s.browser.entries)
            };
            self.worker.submit(Job::Sort {
                generation: self.generation,
                entries,
                sort: new,
            });
        } else {
            self.scene.borrow_mut().browser.resort(new);
            self.ensure_cursor_visible();
        }
        self.damage_all(rt);
    }

    pub fn ensure_cursor_visible(&self) {
        let mut s = self.scene.borrow_mut();
        let rows = s.browser.len();
        if let Some(c) = s.browser.selection.cursor() {
            let m = s.metrics();
            s.scroll = m.scroll_to_row(c, s.scroll, rows);
        }
    }

    pub fn log_selection(&mut self) {
        let n = self.scene.borrow().browser.selection.count();
        if n != self.last_selected_logged {
            self.last_selected_logged = n;
            tracing::info!("files: select count={n}");
        }
    }

    // ---- theme and IPC ----

    fn apply_theme(&mut self, rt: &mut Runtime<Self>, snap: ThemeSnapshot) {
        if self.theme_rev.is_some_and(|r| snap.rev <= r) {
            return;
        }
        self.theme_rev = Some(snap.rev);
        let look = Look::from_theme(&snap.theme);
        tracing::info!("files: theme rev={}", snap.rev);
        let changed = {
            let mut s = self.scene.borrow_mut();
            let changed = s.look != look;
            s.look = look;
            changed
        };
        if changed {
            self.damage_all(rt);
        }
    }

    fn on_ipc(&mut self, rt: &mut Runtime<Self>, msg: IpcMsg) {
        match msg {
            IpcMsg::Connected => tracing::info!("files: ipc connected"),
            IpcMsg::Disconnected => self.pending_spawns.clear(),
            IpcMsg::Theme(snap) => self.apply_theme(rt, snap),
            IpcMsg::Reply { id, ok, message } => {
                if let Some(what) = self.pending_spawns.remove(&id)
                    && !ok
                {
                    self.toast(rt, format!("Cannot start {what}: {message}"), true);
                }
            }
        }
    }

    /// Starts a program through the compositor, or directly without IPC.
    pub fn spawn(&mut self, rt: &mut Runtime<Self>, argv: Vec<String>) {
        let what = argv.first().cloned().unwrap_or_default();
        match self.ipc.spawn(argv.clone()) {
            Some(id) => {
                self.pending_spawns.insert(id, what);
            }
            None => {
                if let Err(e) = system::spawn_direct(&argv) {
                    self.toast(rt, e, true);
                }
            }
        }
    }

    fn on_msg(&mut self, rt: &mut Runtime<Self>, msg: Msg) {
        match msg {
            Msg::Listed(l) => self.on_listed(rt, l),
            Msg::DirChanged => {
                if self.awaiting.is_none() {
                    self.refresh(rt);
                } else {
                    self.stale = true;
                }
            }
            Msg::Mounts(text) => {
                let mounts = parse_mountinfo(&text);
                if mounts != self.mounts {
                    self.mounts = mounts;
                    self.rebuild_places();
                    self.damage_all(rt);
                }
            }
            Msg::Icon((name, raw)) => {
                self.scene.borrow_mut().icons.store(name, raw);
                self.damage_all(rt);
            }
            Msg::Ipc(m) => self.on_ipc(rt, m),
            Msg::Op(m) => self.on_op_msg(rt, m),
        }
    }

    // ---- closing ----

    fn close(&mut self, rt: &mut Runtime<Self>) {
        if self.running.is_none() {
            rt.quit();
            return;
        }
        // Stop the operation first so it can clean up partial files, but never hang.
        self.closing = true;
        self.cancel_running();
        let _ = rt.loop_handle().insert_source(
            Timer::from_duration(CLOSE_GRACE),
            |_, _, state: &mut State<Files>| {
                state.rt.quit();
                TimeoutAction::Drop
            },
        );
    }

    pub fn typeahead_expired(&self) -> bool {
        self.typeahead_at.elapsed() > Duration::from_millis(1000)
    }

    pub fn new_name_default(&self, dir: bool) -> String {
        let s = self.scene.borrow();
        let base = if dir { "New folder" } else { "New file" };
        let taken = |n: &std::ffi::OsStr| s.browser.entries.iter().any(|e| e.name.as_os_str() == n);
        crate::names::first_free(
            |i| {
                if i == 0 {
                    base.into()
                } else {
                    format!("{base} {}", i + 1).into()
                }
            },
            taken,
        )
        .to_string_lossy()
        .into_owned()
    }

    pub fn start_new(&mut self, rt: &mut Runtime<Self>, dir: bool) {
        let name = self.new_name_default(dir);
        let mut edit = LineEdit::new(&name);
        edit.select_all();
        self.scene.borrow_mut().editing = Editing::New { dir, edit };
        self.damage_status(rt);
    }
}

impl App for Files {
    fn event(&mut self, rt: &mut Runtime<Self>, event: Event) {
        match event {
            Event::Configured { size, .. } => self.on_configured(rt, size),
            Event::ScaleChanged { surface, scale } => {
                if Some(surface) == self.surface {
                    self.set_icon_scale(scale);
                    self.damage_all(rt);
                }
            }
            Event::CloseRequested { .. } => self.close(rt),
            Event::Closed { .. } => rt.quit(),
            Event::KeyboardFocus { focused, .. } => {
                self.scene.borrow_mut().focused = focused;
                self.damage_list(rt);
            }
            Event::Input { input, .. } => self.on_input(rt, input),
            Event::SelectionData {
                tag, mime, data, ..
            } => self.on_selection_data(rt, tag, &mime, data),
            Event::SelectionLost { .. } => self.owns_clip = false,
            _ => {}
        }
    }
}

impl Files {
    fn on_configured(&mut self, rt: &mut Runtime<Self>, size: Size) {
        {
            let mut s = self.scene.borrow_mut();
            s.size = size;
            let m = s.metrics();
            let rows = s.browser.len();
            s.scroll = m.clamp_scroll(s.scroll, rows);
        }
        if let Some(scale) = self.surface.and_then(|sid| rt.scale(sid)) {
            self.set_icon_scale(scale);
        }
        self.damage_all(rt);
    }
}

/// Errors that stop the app from starting.
#[derive(Debug)]
pub enum RunError {
    Ui(aurora_ui::runtime::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Ui(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunError {}

/// The directory to open: argv[1] made absolute, else `$HOME`, else `/`.
pub fn start_dir(arg: Option<&str>, home: Option<PathBuf>) -> PathBuf {
    match arg {
        Some(a) => std::path::absolute(a).unwrap_or_else(|_| PathBuf::from(a)),
        None => home
            .filter(|h| h.is_dir())
            .unwrap_or_else(|| PathBuf::from("/")),
    }
}

/// Runs the window until it is closed.
pub fn run(start: PathBuf) -> Result<(), RunError> {
    let (tx, rx): (Sender<Msg>, Channel<Msg>) = channel::channel();
    let text = TextSystem::new();
    let files = Files::new(&tx, start, text.clone());
    let scene = files.scene.clone();
    let mut client = Client::connect(text, files).map_err(RunError::Ui)?;

    let paint_scene = scene.clone();
    let canvas = Node::canvas(move |p, _rect, area| paint_scene.borrow_mut().paint(p, area))
        .size(Dim::Fill(1.0), Dim::Fill(1.0));
    let root = Node::column(vec![canvas]).size(Dim::Fill(1.0), Dim::Fill(1.0));
    let ui = Ui::new(client.runtime().text().clone(), root);

    let path = scene.borrow().browser.path().to_path_buf();
    let surface = client
        .runtime()
        .create_toplevel(
            ToplevelConfig {
                title: path.display().to_string(),
                app_id: APP_ID.into(),
                size: (1000, 680),
                min_size: Some((360, 240)),
                server_decorations: true,
                raw_input: true,
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

    #[cfg(feature = "qa-hooks")]
    crate::testscript::install(&client.handle());

    // The first read starts once the loop exists; its answer arrives through the channel.
    {
        let app = client.app();
        app.generation += 1;
        app.awaiting = Some(app.generation);
        let sort = app.scene.borrow().browser.sort;
        app.requested_sort = sort;
        app.worker.submit(Job::Load {
            generation: app.generation,
            path: path.clone(),
            sort,
        });
        app.watcher.watch(path);
    }
    client.run().map_err(RunError::Ui)?;
    Ok(())
}
