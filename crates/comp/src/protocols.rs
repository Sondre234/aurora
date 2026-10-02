//! Protocol globals. Most only need to stay alive; the few with state the compositor reads
//! are public.
use std::collections::HashSet;

use smithay::{
    reexports::{
        calloop::LoopHandle,
        wayland_protocols::wp::content_type::v1::server::wp_content_type_v1::Type as WpContentType,
        wayland_server::{DisplayHandle, protocol::wl_surface::WlSurface},
    },
    wayland::{
        alpha_modifier::AlphaModifierState,
        compositor::with_states,
        content_type::{ContentTypeState, ContentTypeSurfaceCachedState},
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
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{decoration::XdgDecorationState, dialog::XdgDialogState},
        },
        single_pixel_buffer::SinglePixelBufferState,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
        xdg_foreign::XdgForeignState,
        xdg_toplevel_icon::XdgToplevelIconManager,
        xwayland_shell::XWaylandShellState,
    },
};

use crate::{
    sandbox::{self, Privileged},
    state::Aurora,
    virtual_input::VirtualKeyboardGlobal,
};

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
    /// Solid-colour buffers: Smithay's surface elements draw them as solid quads and the DRM
    /// compositor can scan them out, so nothing else needs to know.
    _single_pixel: SinglePixelBufferState,
    /// wp_alpha_modifier_v1: Smithay's surface elements multiply the client's factor into the
    /// alpha Aurora passes (fades, inactive opacity); the shadow follows it in `wm/window.rs`.
    _alpha_modifier: AlphaModifierState,
    /// wp_content_type_v1: stored by Smithay per surface, read with [`content_type`].
    _content_type: ContentTypeState,
    /// fifo-v1 and commit-timing-v1, signalled from the render loops (`pacing.rs`).
    _pacing: crate::pacing::Pacing,
    /// zxdg_foreign_v2: Smithay sets the imported parent, `parent_changed` re-reads it.
    pub xdg_foreign: XdgForeignState,
    /// xdg_wm_dialog_v1: modal dialogs always float, centred on their parent.
    _dialog: XdgDialogState,
    /// xdg_toplevel_icon_v1: Smithay keeps the committed icon on the surface; nothing draws
    /// it yet (`WindowElement::icon_name`).
    _toplevel_icon: XdgToplevelIconManager,
    /// wp_security_context_v1: sandboxed clients lose the privileged globals (`sandbox.rs`).
    _security_context: smithay::wayland::security_context::SecurityContextState,
    /// text-input-v3 and input-method-v2 (`ime.rs`).
    _ime: crate::ime::Ime,
    /// zwp_pointer_gestures_v1, fed from libinput in `input/gestures.rs`.
    _pointer_gestures: smithay::wayland::pointer_gestures::PointerGesturesState,
}

impl Protocols {
    pub fn new(
        dh: &DisplayHandle,
        handle: &LoopHandle<'static, Aurora>,
        clock_id: u32,
        allow_virtual_keyboard: bool,
    ) -> Self {
        let primary_selection = PrimarySelectionState::new::<Aurora>(dh);
        // Clipboard managers are trusted with every selection; sandboxes are not.
        let data_control = DataControlState::new::<Aurora, _>(dh, Some(&primary_selection), |c| {
            sandbox::can_view(Privileged::DataControl, c)
        });
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
            layer_shell: WlrLayerShellState::new_with_filter::<Aurora, _>(dh, |c| {
                sandbox::can_view(Privileged::LayerShell, c)
            }),
            primary_selection,
            data_control,
            xwayland_shell: XWaylandShellState::new::<Aurora>(dh),
            activation: XdgActivationState::new::<Aurora>(dh),
            idle_notifier: IdleNotifierState::new(dh, handle.clone()),
            shortcuts_inhibit: KeyboardShortcutsInhibitState::new::<Aurora>(dh),
            virtual_keyboard: VirtualKeyboardGlobal::new(dh, allow_virtual_keyboard),
            session_lock: SessionLockManagerState::new::<Aurora, _>(dh, |c| {
                sandbox::can_view(Privileged::SessionLock, c)
            }),
            foreign_toplevel: ForeignToplevelListState::new_with_filter::<Aurora>(dh, |c| {
                sandbox::can_view(Privileged::ForeignToplevelList, c)
            }),
            idle_inhibitors: HashSet::new(),
            active_inhibitor: None,
            _single_pixel: SinglePixelBufferState::new::<Aurora>(dh),
            _alpha_modifier: AlphaModifierState::new::<Aurora>(dh),
            _content_type: ContentTypeState::new::<Aurora>(dh),
            _pacing: crate::pacing::Pacing::new(dh),
            xdg_foreign: XdgForeignState::new::<Aurora>(dh),
            _dialog: XdgDialogState::new::<Aurora>(dh),
            _toplevel_icon: XdgToplevelIconManager::new::<Aurora>(dh),
            _security_context: sandbox::init(dh),
            _ime: crate::ime::Ime::new(dh),
            _pointer_gestures: smithay::wayland::pointer_gestures::PointerGesturesState::new::<
                Aurora,
            >(dh),
        }
    }
}

/// What a surface says it shows (wp_content_type_v1), for policy such as on-demand VRR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentType {
    #[default]
    None,
    Photo,
    Video,
    Game,
}

/// The committed content type of `surface`; `None` when the client never set one.
pub fn content_type(surface: &WlSurface) -> ContentType {
    with_states(surface, |states| {
        match states
            .cached_state
            .get::<ContentTypeSurfaceCachedState>()
            .current()
            .content_type()
        {
            WpContentType::Photo => ContentType::Photo,
            WpContentType::Video => ContentType::Video,
            WpContentType::Game => ContentType::Game,
            _ => ContentType::None,
        }
    })
}
