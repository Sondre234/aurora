//! Window and theme events of [`TermApp`], the `App` impl and the IPC wiring.

use std::time::Duration;

use aurora_ipc::{Body, Event as IpcEvent, Response, ThemeSnapshot, socket_path};
use aurora_ui::Size;
use aurora_ui::runtime::calloop::generic::Generic;
use aurora_ui::runtime::calloop::timer::{TimeoutAction, Timer};
use aurora_ui::runtime::calloop::{Interest, Mode, PostAction};
use aurora_ui::runtime::{App, Event, Runtime, Selection, State, SurfaceId};

use super::TermApp;
use crate::ipc::Conn;
use crate::pty;

impl TermApp {
    pub(super) fn configured(&mut self, rt: &mut Runtime<Self>, id: SurfaceId, size: Size) {
        let scale = rt.scale(id).unwrap_or(1.0);
        let first = !std::mem::replace(&mut self.configured_once, true);
        let changed = self.view.borrow_mut().relayout((size.w, size.h), scale);
        // The first size is announced by `ready`, the child starts at it.
        if changed && !first {
            self.grid_changed();
        }
        self.flush_damage(rt);
        if first {
            self.first_configure(rt);
        }
    }

    fn scale_changed(&mut self, rt: &mut Runtime<Self>, id: SurfaceId, scale: f32) {
        let Some(size) = rt.size(id) else { return };
        if self.view.borrow_mut().relayout((size.w, size.h), scale) {
            self.grid_changed();
        }
        self.flush_damage(rt);
        self.repaint_all(rt);
    }

    fn repaint_all(&mut self, rt: &mut Runtime<Self>) {
        if let Some(id) = self.surface {
            rt.request_redraw(id);
        }
    }

    /// The grid has new dimensions: tell the kernel (SIGWINCH to the foreground job).
    fn grid_changed(&mut self) {
        let g = self.view.borrow().geom;
        tracing::info!("term: resize cols={} rows={}", g.cols, g.rows);
        if let Some(pty) = &self.pty
            && let Err(e) = pty.resize(pty::winsize(g.cols, g.rows, g.cell_w, g.cell_h))
        {
            tracing::warn!("term: cannot resize the pty: {e}");
        }
        self.last_motion_cell = None;
        self.clicks.reset();
    }

    /// Apply a theme from the compositor; stale or duplicate revisions are dropped.
    fn apply_theme(&mut self, rt: &mut Runtime<Self>, ts: ThemeSnapshot) {
        if self.theme_rev.is_some_and(|r| ts.rev <= r) {
            return;
        }
        self.theme_rev = Some(ts.rev);
        tracing::info!("term: theme rev={}", ts.rev);
        if self.view.borrow_mut().set_theme(&ts.theme) {
            self.grid_changed();
        }
        self.flush_damage(rt);
        self.repaint_all(rt);
    }

