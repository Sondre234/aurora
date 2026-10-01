//! The terminal as an `aurora_ui` app: one xdg-toplevel with a canvas, the pty, the
//! compositor IPC (theme only) and the timers, all on one calloop loop.
//!
//! Data flow: pty output is read in chunks of at most [`MAX_READ`] per wakeup, parsed by
//! the [`Backend`], and the emulator's line damage becomes damage on the canvas. The
//! runtime paints at most once per frame callback, so a flood of output costs one repaint
//! per frame, and an idle terminal wakes up for nothing (the cursor blink timer only
//! runs while a blinking cursor is visible and the window is focused). Input arrives as
//! raw toolkit events and leaves as bytes in an [`Outbox`] drained on writable wakeups.

mod events;
mod input;

pub use events::start;

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use aurora_theme::Theme;
use aurora_ui::runtime::calloop::generic::Generic;
use aurora_ui::runtime::calloop::timer::{TimeoutAction, Timer};
use aurora_ui::runtime::calloop::{Interest, Mode, PostAction};
use aurora_ui::runtime::{CursorIcon, Runtime, State, SurfaceId, ToplevelConfig};
use aurora_ui::{Dim, Node, Rect, TextSystem, Ui};

use crate::backend::Notice;
use crate::cli::Cli;
use crate::colors::ColorRef;
use crate::grid::{LineAccum, PADDING};
use crate::ipc::{Backoff, Conn};
use crate::mouse::{self, Reporting};
use crate::pty::{self, Outbox, Pty};
use crate::render::View;
use crate::select::{ClickTracker, ModTracker};

/// Bytes parsed per pty wakeup, so output floods never starve input or painting.
pub const MAX_READ: usize = 64 * 1024;
/// Most bytes drained after the child exited (what its pty buffer can still hold).
const MAX_DRAIN: usize = 1 << 20;
const BLINK: Duration = Duration::from_millis(530);
/// Smallest window, in cells.
const MIN_CELLS: (u32, u32) = (10, 3);
const INITIAL_CELLS: (u32, u32) = (80, 24);

pub struct TermApp {
    cli: Cli,
    view: Rc<RefCell<View>>,
    surface: Option<SurfaceId>,
    started: Instant,
    pty: Option<Pty>,
    outbox: Outbox,
    write_armed: bool,
    sync_armed: bool,
    blink_armed: bool,
    /// The first configure was handled (the child is started after it).
    configured_once: bool,
    /// The child ended (or never started); input goes nowhere.
    ended: bool,
    title: String,
    cursor_icon: CursorIcon,
    theme_rev: Option<u64>,
    conn: Option<Conn>,
    backoff: Backoff,
    warned_no_socket: bool,
    // Pointer state.
    clicks: ClickTracker,
    mods: ModTracker,
    wheel: LineAccum,
    selecting: bool,
    held: Option<mouse::Button>,
    last_motion_cell: Option<(usize, usize)>,
    /// Text we put on the clipboard; Wayland never offers a client its own selection.
    own_clipboard: Option<String>,
    own_primary: Option<String>,
    next_tag: u64,
    /// Set when the terminal cannot work at all; `main` exits nonzero.
    pub fatal: bool,
}

/// `theme.toml` next to the compositor's `config.toml`.
pub fn theme_path_from(
    xdg_config: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let non_empty = |v: Option<std::ffi::OsString>| v.filter(|v| !v.is_empty());
    let base = match non_empty(xdg_config) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(non_empty(home)?).join(".config"),
    };
    Some(base.join("aurora").join("theme.toml"))
}

/// The theme from `theme.toml`; defaults when missing, unreadable or broken.
pub fn load_theme() -> Theme {
    let path = theme_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    );
    match path.map(|p| Theme::load(&p)) {
        Some(Ok((theme, _warnings))) => theme,
        Some(Err(e)) => {
            tracing::warn!("term: theme not loaded: {e}");
            Theme::default()
        }
        None => Theme::default(),
    }
}

impl TermApp {
    pub fn new(cli: Cli, text: TextSystem) -> Self {
        let theme = load_theme();
        let mut view = View::new(text, &theme, (800.0, 500.0), 1.0, cli.scrollback);
        let (w, h) = Self::cells_size(&view, INITIAL_CELLS);
        view.relayout((w, h), 1.0);
        Self {
            title: cli.title.clone(),
            cli,
            view: Rc::new(RefCell::new(view)),
            surface: None,
            started: Instant::now(),
            pty: None,
            outbox: Outbox::default(),
            write_armed: false,
            sync_armed: false,
            blink_armed: false,
            configured_once: false,
            ended: false,
            cursor_icon: CursorIcon::Text,
            theme_rev: None,
            conn: None,
            backoff: Backoff::default(),
            warned_no_socket: false,
            clicks: ClickTracker::default(),
            mods: ModTracker::default(),
            wheel: LineAccum::default(),
            selecting: false,
            held: None,
            last_motion_cell: None,
            own_clipboard: None,
            own_primary: None,
            next_tag: 1,
            fatal: false,
        }
    }

