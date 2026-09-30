//! Protocol globals. Most only need to stay alive; the few with state the compositor reads
//! are public.
use std::collections::HashSet;

use smithay::{
    reexports::{
        calloop::LoopHandle,
        wayland_server::{DisplayHandle, protocol::wl_surface::WlSurface},
    },
    wayland::{
        cursor_shape::CursorShapeManagerState,
        foreign_toplevel_list::ForeignToplevelListState,
        fractional_scale::FractionalScaleManagerState,
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        keyboard_shortcuts_inhibit::{KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor},
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        selection::{primary_selection::PrimarySelectionState, wlr_data_control::DataControlState},
        session_lock::SessionLockManagerState,
        shell::{wlr_layer::WlrLayerShellState, xdg::decoration::XdgDecorationState},
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
        xwayland_shell::XWaylandShellState,
    },
};

use crate::{state::Aurora, virtual_input::VirtualKeyboardGlobal};

pub struct Protocols {
    _output_manager: OutputManagerState,
    _presentation: PresentationState,
    _fractional_scale: FractionalScaleManagerState,
    _viewporter: ViewporterState,
    _decoration: XdgDecorationState,
    _cursor_shape: CursorShapeManagerState,
    _idle_inhibit: IdleInhibitManagerState,
    _relative_pointer: RelativePointerManagerState,
    _pointer_constraints: PointerConstraintsState,
    pub layer_shell: WlrLayerShellState,
    pub primary_selection: PrimarySelectionState,
    pub data_control: DataControlState,
    pub xwayland_shell: XWaylandShellState,
    pub activation: XdgActivationState,
    pub idle_notifier: IdleNotifierState<Aurora>,
    pub shortcuts_inhibit: KeyboardShortcutsInhibitState,
    pub virtual_keyboard: VirtualKeyboardGlobal,
    pub session_lock: SessionLockManagerState,
    pub foreign_toplevel: ForeignToplevelListState,
    /// Surfaces holding an idle inhibitor; pruned by `alive()` whenever the set changes.
    pub idle_inhibitors: HashSet<WlSurface>,
    /// The inhibitor currently taking the shortcuts from the focused surface.
    pub active_inhibitor: Option<KeyboardShortcutsInhibitor>,
}

impl Protocols {
    pub fn new(
        dh: &DisplayHandle,
        handle: &LoopHandle<'static, Aurora>,
        clock_id: u32,
        allow_virtual_keyboard: bool,
    ) -> Self {
        let primary_selection = PrimarySelectionState::new::<Aurora>(dh);
        // Clipboard managers are trusted with every selection, so no filter.
        let data_control =
            DataControlState::new::<Aurora, _>(dh, Some(&primary_selection), |_| true);
        Self {
            _output_manager: OutputManagerState::new_with_xdg_output::<Aurora>(dh),
            _presentation: PresentationState::new::<Aurora>(dh, clock_id),
            _fractional_scale: FractionalScaleManagerState::new::<Aurora>(dh),
            _viewporter: ViewporterState::new::<Aurora>(dh),
            _decoration: XdgDecorationState::new::<Aurora>(dh),
            _cursor_shape: CursorShapeManagerState::new::<Aurora>(dh),
            _idle_inhibit: IdleInhibitManagerState::new::<Aurora>(dh),
            _relative_pointer: RelativePointerManagerState::new::<Aurora>(dh),
            _pointer_constraints: PointerConstraintsState::new::<Aurora>(dh),
            layer_shell: WlrLayerShellState::new::<Aurora>(dh),
            primary_selection,
            data_control,
            xwayland_shell: XWaylandShellState::new::<Aurora>(dh),
            activation: XdgActivationState::new::<Aurora>(dh),
            idle_notifier: IdleNotifierState::new(dh, handle.clone()),
            shortcuts_inhibit: KeyboardShortcutsInhibitState::new::<Aurora>(dh),
            virtual_keyboard: VirtualKeyboardGlobal::new(dh, allow_virtual_keyboard),
            session_lock: SessionLockManagerState::new::<Aurora, _>(dh, |_| true),
            foreign_toplevel: ForeignToplevelListState::new::<Aurora>(dh),
            idle_inhibitors: HashSet::new(),
            active_inhibitor: None,
        }
    }
}