    fn focus_changed(&mut self, rt: &mut Runtime<Self>, focused: bool) {
        self.view.borrow_mut().focused = focused;
        if focused {
            self.cursor_solid(rt);
        }
        self.damage_cursor(rt);
        if self.view.borrow().backend.modes().focus_reporting {
            self.send(rt, if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
        self.update_blink(rt);
    }

    /// The window is going away: hang up the shell like closing a terminal does.
    fn close(&mut self, rt: &mut Runtime<Self>) {
        if let Some(pty) = self.pty.as_mut() {
            pty.hangup();
        }
        self.pty = None;
        rt.quit();
    }

    fn on_ipc_frame(&mut self, rt: &mut Runtime<Self>, body: Body) {
        match body {
            Body::Hello(h) => tracing::info!("term: ipc connected server={}", h.client),
            Body::Event(IpcEvent::Theme(ts)) | Body::Response(Response::Theme(ts)) => {
                self.apply_theme(rt, ts);
            }
            Body::Error(e) => tracing::warn!("term: ipc error {:?}: {}", e.code, e.message),
            _ => {}
        }
    }
}

impl App for TermApp {
    fn event(&mut self, rt: &mut Runtime<Self>, event: Event) {
        let mine = |s: SurfaceId, app: &TermApp| Some(s) == app.surface;
        match event {
            Event::Configured { surface, size } if mine(surface, self) => {
                self.configured(rt, surface, size);
            }
            Event::ScaleChanged { surface, scale } if mine(surface, self) => {
                self.scale_changed(rt, surface, scale);
            }
            Event::CloseRequested { surface } if mine(surface, self) => self.close(rt),
            Event::Closed { surface } if mine(surface, self) => self.close(rt),
            Event::KeyboardFocus { surface, focused } if mine(surface, self) => {
                self.focus_changed(rt, focused);
            }
            Event::Input { surface, input } if mine(surface, self) => {
                self.handle_input(rt, input);
            }
            Event::SelectionData {
                data: Some(data), ..
            } => {
                let text = String::from_utf8_lossy(&data).into_owned();
                self.paste_text(rt, &text);
            }
            Event::SelectionLost { selection } => match selection {
                Selection::Clipboard => self.own_clipboard = None,
                Selection::Primary => {
                    self.own_primary = None;
                    // Another client took the primary selection: drop the highlight.
                    self.selecting = false;
                    self.mutate_selection(rt, |b| {
                        b.select_clear();
                    });
                }
            },
            _ => {}
        }
    }
}

// ---- event loop glue ----

/// First loop iteration: the window, then the IPC link.
pub fn start(state: &mut State<TermApp>) {
    {
        let State { rt, app } = &mut *state;
        app.open_window(rt);
    }
    if !state.app.fatal && !try_connect(state) && ipc_expected() {
        schedule_reconnect(state);
    }
}

/// A compositor socket exists, so a failed connect is worth retrying. Without one the
/// terminal stays on `theme.toml` and an idle terminal has no timers at all.
fn ipc_expected() -> bool {
    socket_path().is_some_and(|p| p.exists())
}

fn try_connect(state: &mut State<TermApp>) -> bool {
    let Some(path) = socket_path() else {
        if !std::mem::replace(&mut state.app.warned_no_socket, true) {
            tracing::info!("term: no ipc socket configured, using theme.toml only");
        }
        return false;
    };
    let conn = match Conn::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("term: ipc connect {}: {e}", path.display());
            return false;
        }
    };
    let stream = match conn.stream() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("term: ipc socket clone: {e}");
            return false;
        }
    };
    let source = Generic::new(stream, Interest::READ, Mode::Level);
    let handle = state.rt.loop_handle().clone();
    if let Err(e) = handle.insert_source(source, |_, _, state| Ok(on_ipc_readable(state))) {
        tracing::warn!("term: cannot watch the ipc socket: {e}");
        return false;
    }
    state.app.conn = Some(conn);
    true
}

fn on_ipc_readable(state: &mut State<TermApp>) -> PostAction {
    let State { rt, app } = state;
    let Some(conn) = app.conn.as_mut() else {
        return PostAction::Remove;
    };
    match conn.read() {
        Ok(frames) => {
            app.backoff.reset();
            for f in frames {
                app.on_ipc_frame(rt, f.body);
            }
            PostAction::Continue
        }
        Err(e) => {
            tracing::warn!("term: ipc connection lost: {e}; reconnecting");
            app.conn = None;
            // A restarted compositor counts revisions from scratch again.
            app.theme_rev = None;
            schedule_reconnect(state);
            PostAction::Remove
        }
    }
}

fn schedule_reconnect(state: &mut State<TermApp>) {
    let delay: Duration = state.app.backoff.next_delay();
    let handle = state.rt.loop_handle().clone();
    let inserted = handle.insert_source(Timer::from_duration(delay), |_, _, state| {
        if try_connect(state) || !ipc_expected() {
            TimeoutAction::Drop
        } else {
            TimeoutAction::ToDuration(state.app.backoff.next_delay())
        }
    });
    if let Err(e) = inserted {
        tracing::error!("term: cannot schedule an ipc reconnect: {e}");
    }
}
