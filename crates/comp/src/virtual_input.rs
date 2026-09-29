//! zwp_virtual_keyboard_v1 (wtype and friends), implemented here rather than with
//! Smithay's manager: that one forwards keys straight to the client, bypassing binds and
//! the emergency chords, and swaps the seat's keymap. Here every event is translated to a
//! key of Aurora's own keymap and goes through `Aurora::on_key` like a physical key.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use smithay::{
    backend::input::{InputTime, KeyState},
    input::keyboard::{KeyboardSource, Keycode, Keysym, keysyms, xkb},
    reexports::{
        wayland_protocols_misc::zwp_virtual_keyboard_v1::server::{
            zwp_virtual_keyboard_manager_v1::{self, ZwpVirtualKeyboardManagerV1},
            zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, backend::ClientId, backend::GlobalId,
            protocol::wl_keyboard::KeymapFormat,
        },
    },
    wayland::{Dispatch2, GlobalDispatch2},
};

use crate::state::Aurora;

/// Standard X11 modifier bits (Mod1 is Alt, Mod4 the logo key, Mod5 AltGr) and the key of
/// Aurora's own keymap that stands in for each.
const MOD_KEYS: [(u32, u32); 5] = [
    (1 << 0, keysyms::KEY_Shift_L),
    (1 << 2, keysyms::KEY_Control_L),
    (1 << 3, keysyms::KEY_Alt_L),
    (1 << 6, keysyms::KEY_Super_L),
    (1 << 7, keysyms::KEY_ISO_Level3_Shift),
];

/// Keeps the global alive; `set_allowed` follows `general.allow_virtual_keyboard`.
pub struct VirtualKeyboardGlobal {
    _global: GlobalId,
    allowed: Arc<AtomicBool>,
}

impl VirtualKeyboardGlobal {
    pub fn new(dh: &DisplayHandle, allowed: bool) -> Self {
        let allowed = Arc::new(AtomicBool::new(allowed));
        let global = dh.create_global::<Aurora, ZwpVirtualKeyboardManagerV1, _>(
            1,
            ManagerGlobal {
                allowed: allowed.clone(),
            },
        );
        Self {
            _global: global,
            allowed,
        }
    }

    /// Clients that already bound keep the object, but their events are dropped while off.
    pub fn set_allowed(&self, allowed: bool) {
        self.allowed.store(allowed, Ordering::Relaxed);
    }
}

pub struct ManagerGlobal {
    allowed: Arc<AtomicBool>,
}

pub struct Manager {
    allowed: Arc<AtomicBool>,
}

impl GlobalDispatch2<ZwpVirtualKeyboardManagerV1, Aurora> for ManagerGlobal {
    fn bind(
        &self,
        _state: &mut Aurora,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpVirtualKeyboardManagerV1>,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        data_init.init(
            resource,
            Manager {
                allowed: self.allowed.clone(),
            },
        );
    }

    fn can_view(&self, _client: &Client) -> bool {
        self.allowed.load(Ordering::Relaxed)
    }
}

impl Dispatch2<ZwpVirtualKeyboardManagerV1, Aurora> for Manager {
    fn request(
        &self,
        _state: &mut Aurora,
        _client: &Client,
        _resource: &ZwpVirtualKeyboardManagerV1,
        request: zwp_virtual_keyboard_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        if let zwp_virtual_keyboard_manager_v1::Request::CreateVirtualKeyboard { id, .. } = request
        {
            data_init.init(
                id,
                VirtualKeyboard {
                    allowed: self.allowed.clone(),
                    source: KeyboardSource::new_auxiliary(),
                    inner: Mutex::new(Inner::default()),
                },
            );
        }
    }
}

#[derive(Default)]
struct Inner {
    /// The client's keymap state: only used to turn its keycodes into keysyms.
    xkb: Option<xkb::State>,
    /// Modifier bits (see `MOD_KEYS`) last requested.
    mods: u32,
    /// Keys this keyboard holds down in Aurora, for cleanup on destroy.
    held: Vec<Keycode>,
}

// Safety: the xkb state is only ever touched on the compositor thread, from the
// dispatch callbacks; the Mutex exists to satisfy the `Send + Sync` bound of user data.
unsafe impl Send for Inner {}

pub struct VirtualKeyboard {
    allowed: Arc<AtomicBool>,
    source: KeyboardSource,
    inner: Mutex<Inner>,
}

