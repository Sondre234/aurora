//! The key filter: emergency chords first, then config binds, then the focused client.
use std::time::Duration;

use smithay::{
    backend::input::{InputTime, KeyState},
    input::keyboard::{FilterResult, KeyboardSource, Keycode, KeysymHandle, ModifiersState},
    reexports::calloop::{
        RegistrationToken,
        timer::{TimeoutAction, Timer},
    },
    utils::SERIAL_COUNTER,
};

use crate::{
    action::Action,
    config::keybind::{Bind, Chord, Mods, Trigger},
    emergency::{self, Emergency},
    overview::input::{OverviewKey, key_for},
    state::{Aurora, REPEAT_DELAY, REPEAT_RATE},
};

/// What the filter decided for a key press.
enum KeyOutcome {
    Emergency(Emergency),
    Bound(Bind),
    /// A key the overview acts on.
    Overview(OverviewKey),
    /// The release of a key whose press a bind took.
    Swallowed,
}

/// The repeat bind that is currently held.
pub struct Repeat {
    keycode: Keycode,
    token: RegistrationToken,
}

impl Aurora {
    /// Shortcuts are off while an exclusive layer surface has the keyboard (and while
    /// a client holds a keyboard-shortcuts inhibitor); binds marked `bypass_inhibit` ignore
    /// this. The emergency chords never consult it.
    pub fn binds_allowed(&self) -> bool {
        self.layer_focus.exclusive.is_none()
            && !self
                .protocols
                .active_inhibitor
                .as_ref()
                .is_some_and(|i| i.is_active())
    }

    /// The entry point for every key, physical or injected. `time` is the event's own.
    pub fn on_key(
        &mut self,
        source: KeyboardSource,
        keycode: Keycode,
        key_state: KeyState,
        time: InputTime,
    ) {
        self.notify_activity();
        let serial = SERIAL_COUNTER.next_serial();
        let outcome = self.keyboard.clone().input_from_source::<KeyOutcome, _>(
            source,
            self,
            keycode,
            key_state,
            serial,
            time,
            |state, modifiers, handle| state.filter_key(keycode, key_state, modifiers, &handle),
        );

        match &outcome {
            Some(KeyOutcome::Emergency(Emergency::Quit)) => {
                tracing::warn!("quitting: quit chord");
                crate::safety::arm_exit_deadline();
                self.loop_signal.stop();
            }
            Some(KeyOutcome::Emergency(Emergency::VtSwitch(vt))) => {
                let vt = *vt;
                tracing::info!(vt, "VT switch requested");
                self.backend.change_vt(vt);
            }
            Some(KeyOutcome::Bound(bind)) => {
                // Owned, so a config reload triggered by the action cannot dangle.
                self.cancel_repeat();
                self.dispatch(bind.action.clone());
                if bind.repeat {
                    self.start_repeat(keycode, bind.action.clone());
                }
            }
            Some(KeyOutcome::Overview(key)) => self.overview_key(*key),
            Some(KeyOutcome::Swallowed) | None => {}
        }
        if matches!(outcome, Some(KeyOutcome::Emergency(_))) {
            let _ = self.display_handle.flush_clients();
        }
    }

