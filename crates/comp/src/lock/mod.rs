//! Session lock (ext-session-lock): the compositor side. `Aurora.lock` is `Some` from the
//! moment a client asks to lock until the owner unlocks (or an unconfirmed attempt dies), and
//! a dead lock client never unlocks: the state stays `Locked` and every output shows black
//! until a new client takes over. The rules live in the pure `machine`; this module applies
//! them to Smithay and the scene.
//!
//! While `Aurora.lock` is `Some`:
//! - `scene::output_elements` draws only the output's lock surface (over opaque black) and
//!   nothing else, before any layer, window, blur or overview element;
//! - binds are refused (`binds_allowed`, `dispatch`), only the hardcoded emergency chords
//!   (quit, VT switch) still run, and no config can change that (they are classified before
//!   the config is consulted);
//! - keyboard focus is forced to a lock surface (or nothing), `hit_test` returns a lock
//!   surface or nothing, and focus, activation, drags, the overview and the layer-shell
//!   exclusive focus logic are guarded.
use std::sync::{
    Mutex, PoisonError,
    atomic::{AtomicBool, Ordering},
};

use smithay::{
    backend::renderer::element::solid::SolidColorBuffer,
    desktop::{WindowSurfaceType, utils::under_from_surface_tree},
    output::Output,
    reexports::{
        wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
        wayland_server::{
            Resource,
            protocol::{wl_output::WlOutput, wl_surface::WlSurface},
        },
    },
    utils::{IsAlive, Logical, Point, SERIAL_COUNTER, Size},
    wayland::session_lock::{LockSurface, SessionLocker},
};

use crate::{focus::FocusTarget, layers::Hit, state::Aurora, wm::apply::has_buffer};
use machine::{LockId, LockMachine, Lost, Verdict};

mod machine;

/// Whether the session is locked or locking. Process wide because the render paths build
/// scenes from free functions while the backend is borrowed, and a flag is set before any
/// frame after the request can be drawn (a new output included).
static ENGAGED: AtomicBool = AtomicBool::new(false);

pub fn engaged() -> bool {
    ENGAGED.load(Ordering::Relaxed)
}

/// One lock surface of the current owner.
struct Entry {
    output: Output,
    surface: LockSurface,
    /// Logical size last sent in a configure.
    size: Size<i32, Logical>,
}

#[derive(Default)]
pub struct LockState {
    machine: LockMachine,
    /// The owner's lock object, to notice its client going away.
    owner: Option<(LockId, ExtSessionLockV1)>,
    /// Held until every output has a committed lock surface; dropping it tells the client
    /// the lock failed.
    locker: Option<SessionLocker>,
    entries: Vec<Entry>,
}

/// What the render path reads per output, retained in the output's user data.
struct OutputLock(Mutex<OutputLockInner>);

struct OutputLockInner {
    surface: Option<LockSurface>,
    black: SolidColorBuffer,
}

const BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

fn with_output<T>(output: &Output, f: impl FnOnce(&mut OutputLockInner) -> T) -> T {
    let data = output.user_data().get_or_insert_threadsafe(|| {
        OutputLock(Mutex::new(OutputLockInner {
            surface: None,
            black: SolidColorBuffer::new((0, 0), BLACK),
        }))
    });
    f(&mut data.0.lock().unwrap_or_else(PoisonError::into_inner))
}

fn set_output_surface(output: &Output, surface: Option<LockSurface>) {
    with_output(output, |o| o.surface = surface);
}

/// The live lock surface of `output`, if it has one.
pub fn surface_of(output: &Output) -> Option<WlSurface> {
    with_output(output, |o| {
        o.surface
            .as_ref()
            .filter(|s| s.alive())
            .map(|s| s.wl_surface().clone())
    })
}

/// What the scene draws for a locked `output` of logical `size`: its lock surface, if any,
/// and the opaque black buffer that goes behind it.
pub fn view(output: &Output, size: Size<i32, Logical>) -> (Option<WlSurface>, SolidColorBuffer) {
    with_output(output, |o| {
        o.black.update(size, BLACK);
        let surface = o
            .surface
            .as_ref()
            .filter(|s| s.alive())
            .map(|s| s.wl_surface().clone());
        (surface, o.black.clone())
    })
}

fn to_u32(size: Size<i32, Logical>) -> Size<u32, Logical> {
    (size.w.max(1) as u32, size.h.max(1) as u32).into()
}

impl Aurora {
    /// The session is locked or locking: nothing but lock surfaces may be seen or reached.
    pub fn is_locked(&self) -> bool {
        self.lock.is_some()
    }

    fn output_names(&self) -> Vec<String> {
        self.wm.outputs.iter().map(Output::name).collect()
    }

    fn logical_size(&self, output: &Output) -> Option<Size<i32, Logical>> {
        self.space.output_geometry(output).map(|g| g.size)
    }