impl Dispatch2<ZwpVirtualKeyboardV1, Aurora> for VirtualKeyboard {
    fn request(
        &self,
        state: &mut Aurora,
        _client: &Client,
        resource: &ZwpVirtualKeyboardV1,
        request: zwp_virtual_keyboard_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Aurora>,
    ) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        match request {
            zwp_virtual_keyboard_v1::Request::Keymap { format, fd, size } => {
                if format != KeymapFormat::XkbV1 as u32 {
                    return tracing::debug!(format, "virtual keyboard: unsupported keymap format");
                }
                let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                // Safety: the fd is the client's keymap; xkbcommon maps and parses it.
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &context,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                };
                match keymap {
                    Ok(Some(keymap)) => inner.xkb = Some(xkb::State::new(&keymap)),
                    _ => tracing::debug!("virtual keyboard: unusable keymap"),
                }
            }
            zwp_virtual_keyboard_v1::Request::Key {
                time,
                key,
                state: key_state,
            } => {
                let Some(xkb) = inner.xkb.as_ref() else {
                    return resource.post_error(
                        zwp_virtual_keyboard_v1::Error::NoKeymap,
                        "key sent before keymap",
                    );
                };
                if !self.allowed.load(Ordering::Relaxed) {
                    return;
                }
                let sym = xkb.key_get_one_sym(xkb::Keycode::new(key.saturating_add(8)));
                let pressed = key_state == 1;
                let time = InputTime::from_millis(time);
                let Some(keycode) = state.inject_keysym(self.source, sym, pressed, time) else {
                    return;
                };
                inner.held.retain(|k| *k != keycode);
                if pressed {
                    inner.held.push(keycode);
                }
            }
            zwp_virtual_keyboard_v1::Request::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                let Some(xkb) = inner.xkb.as_mut() else {
                    return resource.post_error(
                        zwp_virtual_keyboard_v1::Error::NoKeymap,
                        "modifiers sent before keymap",
                    );
                };
                if !self.allowed.load(Ordering::Relaxed) {
                    return;
                }
                xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                let known: u32 = MOD_KEYS.iter().map(|(bit, _)| bit).sum();
                let mask = (mods_depressed | mods_latched | mods_locked) & known;
                let changed = mask ^ inner.mods;
                inner.mods = mask;
                let now = InputTime::now();
                for (bit, sym) in MOD_KEYS {
                    if changed & bit != 0 {
                        let sym = Keysym::new(sym);
                        let pressed = mask & bit != 0;
                        if let Some(keycode) = state.inject_key(self.source, sym, pressed, now) {
                            inner.held.retain(|k| *k != keycode);
                            if pressed {
                                inner.held.push(keycode);
                            }
                        }
                    }
                }
            }
            zwp_virtual_keyboard_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(&self, state: &mut Aurora, _client: ClientId, _resource: &ZwpVirtualKeyboardV1) {
        let held = self
            .inner
            .lock()
            .map(|mut i| std::mem::take(&mut i.held))
            .unwrap_or_default();
        let keyboard = state.keyboard.clone();
        keyboard.release_source(state, self.source);
        // The releases above skip the filter, so entries a bind left behind go here.
        state.suppressed_keys.retain(|k| !held.contains(k));
        if state
            .input
            .repeat
            .as_ref()
            .is_some_and(|r| held.contains(&r.keycode()))
        {
            state.cancel_repeat();
        }
    }
}

/// How a keysym is typed on Aurora's keymap: modifiers are the shift levels it needs.
struct Level {
    shift: bool,
    altgr: bool,
}

impl Aurora {
    /// Presses or releases the key that produces `sym` on Aurora's own layout, wrapping the
    /// press in Shift or AltGr when the symbol lives on a higher level. Unknown symbols are
    /// dropped. Returns the keycode used.
    fn inject_keysym(
        &mut self,
        source: KeyboardSource,
        sym: Keysym,
        pressed: bool,
        time: InputTime,
    ) -> Option<Keycode> {
        let Some((keycode, level)) = self.locate_keysym(sym) else {
            tracing::debug!(
                sym = sym.raw(),
                "virtual keyboard: no key for keysym, dropped"
            );
            return None;
        };
        if !pressed {
            self.on_key(source, keycode, KeyState::Released, time);
            return Some(keycode);
        }
        let mods = self.keyboard.modifier_state();
        let mut wrap = Vec::new();
        if level.shift && !mods.shift {
            wrap.extend(
                self.keyboard
                    .keycode_for_keysym(Keysym::new(keysyms::KEY_Shift_L)),
            );
        }
        if level.altgr && !mods.iso_level3_shift {
            wrap.extend(
                self.keyboard
                    .keycode_for_keysym(Keysym::new(keysyms::KEY_ISO_Level3_Shift)),
            );
        }
        for m in &wrap {
            self.on_key(source, *m, KeyState::Pressed, time);
        }
        self.on_key(source, keycode, KeyState::Pressed, time);
        for m in wrap.iter().rev() {
            self.on_key(source, *m, KeyState::Released, time);
        }
        Some(keycode)
    }

    /// Like `inject_keysym`, for modifier keys, which are always on level 0.
    fn inject_key(
        &mut self,
        source: KeyboardSource,
        sym: Keysym,
        pressed: bool,
        time: InputTime,
    ) -> Option<Keycode> {
        let keycode = self.keyboard.keycode_for_keysym(sym)?;
        let state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        self.on_key(source, keycode, state, time);
        Some(keycode)
    }

    fn locate_keysym(&mut self, sym: Keysym) -> Option<(Keycode, Level)> {
        let keyboard = self.keyboard.clone();
        let keycode = keyboard.keycode_for_keysym(sym)?;
        let level = keyboard.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().ok()?;
            // Safety: the references do not outlive the lock guard.
            let (keymap, state) = unsafe { (xkb.keymap(), xkb.state()) };
            let layout = state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
            (0..keymap.num_levels_for_key(keycode, layout)).find(|level| {
                keymap
                    .key_get_syms_by_level(keycode, layout, *level)
                    .contains(&sym)
            })
        })?;
        // The four-level types of the usual layouts: shift, AltGr, both.
        let (shift, altgr) = match level {
            0 => (false, false),
            1 => (true, false),
            2 => (false, true),
            3 => (true, true),
            _ => return None,
        };
        Some((keycode, Level { shift, altgr }))
    }
}