    /// Logical window size for a grid of `cells` (columns, rows) including the padding.
    fn cells_size(view: &View, cells: (u32, u32)) -> (f32, f32) {
        let (w, h) = view.metrics.size_of(cells.0, cells.1);
        (w + 2.0 * PADDING, h + 2.0 * PADDING)
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    // ---- startup ----

    /// Create the window. Called once from the first loop iteration.
    fn open_window(&mut self, rt: &mut Runtime<Self>) {
        let (init, min) = {
            let v = self.view.borrow();
            (
                Self::cells_size(&v, INITIAL_CELLS),
                Self::cells_size(&v, MIN_CELLS),
            )
        };
        let view = self.view.clone();
        let canvas = Node::canvas(move |p, node, area| view.borrow().paint(p, node, area))
            .size(Dim::Fill(1.0), Dim::Fill(1.0));
        let ui = Ui::new(rt.text().clone(), canvas);
        let cfg = ToplevelConfig {
            title: self.cli.title.clone(),
            app_id: self.cli.class.clone(),
            size: (init.0 as u32, init.1 as u32),
            min_size: Some((min.0 as u32, min.1 as u32)),
            server_decorations: true,
            raw_input: true,
        };
        match rt.create_toplevel(cfg, ui) {
            Ok(id) => {
                rt.set_cursor(id, CursorIcon::Text);
                self.surface = Some(id);
            }
            Err(e) => {
                tracing::error!("term: cannot create the window: {e}");
                self.fatal = true;
                rt.quit();
            }
        }
    }

    /// The first configure fixed the size: announce, then start the child at that size.
    fn first_configure(&mut self, rt: &mut Runtime<Self>) {
        {
            let v = self.view.borrow();
            tracing::info!(
                "term: ready cols={} rows={} scale={} font={} cell={}x{}",
                v.geom.cols,
                v.geom.rows,
                v.metrics.scale,
                v.font_name(),
                v.geom.cell_w,
                v.geom.cell_h
            );
        }
        self.spawn_child(rt);
    }

    fn spawn_child(&mut self, rt: &mut Runtime<Self>) {
        let argv = pty::command_line(self.cli.command.as_deref(), std::env::var_os("SHELL"));
        let ws = {
            let g = self.view.borrow().geom;
            pty::winsize(g.cols, g.rows, g.cell_w, g.cell_h)
        };
        match Pty::spawn(&argv, self.cli.cwd.as_deref(), ws) {
            Ok(pty) => {
                tracing::info!("term: spawn pid={} cmd={}", pty.pid(), argv[0]);
                if let Err(e) = self.watch_pty(rt, &pty) {
                    tracing::error!("term: cannot watch the pty: {e}");
                    self.fatal = true;
                    rt.quit();
                }
                self.pty = Some(pty);
                #[cfg(feature = "qa-hooks")]
                self.test_input(rt);
            }
            Err(e) => {
                tracing::error!("term: cannot start {:?}: {e}", argv[0]);
                self.ended = true;
                let msg = format!("\x1b[31m[cannot start {}: {e}]\x1b[0m\r\n", argv[0]);
                self.view.borrow_mut().backend.feed(msg.as_bytes());
                self.flush_damage(rt);
                if !self.cli.hold {
                    self.fatal = true;
                    rt.quit();
                }
            }
        }
    }

    /// Register the pty output and the child's pidfd with the loop.
    fn watch_pty(&mut self, rt: &Runtime<Self>, pty: &Pty) -> io::Result<()> {
        let lh = rt.loop_handle().clone();
        let read = Generic::new(pty.dup_master()?, Interest::READ, Mode::Level);
        lh.insert_source(read, |_, _, state: &mut State<TermApp>| {
            let State { rt, app } = state;
            Ok(app.on_pty_readable(rt))
        })
        .map_err(|e| io::Error::other(e.to_string()))?;
        let pidfd = Generic::new(pty.pidfd()?, Interest::READ, Mode::Level);
        lh.insert_source(pidfd, |_, _, state: &mut State<TermApp>| {
            let State { rt, app } = state;
            app.on_child_exit(rt);
            Ok(PostAction::Remove)
        })
        .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(())
    }

    /// Feed the QA input file to the pty as if typed (`qa-hooks` builds only).
    #[cfg(feature = "qa-hooks")]
    fn test_input(&mut self, rt: &mut Runtime<Self>) {
        let Some(path) = std::env::var_os("AURORA_TERM_TEST_INPUT") else {
            return;
        };
        match std::fs::read(&path) {
            Ok(bytes) => {
                tracing::info!("term: test input bytes={}", bytes.len());
                self.send(rt, &bytes);
            }
            Err(e) => tracing::warn!("term: test input {path:?}: {e}"),
        }
    }

    // ---- pty ----

    fn on_pty_readable(&mut self, rt: &mut Runtime<Self>) -> PostAction {
        let (got, eof) = self.read_pty(MAX_READ);
        if got > 0 {
            self.after_output(rt);
        }
        if eof {
            PostAction::Remove
        } else {
            PostAction::Continue
        }
    }

    /// Read and parse up to `cap` bytes. Returns (bytes read, pty closed).
    fn read_pty(&mut self, cap: usize) -> (usize, bool) {
        let Some(pty) = &self.pty else {
            return (0, true);
        };
        let view = self.view.clone();
        let mut v = view.borrow_mut();
        let mut buf = [0u8; 16 * 1024];
        let mut total = 0;
        loop {
            match pty.read(&mut buf) {
                Ok(0) => return (total, true),
                Ok(n) => {
                    v.backend.feed(&buf[..n]);
                    total += n;
                    if total >= cap {
                        return (total, false);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return (total, false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // EIO: every slave end is closed, the child is gone.
                Err(_) => return (total, true),
            }
        }
    }

    /// After new output: answer the emulator's requests, repaint what changed.
    fn after_output(&mut self, rt: &mut Runtime<Self>) {
        self.process_notices(rt);
        self.flush_damage(rt);
        self.arm_sync(rt);
        self.refresh_modes(rt);
        self.update_blink(rt);
    }

    fn process_notices(&mut self, rt: &mut Runtime<Self>) {
        let notices = self.view.borrow_mut().backend.take_notices();
        for n in notices {
            match n {
                Notice::Title(t) => {
                    let title = t.unwrap_or_else(|| self.cli.title.clone());
                    tracing::info!("term: title {title}");
                    if let Some(id) = self.surface {
                        rt.set_title(id, &title);
                    }
                    self.title = title;
                }
                Notice::Clipboard { primary, text } => {
                    self.set_selection_text(rt, primary, text);
                }
                Notice::PtyWrite(bytes) => self.send(rt, &bytes),
                Notice::ColorRequest { index, reply } => {
                    let color = {
                        let v = self.view.borrow();
                        let over = |i: u16| v.backend.color_override(i);
                        v.scheme.resolve(ColorRef::Index(index), &over)
                    };
                    self.send(rt, reply(color).as_bytes());
                }
                Notice::CursorBlink => {}
            }
        }
    }

    /// Damage the canvas for whatever the emulator changed.
    fn flush_damage(&mut self, rt: &mut Runtime<Self>) {
        let rects = self.view.borrow_mut().damage_rects();
        if let Some(ui) = self.surface.and_then(|id| rt.ui(id)) {
            for r in rects {
                ui.damage(r);
            }
        }
    }

    fn damage_rect(&self, rt: &mut Runtime<Self>, r: Rect) {
        if let Some(ui) = self.surface.and_then(|id| rt.ui(id)) {
            ui.damage(r);
        }
    }

    fn damage_cursor(&self, rt: &mut Runtime<Self>) {
        let r = self.view.borrow().cursor_rect();
        if let Some(r) = r {
            self.damage_rect(rt, r);
        }
    }

    /// A synchronized update (DECSET 2026) holds output back until it ends or this
    /// timeout, so a program that never ends it cannot freeze the screen.
    fn arm_sync(&mut self, rt: &mut Runtime<Self>) {
        let Some(deadline) = self.view.borrow().backend.sync_deadline() else {
            return;
        };
        if std::mem::replace(&mut self.sync_armed, true) {
            return;
        }
        let timer = Timer::from_deadline(deadline);
        let lh = rt.loop_handle().clone();
        let _ = lh.insert_source(timer, |_, _, state: &mut State<TermApp>| {
            let State { rt, app } = state;
            app.sync_armed = false;
            app.view.borrow_mut().backend.end_sync();
            app.after_output(rt);
            TimeoutAction::Drop
        });
    }

    /// Mouse reporting decides the pointer shape.
    fn refresh_modes(&mut self, rt: &mut Runtime<Self>) {
        let reporting = self.view.borrow().backend.modes().reporting;
        let icon = if reporting == Reporting::Off {
            CursorIcon::Text
        } else {
            CursorIcon::Default
        };
        if icon != self.cursor_icon {
            self.cursor_icon = icon;
            if let Some(id) = self.surface {
                rt.set_cursor(id, icon);
            }
        }
    }

    fn want_blink(&self) -> bool {
        let v = self.view.borrow();
        v.focused && v.backend.cursor().is_some_and(|c| c.blinking)
    }

    /// Run the blink timer while a blinking cursor is visible in a focused window.
    fn update_blink(&mut self, rt: &mut Runtime<Self>) {
        if !self.want_blink() || self.blink_armed {
            return;
        }
        self.blink_armed = true;
        let lh = rt.loop_handle().clone();
        let _ = lh.insert_source(
            Timer::from_duration(BLINK),
            |_, _, state: &mut State<TermApp>| {
                let State { rt, app } = state;
                if app.want_blink() {
                    {
                        let mut v = app.view.borrow_mut();
                        v.cursor_on = !v.cursor_on;
                    }
                    app.damage_cursor(rt);
                    TimeoutAction::ToDuration(BLINK)
                } else {
                    app.blink_armed = false;
                    app.view.borrow_mut().cursor_on = true;
                    app.damage_cursor(rt);
                    TimeoutAction::Drop
                }
            },
        );
    }

    /// Show the cursor solid again (typing, focus) and restart its phase.
    fn cursor_solid(&mut self, rt: &mut Runtime<Self>) {
        let was_off = std::mem::replace(&mut self.view.borrow_mut().cursor_on, true);
        if !was_off {
            return;
        }
        self.damage_cursor(rt);
    }

    fn on_child_exit(&mut self, rt: &mut Runtime<Self>) {
        // Whatever the child wrote last is still in the pty buffer.
        let _ = self.read_pty(MAX_DRAIN);
        let Some(pty) = self.pty.as_mut() else { return };
        let pid = pty.pid();
        let status = match pty.try_wait() {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("term: wait failed: {e}");
                return;
            }
        };
        let text = pty::exit_text(&status);
        tracing::info!("term: exit pid={pid} {text}");
        self.pty = None;
        self.outbox = Outbox::default();
        self.ended = true;
        if self.cli.hold {
            let msg = format!("\r\n\x1b[2m[process exited: {text}]\x1b[0m\r\n");
            self.view.borrow_mut().backend.feed(msg.as_bytes());
            self.after_output(rt);
        } else {
            self.after_output(rt);
            rt.quit();
        }
    }

    /// Queue bytes for the child and write what the pty takes right away.
    fn send(&mut self, rt: &mut Runtime<Self>, bytes: &[u8]) {
        if self.pty.is_none() || bytes.is_empty() {
            return;
        }
        self.outbox.push(bytes);
        self.flush_outbox(rt);
    }

    fn flush_outbox(&mut self, rt: &mut Runtime<Self>) {
        let Some(pty) = &self.pty else { return };
        match self.outbox.flush(|b| pty.write(b)) {
            Ok(true) => {}
            Ok(false) => self.arm_writer(rt),
            Err(e) => {
                tracing::debug!("term: pty write: {e}");
                self.outbox = Outbox::default();
            }
        }
    }

    fn arm_writer(&mut self, rt: &mut Runtime<Self>) {
        if self.write_armed {
            return;
        }
        let Some(fd) = self.pty.as_ref().and_then(|p| p.dup_master().ok()) else {
            return;
        };
        self.write_armed = true;
        let lh = rt.loop_handle().clone();
        let src = Generic::new(fd, Interest::WRITE, Mode::Level);
        let res = lh.insert_source(src, |_, _, state: &mut State<TermApp>| {
            let State { rt, app } = state;
            Ok(app.on_writable(rt))
        });
        if res.is_err() {
            self.write_armed = false;
        }
    }

    fn on_writable(&mut self, rt: &mut Runtime<Self>) -> PostAction {
        self.flush_outbox(rt);
        // `flush_outbox` re-arms only through `arm_writer`, which is a no-op while armed.
        if self.outbox.is_empty() || self.pty.is_none() {
            self.write_armed = false;
            PostAction::Remove
        } else {
            PostAction::Continue
        }
    }
}