    /// A client asked to lock the session.
    pub fn lock_request(&mut self, locker: SessionLocker) {
        tracing::info!("lock: requested");
        // A client that died since the last loop turn must not keep a newcomer out.
        self.lock_update();
        let names = self.output_names();
        let state = self.lock.get_or_insert_with(LockState::default);
        let Verdict::Accepted(id) = state.machine.request(&names) else {
            tracing::info!("lock: denied, a session lock is already held");
            // Dropping `locker` tells the client `finished`.
            return;
        };
        state.owner = Some((id, locker.ext_session_lock().clone()));
        state.locker = Some(locker);
        state.entries.clear();
        for output in &self.wm.outputs {
            set_output_surface(output, None);
        }
        self.lock_engage();
        self.lock_update();
    }

    /// Everything that must stop the moment the session is engaged.
    fn lock_engage(&mut self) {
        ENGAGED.store(true, Ordering::Relaxed);
        self.overview = None;
        self.cancel_repeat();
        self.end_popup_grab_for(None);
        if self.pointer.is_grabbed() {
            let pointer = self.pointer.clone();
            pointer.unset_grab(
                self,
                SERIAL_COUNTER.next_serial(),
                smithay::backend::input::InputTime::now(),
            );
        }
        self.lock_focus();
        self.resend_pointer_focus();
        self.queue_redraw_all();
    }

