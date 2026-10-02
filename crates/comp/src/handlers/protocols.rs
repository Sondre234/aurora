//! Handlers for the small protocols: decoration, activation, data control, idle and
//! keyboard-shortcuts inhibit. Pointer constraints live in `constraints.rs`.
use std::time::Duration;

use smithay::{
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::IsAlive,
    wayland::{
        compositor::get_parent,
        foreign_toplevel_list::{ForeignToplevelListHandler, ForeignToplevelListState},
        idle_inhibit::IdleInhibitHandler,
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState,
            KeyboardShortcutsInhibitor, KeyboardShortcutsInhibitorSeat,
        },
        seat::WaylandFocus,
        selection::wlr_data_control::{DataControlHandler, DataControlState},
        shell::xdg::{ToplevelSurface, decoration::XdgDecorationHandler},
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        xdg_foreign::{XdgForeignHandler, XdgForeignState},
    },
};

use crate::{Aurora, focus::FocusTarget};

/// Activation tokens older than this are ignored and pruned.
const TOKEN_TTL: Duration = Duration::from_secs(10);

/// Borders are drawn by the compositor, so every toplevel is told to skip client decorations.
impl XdgDecorationHandler for Aurora {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        self.force_server_side(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        self.force_server_side(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.force_server_side(&toplevel);
    }
}

impl Aurora {
    fn force_server_side(&self, toplevel: &ToplevelSurface) {
        toplevel.with_pending_state(|state| state.decoration_mode = Some(Mode::ServerSide));
        // Before the initial configure the mode simply rides along with it.
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

impl XdgActivationHandler for Aurora {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.protocols.activation
    }

    /// A client token counts only if it names our seat and a serial at least as new as the
    /// keyboard's last enter, i.e. it came from input the user just made.
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        self.protocols
            .activation
            .retain_tokens(|_, d| d.timestamp.elapsed() < TOKEN_TTL);
        let Some((serial, wl_seat)) = &data.serial else {
            return false;
        };
        let ours =
            smithay::input::Seat::<Aurora>::from_resource(wl_seat).is_some_and(|s| s == self.seat);
        let issued = smithay::utils::SERIAL_COUNTER.next_serial();
        ours && issued.is_no_older_than(serial)
            && self
                .keyboard
                .last_enter()
                .is_none_or(|enter| serial.is_no_older_than(&enter))
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        self.protocols.activation.remove_token(&token);
        if data.timestamp.elapsed() > TOKEN_TTL || self.is_locked() {
            return;
        }
        let mut root = surface;
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        let Some(id) = self.wm.id_of(&root) else {
            return;
        };
        let Some(win) = self.wm.windows.get_mut(&id) else {
            return;
        };
        let visible = self.wm.ws_output.contains_key(&win.ws);
        // Tokens we minted for spawned apps are trusted; a client's need the config's blessing.
        let trusted = data.client_id.is_none() || self.config.general.focus_on_activate;
        if visible && trusted {
            self.focus_window(Some(id), true);
        } else if self.wm.focused != Some(id) {
            win.urgent = true;
            tracing::info!("activation: urgent {}:{}", id.0, win.app_id);
        }
    }
}

impl ForeignToplevelListHandler for Aurora {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.protocols.foreign_toplevel
    }
}

impl DataControlHandler for Aurora {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.protocols.data_control
    }
}

impl IdleInhibitHandler for Aurora {
    fn inhibit(&mut self, surface: WlSurface) {
        self.protocols.idle_inhibitors.insert(surface);
        self.update_idle_inhibit();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.protocols.idle_inhibitors.remove(&surface);
        self.update_idle_inhibit();
    }
}

impl IdleNotifierHandler for Aurora {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.protocols.idle_notifier
    }
}

impl Aurora {
    /// Drops inhibitors whose surface died without saying so, then tells the notifier.
    pub fn update_idle_inhibit(&mut self) {
        self.protocols.idle_inhibitors.retain(|s| s.alive());
        let inhibited = !self.protocols.idle_inhibitors.is_empty();
        self.protocols.idle_notifier.set_is_inhibited(inhibited);
    }

    /// Any input is activity for ext-idle-notify.
    pub fn notify_activity(&mut self) {
        let seat = self.seat.clone();
        self.protocols.idle_notifier.notify_activity(&seat);
    }
}

impl KeyboardShortcutsInhibitHandler for Aurora {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.protocols.shortcuts_inhibit
    }

    fn new_inhibitor(&mut self, _inhibitor: KeyboardShortcutsInhibitor) {
        let focus = self.keyboard.current_focus();
        self.update_shortcuts_inhibit(focus.as_ref());
    }

    fn inhibitor_destroyed(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        if self.protocols.active_inhibitor.as_ref() == Some(&inhibitor) {
            self.protocols.active_inhibitor = None;
        }
    }
}

impl Aurora {
    /// Keyboard focus moved: the inhibitor of the old surface stands down and the new
    /// surface's, if it asked for one, takes over. Emergency chords never consult any of this.
    pub fn update_shortcuts_inhibit(&mut self, focus: Option<&FocusTarget>) {
        let surface = focus.and_then(|f| f.wl_surface()).map(|s| s.into_owned());
        if let Some(old) = self.protocols.active_inhibitor.take()
            && surface.as_ref() != Some(old.wl_surface())
        {
            old.inactivate();
        }
        let Some(surface) = surface else { return };
        if let Some(inhibitor) = self.seat.keyboard_shortcuts_inhibitor_for_surface(&surface) {
            inhibitor.activate();
            self.protocols.active_inhibitor = Some(inhibitor);
        }
    }

    /// The `revoke-inhibit` action: takes the keyboard back from the focused client.
    pub fn revoke_shortcuts_inhibit(&mut self) {
        // An exclusive layer surface that took the keyboard is dropped to on-demand too, so a
        // hung launcher cannot keep every bind off.
        if let Some(layer) = self.layer_focus.exclusive.clone() {
            tracing::info!("layer focus: revoked from {}", layer.namespace());
            self.layer_focus.demoted.push(layer);
            self.refresh_layer_focus();
        }
        if let Some(inhibitor) = &self.protocols.active_inhibitor {
            tracing::info!("shortcuts inhibit: revoked");
            inhibitor.inactivate();
        }
    }
}

/// Portal dialogs (file chooser, screen share picker) run in another process and name their
/// parent through an exported handle; Smithay checks it and sets the toplevel's parent.
impl XdgForeignHandler for Aurora {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.protocols.xdg_foreign
    }
}
