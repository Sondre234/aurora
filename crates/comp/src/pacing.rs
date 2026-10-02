//! Client frame pacing: fifo-v1 and commit-timing-v1, what Mesa's FIFO present mode and
//! Proton pace their frames with.
//!
//! fifo: Smithay (managed mode) holds back a commit that waits on a barrier. When an output
//! renders, the barriers of the updates that frame shows are latched (taken off the surfaces
//! into the output's list); when the frame is presented (the vblank of a flip, the estimated
//! vblank of an empty frame, the frame timer of the nested and headless outputs) they are
//! signalled. A FIFO client thus advances one content update per refresh, and never past an
//! update that has not been on screen. A surface shown on several outputs follows its primary
//! scanout output. Surfaces shown nowhere (a window on a hidden workspace) keep their barrier
//! until they are shown again, just as they get no frame callbacks meanwhile.
//!
//! commit-timing: unmanaged, so Aurora sees every deadline. The pre-commit hook registers a
//! barrier for the timestamp and arms a timer at it. Before an output renders, the barriers
//! due by that frame's predicted presentation are released so the update lands in the frame
//! meant for it; the timer releases whatever is still held at its deadline (an idle output
//! renders nothing, a hidden surface is never rendered). An update is never applied before
//! its timestamp's vblank, at worst one frame after it.
use std::{collections::HashMap, sync::Mutex, time::Duration};

use smithay::{
    desktop::{
        layer_map_for_output,
        utils::{surface_primary_scanout_output, with_surfaces_surface_tree},
    },
    input::pointer::CursorImageStatus,
    output::Output,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_server::{
            Client, Resource, Weak, backend::ClientId, protocol::wl_surface::WlSurface,
        },
    },
    utils::{Monotonic, Time},
    wayland::{
        commit_timing::{
            CommitTimerBarrierStateUserData, CommitTimerStateUserData, CommitTimingManagerState,
        },
        compositor::{
            Barrier, CompositorHandler, SurfaceData, add_blocker, add_pre_commit_hook, with_states,
        },
        fifo::{FifoBarrierCachedState, FifoManagerState},
    },
};

use crate::state::Aurora;

/// Keeps both globals alive.
pub struct Pacing {
    _fifo: FifoManagerState,
    _commit_timing: CommitTimingManagerState,
}

impl Pacing {
    pub fn new(dh: &smithay::reexports::wayland_server::DisplayHandle) -> Self {
        Self {
            _fifo: FifoManagerState::new::<Aurora>(dh),
            _commit_timing: CommitTimingManagerState::unmanaged::<Aurora>(dh),
        }
    }
}

/// When the frame rendered at `now` reaches the screen: the first vblank after `now` on the
/// grid of the last presentation, or `now` itself when there is no usable last presentation
/// (never later than the truth, so no update is released early by more than the guess).
pub fn predict_presentation(now: Duration, last: Option<Duration>, frame: Duration) -> Duration {
    match last {
        Some(last) if last <= now && !frame.is_zero() => {
            let since = (now - last).as_nanos();
            let frame_ns = frame.as_nanos();
            let n = since.div_ceil(frame_ns).max(1);
            last + Duration::from_nanos((n * frame_ns).min(u128::from(u64::MAX)) as u64)
        }
        _ => now,
    }
}

/// The commit-timing hook of one surface; installed for every surface at creation.
pub fn install_commit_timer_hook(surface: &WlSurface) {
    add_pre_commit_hook::<Aurora, _>(surface, |state, _dh, surface| {
        let timestamp = with_states(surface, |states| {
            states
                .data_map
                .get::<CommitTimerStateUserData>()
                .and_then(|t| t.borrow_mut().timestamp.take())
        });
        let Some(timestamp) = timestamp else { return };
        let due: Time<Monotonic> = timestamp.into();
        let delay = Time::elapsed(&state.clock.now(), due);
        // A deadline already passed holds nothing back.
        if delay.is_zero() {
            return;
        }
        let weak = surface.downgrade();
        let timer = state
            .handle
            .insert_source(Timer::from_duration(delay), move |_, _, state| {
                state.commit_timer_due(&weak);
                TimeoutAction::Drop
            });
        if let Err(err) = timer {
            // Without the timer the update could wait for a render that never comes.
            tracing::warn!(%err, "commit timing: cannot arm the deadline, not holding the update");
            return;
        }
        let barrier = with_states(surface, |states| {
            let barriers = states
                .data_map
                .get_or_insert(CommitTimerBarrierStateUserData::default);
            barriers.lock().ok().map(|mut b| b.register(timestamp))
        });
        if let Some(barrier) = barrier {
            add_blocker(surface, barrier);
        }
    });
}

/// Releases the commit timers of `states` due by `until`; returns whether one was.
fn release_timers(states: &SurfaceData, until: Time<Monotonic>) -> bool {
    states
        .data_map
        .get::<CommitTimerBarrierStateUserData>()
        .and_then(|b| b.lock().ok().map(|mut b| b.signal_until(until)))
        .unwrap_or(false)
}