    fn filter_key(
        &mut self,
        keycode: Keycode,
        key_state: KeyState,
        modifiers: &ModifiersState,
        handle: &KeysymHandle<'_>,
    ) -> FilterResult<KeyOutcome> {
        if key_state == KeyState::Released {
            let Some(i) = self.suppressed_keys.iter().position(|k| *k == keycode) else {
                return FilterResult::Forward;
            };
            self.suppressed_keys.swap_remove(i);
            if self
                .input
                .repeat
                .as_ref()
                .is_some_and(|r| r.keycode == keycode)
            {
                self.cancel_repeat();
            }
            return FilterResult::Intercept(KeyOutcome::Swallowed);
        }

        let mods = Mods::from_state(modifiers);
        let raw = handle.raw_syms();
        // Runs first and ignores everything else on purpose: the quit chord and VT switch
        // must always work.
        if let Some(action) =
            emergency::classify(mods, handle.raw_code(), &raw, handle.modified_sym())
        {
            self.suppress(keycode);
            return FilterResult::Intercept(KeyOutcome::Emergency(action));
        }
        if emergency::near_miss(mods, &raw) {
            // Leaves a trace when a VT chord is pressed with the wrong modifiers.
            tracing::info!(
                alt = modifiers.alt,
                altgr = modifiers.iso_level3_shift,
                logo = modifiers.logo,
                "Ctrl+F-key pressed without Alt, not a VT switch"
            );
        }

        // Raw level-0 syms, so the bind does not depend on the layout's shifted symbols and
        // AltGr (its own modifier bit) never turns Super+AltGr+q into Super+q.
        let allowed = self.binds_allowed();
        // The overview takes every key except the chord that toggles it.
        if allowed && self.overview_grabs_input() {
            let toggles = raw.iter().any(|sym| {
                let chord = Chord {
                    mods,
                    trigger: Trigger::Key(sym.raw()),
                };
                self.config
                    .binds
                    .get(&chord)
                    .is_some_and(|b| b.action == Action::Overview)
            });
            if !toggles {
                self.suppress(keycode);
                return FilterResult::Intercept(match key_for(&raw, modifiers.shift) {
                    Some(key) => KeyOutcome::Overview(key),
                    None => KeyOutcome::Swallowed,
                });
            }
        }
        let bind = raw.iter().find_map(|sym| {
            let chord = Chord {
                mods,
                trigger: Trigger::Key(sym.raw()),
            };
            self.config
                .binds
                .get(&chord)
                .filter(|b| allowed || b.bypass_inhibit)
                .cloned()
        });
        match bind {
            Some(bind) => {
                self.suppress(keycode);
                FilterResult::Intercept(KeyOutcome::Bound(bind))
            }
            None => FilterResult::Forward,
        }
    }

    fn suppress(&mut self, keycode: Keycode) {
        if !self.suppressed_keys.contains(&keycode) {
            self.suppressed_keys.push(keycode);
        }
    }

    /// Fires `action` again after `REPEAT_DELAY`, then at `REPEAT_RATE`, until the key goes up.
    fn start_repeat(&mut self, keycode: Keycode, action: Action) {
        let period = Duration::from_micros(1_000_000 / REPEAT_RATE as u64);
        let timer = Timer::from_duration(Duration::from_millis(REPEAT_DELAY as u64));
        let token = self.handle.insert_source(timer, move |_, _, state| {
            // Also stops if the release was absorbed elsewhere (another source, a pause).
            let held = state.keyboard.pressed_keys().contains(&keycode);
            if !held
                || state
                    .input
                    .repeat
                    .as_ref()
                    .is_none_or(|r| r.keycode != keycode)
            {
                state.input.repeat = None;
                return TimeoutAction::Drop;
            }
            state.dispatch(action.clone());
            // The action may have cancelled the repeat, this very timer included.
            if state
                .input
                .repeat
                .as_ref()
                .is_some_and(|r| r.keycode == keycode)
            {
                TimeoutAction::ToDuration(period)
            } else {
                TimeoutAction::Drop
            }
        });
        match token {
            Ok(token) => self.input.repeat = Some(Repeat { keycode, token }),
            Err(err) => tracing::warn!(%err, "cannot start key repeat"),
        }
    }

    pub fn cancel_repeat(&mut self) {
        if let Some(repeat) = self.input.repeat.take() {
            self.handle.remove(repeat.token);
        }
    }

    /// Everything a VT switch would have swallowed the releases of.
    pub fn reset_input_state(&mut self) {
        self.suppressed_keys.clear();
        self.input.suppressed_buttons.clear();
        self.input.wheel_v120 = 0.0;
        self.cancel_repeat();
    }
}

impl Repeat {
    pub fn keycode(&self) -> Keycode {
        self.keycode
    }
}
