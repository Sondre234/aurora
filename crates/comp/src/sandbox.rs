//! wp-security-context-v1: sandboxed clients (Flatpak) and the globals they may not see.
//!
//! A sandbox engine binds the manager, hands Aurora a listening socket plus metadata and
//! closes a fd when the sandbox goes away. Every client accepted on that socket carries the
//! `SecurityContext` in its `ClientState`, and the privileged globals filter it out through
//! their `can_view` filter, so the client never even learns they exist.
//!
//! Policy (niri's, plus Aurora's own privileged globals): hidden from sandboxed clients are
//! whatever reads or injects input, reads the screen or other clients' data, or takes over
//! the session:
//!
//! | Global | Why |
//! |---|---|
//! | `zwlr_data_control_manager_v1` | reads and replaces the clipboard of every client |
//! | `zwp_virtual_keyboard_manager_v1` | injects keys, and through Aurora's binds runs actions |
//! | `zwp_input_method_manager_v2` | sees every key and every text field |
//! | `ext_image_copy_capture_manager_v1`, `ext_output_image_capture_source_manager_v1` | screenshots without the portal |
//! | `ext_session_lock_manager_v1` | takes over (or fakes) the lock screen |
//! | `zwlr_layer_shell_v1` | overlays above every window, exclusive keyboard focus |
//! | `ext_foreign_toplevel_list_v1` | lists every window with title and app id |
//! | `wp_security_context_manager_v1` | a sandbox must not mint new contexts |
//!
//! Output management and gamma control (the DISPLAY stream) belong in this table too: filter
//! them with [`can_view`] and a new [`Privileged`] variant.
//!
//! Everything else (xdg-shell, xdg-activation, idle-inhibit, pointer constraints, text input,
//! fifo, dmabuf, ...) is an ordinary client's right and stays visible.
use smithay::{
    reexports::wayland_server::{Client, DisplayHandle},
    wayland::security_context::{
        SecurityContext, SecurityContextHandler, SecurityContextListenerSource,
        SecurityContextState,
    },
};
use std::sync::Arc;

use crate::state::{Aurora, ClientState};

/// The globals sandboxed clients do not get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privileged {
    DataControl,
    VirtualKeyboard,
    InputMethod,
    ImageCopyCapture,
    SessionLock,
    LayerShell,
    ForeignToplevelList,
    SecurityContext,
}

impl Privileged {
    /// Whether a client sandboxed or not may see the global. The policy table above.
    pub const fn allowed(self, sandboxed: bool) -> bool {
        match self {
            Self::DataControl
            | Self::VirtualKeyboard
            | Self::InputMethod
            | Self::ImageCopyCapture
            | Self::SessionLock
            | Self::LayerShell
            | Self::ForeignToplevelList
            | Self::SecurityContext => !sandboxed,
        }
    }
}

/// Whether the client came in through a security context's socket. XWayland and clients on
/// the main socket are not.
pub fn is_sandboxed(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|data| data.security_context.is_some())
}

/// The `can_view` filter of a privileged global.
pub fn can_view(global: Privileged, client: &Client) -> bool {
    global.allowed(is_sandboxed(client))
}

/// Creates the manager global, which sandboxed clients cannot see themselves.
pub fn init(dh: &DisplayHandle) -> SecurityContextState {
    SecurityContextState::new::<Aurora, _>(dh, |client| {
        can_view(Privileged::SecurityContext, client)
    })
}

impl SecurityContextHandler for Aurora {
    fn context_created(&mut self, source: SecurityContextListenerSource, context: SecurityContext) {
        tracing::info!(
            engine = context.sandbox_engine.as_deref().unwrap_or("?"),
            app_id = context.app_id.as_deref().unwrap_or("?"),
            "security context: new sandbox socket"
        );
        let registered = self.handle.insert_source(source, move |stream, _, state| {
            let data = ClientState {
                security_context: Some(context.clone()),
                ..ClientState::default()
            };
            if let Err(err) = state.display_handle.insert_client(stream, Arc::new(data)) {
                tracing::warn!(%err, "security context: failed to accept a sandboxed client");
            }
        });
        if let Err(err) = registered {
            tracing::warn!(%err, "security context: cannot listen on the sandbox socket");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Privileged; 8] = [
        Privileged::DataControl,
        Privileged::VirtualKeyboard,
        Privileged::InputMethod,
        Privileged::ImageCopyCapture,
        Privileged::SessionLock,
        Privileged::LayerShell,
        Privileged::ForeignToplevelList,
        Privileged::SecurityContext,
    ];

    #[test]
    fn sandboxed_clients_see_no_privileged_global() {
        for global in ALL {
            assert!(!global.allowed(true), "{global:?} leaks into the sandbox");
        }
    }

    #[test]
    fn unsandboxed_clients_see_every_global() {
        for global in ALL {
            assert!(
                global.allowed(false),
                "{global:?} hidden from normal clients"
            );
        }
    }
}