/// Takes the fifo barrier of `states` if `output` is where the surface is presented.
fn take_fifo(surface: &WlSurface, states: &SurfaceData, output: &Output) -> Option<Barrier> {
    let primary = surface_primary_scanout_output(surface, states);
    if primary.as_ref().is_some_and(|o| o != output) {
        return None;
    }
    states
        .cached_state
        .get::<FifoBarrierCachedState>()
        .current()
        .barrier
        .take()
}

/// Barriers latched by an output's last render, signalled when that frame is presented.
#[derive(Default)]
struct Latched(Mutex<Vec<(Barrier, Client)>>);

impl Aurora {
    /// A commit timer reached its deadline: whatever is still held for it goes now.
    fn commit_timer_due(&mut self, surface: &Weak<WlSurface>) {
        let Ok(surface) = surface.upgrade() else {
            return;
        };
        let now = self.clock.now();
        if with_states(&surface, |states| release_timers(states, now))
            && let Some(client) = surface.client()
        {
            self.clear_blockers([(client.id(), client)].into());
        }
    }

    /// Before `output` renders a frame that is presented at `target`: releases the commit
    /// timers due by then, so those updates make it into this frame.
    pub fn release_commit_timers(&mut self, output: &Output, target: Duration) {
        let target = Time::<Monotonic>::from(target);
        let mut clients = HashMap::new();
        self.with_output_surfaces(output, |surface, states| {
            if release_timers(states, target)
                && let Some(client) = surface.client()
            {
                clients.insert(client.id(), client);
            }
        });
        self.clear_blockers(clients);
    }

    /// `output` rendered a frame: the fifo barriers of the updates it shows wait for it to be
    /// presented. Barriers a previous frame left (it never presented) go first.
    pub fn latch_fifo_barriers(&mut self, output: &Output) {
        let mut taken = Vec::new();
        self.with_output_surfaces(output, |surface, states| {
            if let Some(barrier) = take_fifo(surface, states, output)
                && let Some(client) = surface.client()
            {
                taken.push((barrier, client));
            }
        });
        let latched = output
            .user_data()
            .get_or_insert_threadsafe(Latched::default);
        let stale = latched
            .0
            .lock()
            .map(|mut l| std::mem::replace(&mut *l, taken))
            .unwrap_or_default();
        self.signal(stale);
    }

    /// `output` presented its frame: the updates waiting on what it showed may follow.
    pub fn signal_fifo_barriers(&mut self, output: &Output) {
        let Some(latched) = output.user_data().get::<Latched>() else {
            return;
        };
        let due = latched
            .0
            .lock()
            .map(|mut l| std::mem::take(&mut *l))
            .unwrap_or_default();
        self.signal(due);
    }

    fn signal(&mut self, barriers: Vec<(Barrier, Client)>) {
        if barriers.is_empty() {
            return;
        }
        let mut clients = HashMap::new();
        for (barrier, client) in barriers {
            barrier.signal();
            clients.insert(client.id(), client);
        }
        self.clear_blockers(clients);
    }

    /// Lets the held commits of `clients` whose blockers are gone go through.
    #[allow(clippy::mutable_key_type)]
    fn clear_blockers(&mut self, clients: HashMap<ClientId, Client>) {
        let dh = self.display_handle.clone();
        for client in clients.into_values() {
            self.client_compositor_state(&client)
                .blocker_cleared(self, &dh);
        }
    }

    /// Every surface `output` draws: windows, X11 overrides, layers, the lock surface, the
    /// cursor. `f` must not call back into the compositor (the layer map is locked).
    fn with_output_surfaces(&self, output: &Output, mut f: impl FnMut(&WlSurface, &SurfaceData)) {
        if !crate::lock::engaged() {
            for window in self.space.elements() {
                if self.space.outputs_for_element(window).contains(output) {
                    window.with_surfaces(&mut f);
                }
            }
            let unmanaged = &self.xwayland.unmanaged;
            for window in unmanaged.elements() {
                if unmanaged.outputs_for_element(window).contains(output) {
                    window.with_surfaces(&mut f);
                }
            }
            for layer in layer_map_for_output(output).layers() {
                layer.with_surfaces(&mut f);
            }
        }
        if let Some(surface) = crate::lock::surface_of(output) {
            with_surfaces_surface_tree(&surface, &mut f);
        }
        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
            with_surfaces_surface_tree(surface, &mut f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_micros(6944);

    #[test]
    fn predicts_the_next_vblank_on_the_grid() {
        let last = Duration::from_millis(100);
        let now = last + FRAME.mul_f64(0.6);
        assert_eq!(predict_presentation(now, Some(last), FRAME), last + FRAME);
        // Several frames idle: still on the grid, the first vblank after now.
        let now = last + FRAME * 3 + FRAME / 4;
        assert_eq!(
            predict_presentation(now, Some(last), FRAME),
            last + FRAME * 4
        );
    }

    #[test]
    fn without_history_the_frame_target_is_now() {
        let now = Duration::from_millis(5);
        assert_eq!(predict_presentation(now, None, FRAME), now);
        assert_eq!(predict_presentation(now, Some(now + FRAME), FRAME), now);
        assert_eq!(predict_presentation(now, Some(now), Duration::ZERO), now);
    }
}
