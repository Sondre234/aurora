//! `wp_tearing_control_v1`: a surface may hint that its frames can be presented as soon as
//! they are ready (async page flips, tearing) instead of at vblank. Aurora would honour it
//! only for the fullscreen window of an output, and only with `[general] allow_tearing`.
//!
//! Bookkeeping only. Smithay's `DrmCompositor` at the pinned rev has no way to ask for an
//! async flip: its page flips are always committed with `PAGE_FLIP_EVENT | NONBLOCK`
//! (`drm/surface/atomic.rs`), there is no `DRM_MODE_PAGE_FLIP_ASYNC` in its API, and the
//! render loop paces itself on one flip per vblank. Atomic async flips also need kernel 6.8+
//! and may only change the primary plane's framebuffer, which the compositor would have to
//! guarantee (direct scanout of the fullscreen surface, no cursor plane update in the same
//! commit). So the hint is tracked per surface, the decision is computed and logged when it
//! changes, and presentation stays vsynced until the backend can do it.
use std::sync::atomic::{AtomicBool, Ordering};

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::wp::tearing_control::v1::server::{
            wp_tearing_control_manager_v1::{self, WpTearingControlManagerV1},
            wp_tearing_control_v1::{self, PresentationHint, WpTearingControlV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, WEnum, Weak, backend::GlobalId,
            protocol::wl_surface::WlSurface,
        },
    },
    wayland::{
        Dispatch2, GlobalDispatch2,
        compositor::{Cacheable, with_states},
        seat::WaylandFocus,
    },
};

use crate::{state::Aurora, wm::Wm};

/// The double-buffered hint: `true` for async.
#[derive(Clone, Copy, Debug, Default)]
struct Hint(bool);

impl Cacheable for Hint {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        *self
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// Set while the surface has a tearing control object.
#[derive(Default)]
struct Attached(AtomicBool);

/// Whether the surface's committed hint is async.
pub fn wants_async(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states.cached_state.get::<Hint>().current().0
    })
}

/// Tearing is used only for a fullscreen surface that asks for it, and only when the user
/// allows it at all.
pub fn allowed(allow_tearing: bool, fullscreen: bool, hint_async: bool) -> bool {
    allow_tearing && fullscreen && hint_async
}

/// The decision for `output` right now.
pub fn wanted(wm: &Wm, output: &Output, allow_tearing: bool) -> bool {
    if !allow_tearing {
        return false;
    }
    let surface = wm
        .active_ws
        .get(output)
        .and_then(|ws| wm.workspaces.get(ws))
        .and_then(|w| w.fullscreen())
        .filter(|(_, mode)| *mode == aurora_layout::FsMode::Fullscreen)
        .and_then(|(id, _)| wm.windows.get(&id))
        .and_then(|win| win.element.wl_surface().map(|s| s.into_owned()));
    allowed(
        allow_tearing,
        surface.is_some(),
        surface.as_ref().is_some_and(wants_async),
    )
}

pub struct TearingState {
    _global: GlobalId,
    /// Outputs whose fullscreen surface currently qualifies, to log only the changes.
    wanted: Vec<String>,
}

impl TearingState {
    pub fn new(dh: &DisplayHandle) -> Self {
        Self {
            _global: dh.create_global::<Aurora, WpTearingControlManagerV1, _>(1, ManagerGlobal),
            wanted: Vec::new(),
        }
    }

    /// Records the decision for an output from the render path; logs when it changes.
    pub fn note(&mut self, output: &Output, wanted: bool) {
        let name = output.name();
        let known = self.wanted.iter().position(|n| *n == name);
        match (wanted, known) {
            (true, None) => {
                tracing::info!(
                    "tearing: output={name} fullscreen surface asks for async presentation; \
                     async page flips are not supported by this backend, presenting with vsync"
                );
                self.wanted.push(name);
            }
            (false, Some(i)) => {
                tracing::info!("tearing: output={name} back to vsync");
                self.wanted.swap_remove(i);
            }
            _ => {}
        }
    }
}

pub struct ManagerGlobal;
pub struct Manager;
pub struct Control(Weak<WlSurface>);

impl GlobalDispatch2<WpTearingControlManagerV1, Aurora> for ManagerGlobal {
    fn bind(
        &self,
        _state: &mut Aurora,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WpTearingControlManagerV1>,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        data_init.init(resource, Manager);
    }
}

impl Dispatch2<WpTearingControlManagerV1, Aurora> for Manager {
    fn request(
        &self,
        _state: &mut Aurora,
        _client: &Client,
        resource: &WpTearingControlManagerV1,
        request: wp_tearing_control_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        let wp_tearing_control_manager_v1::Request::GetTearingControl { id, surface } = request
        else {
            return;
        };
        let taken = with_states(&surface, |states| {
            states
                .data_map
                .get_or_insert_threadsafe(Attached::default)
                .0
                .swap(true, Ordering::Relaxed)
        });
        if taken {
            resource.post_error(
                wp_tearing_control_manager_v1::Error::TearingControlExists,
                "the surface already has a tearing control",
            );
            return;
        }
        data_init.init(id, Control(surface.downgrade()));
    }
}

impl Dispatch2<WpTearingControlV1, Aurora> for Control {
    fn request(
        &self,
        _state: &mut Aurora,
        _client: &Client,
        _resource: &WpTearingControlV1,
        request: wp_tearing_control_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Aurora>,
    ) {
        let Ok(surface) = self.0.upgrade() else {
            return;
        };
        match request {
            wp_tearing_control_v1::Request::SetPresentationHint { hint } => {
                let hint = matches!(hint, WEnum::Value(PresentationHint::Async));
                with_states(&surface, |states| {
                    states.cached_state.get::<Hint>().pending().0 = hint;
                });
            }
            // Back to vsync at the next commit, and the surface may get a new control.
            wp_tearing_control_v1::Request::Destroy => with_states(&surface, |states| {
                states.cached_state.get::<Hint>().pending().0 = false;
                if let Some(attached) = states.data_map.get::<Attached>() {
                    attached.0.store(false, Ordering::Relaxed);
                }
            }),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tearing_needs_permission_fullscreen_and_the_hint() {
        assert!(allowed(true, true, true));
        assert!(!allowed(false, true, true), "allow_tearing = false wins");
        assert!(!allowed(true, false, true), "not fullscreen");
        assert!(!allowed(true, true, false), "vsync hint");
    }
}