    /// A lock surface was created for `wl_output`.
    pub fn lock_new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let output = Output::from_resource(&wl_output).filter(|o| self.wm.outputs.contains(o));
        let size = output
            .as_ref()
            .and_then(|o| self.logical_size(o))
            .unwrap_or_else(|| (1, 1).into());
        // Smithay sends the initial configure once this returns.
        surface.with_pending_state(|s| s.size = Some(to_u32(size)));
        let (Some(output), Some(state)) = (output, self.lock.as_mut()) else {
            return;
        };
        let owner = state
            .owner
            .as_ref()
            .filter(|(_, res)| res == surface.ext_session_lock())
            .map(|(id, _)| *id);
        let name = output.name();
        if !owner.is_some_and(|id| state.machine.surface_added(id, &name)) {
            tracing::info!("lock: surface refused output={name}");
            return;
        }
        state.entries.push(Entry {
            output: output.clone(),
            surface: surface.clone(),
            size,
        });
        set_output_surface(&output, Some(surface));
        tracing::info!("lock: surface output={name}");
    }

    /// The owner sent `unlock_and_destroy` (Smithay only calls this for the lock it confirmed).
    pub fn lock_unlock(&mut self) {
        let Some(state) = self.lock.as_mut() else {
            return;
        };
        if state.machine.unlock() {
            tracing::info!("lock: unlocked");
            self.lock_release();
        } else {
            tracing::warn!("lock: unlock ignored, no living owner");
        }
    }

    /// Back to the normal session: windows, layers and keyboard focus return.
    fn lock_release(&mut self) {
        self.lock = None;
        ENGAGED.store(false, Ordering::Relaxed);
        for output in &self.wm.outputs {
            set_output_surface(output, None);
        }
        self.restore_focus_after_lock();
        self.resend_pointer_focus();
        self.queue_redraw_all();
    }

    /// Runs on every loop turn while a lock exists (and after lock surface commits): notices
    /// the lock client dying, prunes dead surfaces, follows output changes, confirms the lock
    /// once every output is covered, and keeps the keyboard on a lock surface.
    pub fn lock_update(&mut self) {
        if self.lock.is_none() {
            return;
        }
        self.lock_check_owner();
        if self.lock.is_none() {
            return;
        }
        self.lock_prune_surfaces();
        self.lock_sync_outputs();
        self.lock_try_confirm();
        self.lock_focus();
    }

    fn lock_check_owner(&mut self) {
        let Some(state) = self.lock.as_mut() else {
            return;
        };
        let Some((id, res)) = state.owner.as_ref() else {
            return;
        };
        if res.is_alive() {
            return;
        }
        let id = *id;
        state.owner = None;
        state.locker = None;
        match state.machine.client_gone(id) {
            Lost::Cancelled => {
                tracing::info!("lock: client gone before locking, cancelled");
                self.lock_release();
            }
            Lost::Orphaned => {
                tracing::info!("lock: client gone, session stays locked");
                self.lock_forget_surfaces();
                self.queue_redraw_all();
            }
            Lost::Nothing => {}
        }
    }

    fn lock_forget_surfaces(&mut self) {
        let Some(state) = self.lock.as_mut() else {
            return;
        };
        for entry in state.entries.drain(..) {
            set_output_surface(&entry.output, None);
        }
    }

    fn lock_prune_surfaces(&mut self) {
        let Some(state) = self.lock.as_mut() else {
            return;
        };
        let mut gone = Vec::new();
        state.entries.retain(|e| {
            let alive = e.surface.alive();
            if !alive {
                gone.push(e.output.clone());
            }
            alive
        });
        for output in &gone {
            state.machine.surface_removed(&output.name());
            set_output_surface(output, None);
        }
        for output in &gone {
            self.queue_redraw_output(output);
        }
    }

    /// Output changes while locked: vanished outputs drop their surfaces, resized ones get
    /// a new configure. New outputs stay black until the client serves them.
    fn lock_sync_outputs(&mut self) {
        let names = self.output_names();
        let sizes: Vec<_> = self
            .lock
            .as_ref()
            .map(|s| {
                s.entries
                    .iter()
                    .map(|e| self.logical_size(&e.output))
                    .collect()
            })
            .unwrap_or_default();
        let Some(state) = self.lock.as_mut() else {
            return;
        };
        state.machine.set_outputs(&names);
        let mut resized = Vec::new();
        for (entry, size) in state.entries.iter_mut().zip(sizes) {
            if let Some(size) = size.filter(|s| *s != entry.size) {
                entry.size = size;
                entry
                    .surface
                    .with_pending_state(|s| s.size = Some(to_u32(size)));
                entry.surface.send_configure();
                resized.push(entry.output.clone());
            }
        }
        state.entries.retain(|e| names.contains(&e.output.name()));
        for output in resized {
            self.queue_redraw_output(&output);
        }
    }

    fn lock_try_confirm(&mut self) {
        let Some(state) = self.lock.as_mut() else {
            return;
        };
        if !state.machine.ready_to_confirm() {
            return;
        }
        let Some(locker) = state.locker.take() else {
            return;
        };
        state.machine.confirmed();
        // Every output has a lock surface with a buffer and draws nothing else.
        locker.lock();
        tracing::info!("lock: locked");
        self.queue_redraw_all();
    }

    fn is_lock_surface(&self, target: &FocusTarget) -> bool {
        let FocusTarget::Wl(surface) = target else {
            return false;
        };
        self.lock
            .as_ref()
            .is_some_and(|s| s.entries.iter().any(|e| e.surface.wl_surface() == surface))
    }

    /// The lock surface that should hold the keyboard: the one under the pointer, else the
    /// active output's, else any that has content.
    fn lock_focus_target(&self) -> Option<FocusTarget> {
        let state = self.lock.as_ref()?;
        let usable = |e: &&Entry| e.surface.alive() && has_buffer(e.surface.wl_surface());
        let on = |o: Option<&Output>| {
            state
                .entries
                .iter()
                .filter(usable)
                .find(|e| Some(&e.output) == o)
        };
        let under = self.output_at(self.pointer.current_location());
        let entry = on(under.as_ref())
            .or_else(|| on(self.wm.active_output.as_ref()))
            .or_else(|| state.entries.iter().find(usable))?;
        Some(FocusTarget::Wl(entry.surface.wl_surface().clone()))
    }

    /// Forces the keyboard onto a lock surface, or onto nothing while none has content. A
    /// lock surface that already holds it keeps it.
    fn lock_focus(&mut self) {
        let current = self.keyboard.current_focus();
        if current
            .as_ref()
            .is_some_and(|c| c.alive() && self.is_lock_surface(c))
        {
            return;
        }
        let want = self.lock_focus_target();
        if want.is_none() && current.is_none() {
            return;
        }
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, want, SERIAL_COUNTER.next_serial());
    }

    /// A click on a lock surface gives it the keyboard.
    pub fn lock_focus_surface(&mut self, surface: &WlSurface) {
        let Some(entry) = self
            .lock
            .as_ref()
            .and_then(|s| s.entries.iter().find(|e| e.surface.wl_surface() == surface))
        else {
            return;
        };
        let target = FocusTarget::Wl(entry.surface.wl_surface().clone());
        if self.keyboard.current_focus().as_ref() != Some(&target) {
            let keyboard = self.keyboard.clone();
            keyboard.set_focus(self, Some(target), SERIAL_COUNTER.next_serial());
        }
    }

    /// A commit of `root` if it is a lock surface. Returns whether it was one.
    pub fn lock_commit(&mut self, root: &WlSurface) -> bool {
        let Some(state) = self.lock.as_mut() else {
            return false;
        };
        let Some(output) = state
            .entries
            .iter()
            .find(|e| e.surface.wl_surface() == root)
            .map(|e| e.output.clone())
        else {
            return false;
        };
        if has_buffer(root) {
            state.machine.surface_committed(&output.name());
        }
        self.queue_redraw_output(&output);
        self.lock_update();
        true
    }

    /// Pointer hit while locked: the lock surface of the output under `pos`, or nothing.
    pub fn lock_hit(&self, pos: Point<f64, Logical>) -> Hit {
        let Some(output) = self.output_at(pos) else {
            return Hit::Nothing;
        };
        let (Some(surface), Some(geo)) = (surface_of(&output), self.space.output_geometry(&output))
        else {
            return Hit::Nothing;
        };
        let local = pos - geo.loc.to_f64();
        under_from_surface_tree(&surface, local, (0, 0), WindowSurfaceType::ALL).map_or(
            Hit::Nothing,
            |(surface, at)| Hit::Lock {
                surface,
                loc: (at + geo.loc).to_f64(),
            },
        )
    }

    /// `dump: lock state=unlocked|locking|locked surfaces=<n>`.
    pub fn dump_lock(&self) {
        let (state, surfaces) = self.lock.as_ref().map_or(("unlocked", 0), |s| {
            (s.machine.phase().name(), s.machine.surface_count())
        });
        tracing::info!("dump: lock state={state} surfaces={surfaces}");
    }
}
