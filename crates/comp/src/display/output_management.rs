//! `zwlr_output_manager_v1` (wlr-output-management v4): wlr-randr, wdisplays and kanshi read
//! every connected output (enabled or not) and change mode, position, scale, transform,
//! enabled and adaptive sync live.
//!
//! Reading: the heads are rebuilt from the outputs when something marked them `dirty`
//! (an output came or went, the arrangement changed, VRR toggled) and a client is bound;
//! only what changed is sent, followed by `done` with a new serial.
//!
//! Writing: a configuration must name every head and carry the latest serial (else
//! `cancelled`). `test` validates against what each head offers; `apply` turns the request
//! into runtime overrides of the `[[output]]` rules (`rules.rs`), applies them through the
//! same paths a config reload uses, checks the result and reverts on failure. Overrides are
//! runtime only: the next config reload drops them and the file wins.
use std::sync::{Arc, Mutex, PoisonError};

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols_wlr::output_management::v1::server::{
            zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
            zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
            zwlr_output_head_v1::{self, AdaptiveSyncState, ZwlrOutputHeadV1},
            zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
            zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, WEnum,
            backend::{ClientId, GlobalId},
            protocol::wl_output::Transform as WlTransform,
        },
    },
    utils::Transform,
    wayland::{Dispatch2, GlobalDispatch2},
};

use super::rules::OutputOverride;
use crate::{
    backend::Backend,
    config::{ModeSpec, VrrMode},
    state::Aurora,
};

/// Highest version served.
const VERSION: u32 = 4;
/// A custom refresh this close (mHz) to an offered mode's counts as that mode.
const REFRESH_SLACK_MHZ: i32 = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeInfo {
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
    pub preferred: bool,
}

impl ModeInfo {
    fn spec(self) -> ModeSpec {
        ModeSpec {
            width: self.width,
            height: self.height,
            refresh_mhz: u32::try_from(self.refresh_mhz).ok(),
        }
    }

    fn same(&self, other: &ModeInfo) -> bool {
        (self.width, self.height, self.refresh_mhz)
            == (other.width, other.height, other.refresh_mhz)
    }
}

/// One head as the protocol describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct HeadState {
    pub name: String,
    pub description: String,
    pub make: String,
    pub model: String,
    pub serial: String,
    /// Millimetres, `(0, 0)` when unknown.
    pub physical_size: (i32, i32),
    pub modes: Vec<ModeInfo>,
    pub enabled: bool,
    /// Index into `modes`.
    pub current_mode: Option<usize>,
    pub position: (i32, i32),
    pub transform: Transform,
    pub scale: f64,
    pub adaptive_sync: bool,
    /// False for outputs whose mode and power the compositor does not own (nested window,
    /// QA outputs): those only take position, scale and transform.
    pub can_modeset: bool,
    pub vrr_capable: bool,
}

impl HeadState {
    /// A head for a live output: mode list, mode, place, scale and transform from it.
    pub fn from_output(output: &Output, can_modeset: bool) -> Self {
        let props = output.physical_properties();
        let preferred = output.preferred_mode();
        let modes: Vec<ModeInfo> = output
            .modes()
            .iter()
            .map(|m| ModeInfo {
                width: m.size.w,
                height: m.size.h,
                refresh_mhz: m.refresh,
                preferred: Some(*m) == preferred,
            })
            .collect();
        let current = output.current_mode();
        let current_mode = current.and_then(|c| {
            modes
                .iter()
                .position(|m| (m.width, m.height, m.refresh_mhz) == (c.size.w, c.size.h, c.refresh))
        });
        let location = output.current_location();
        Self {
            name: output.name(),
            description: output.description(),
            make: props.make,
            model: props.model,
            serial: props.serial_number,
            physical_size: (props.size.w, props.size.h),
            modes,
            enabled: true,
            current_mode,
            position: (location.x, location.y),
            // A nested window's transform is how it draws, not a rotation.
            transform: if can_modeset {
                output.current_transform()
            } else {
                Transform::Normal
            },
            scale: output.current_scale().fractional_scale(),
            adaptive_sync: false,
            can_modeset,
            vrr_capable: false,
        }
    }
}

/// Which events a head needs to go from `old` to `new`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeadChanges {
    /// Something sent only once (name, modes, make...) changed: finish and recreate the head.
    pub recreate: bool,
    pub enabled: bool,
    pub current_mode: bool,
    pub position: bool,
    pub transform: bool,
    pub scale: bool,
    pub adaptive_sync: bool,
}

impl HeadChanges {
    pub fn any(&self) -> bool {
        *self != Self::default()
    }
}

/// The events that turn a client's view of `old` into `new`. Properties of a disabled head
/// are irrelevant, so they are only sent when it is (or becomes) enabled.
pub fn head_changes(old: &HeadState, new: &HeadState) -> HeadChanges {
    let once = |h: &HeadState| {
        (
            h.name.clone(),
            h.description.clone(),
            h.make.clone(),
            h.model.clone(),
            h.serial.clone(),
            h.physical_size,
            h.modes.clone(),
        )
    };
    if once(old) != once(new) {
        return HeadChanges {
            recreate: true,
            ..Default::default()
        };
    }
    let fresh = new.enabled && !old.enabled;
    HeadChanges {
        recreate: false,
        enabled: old.enabled != new.enabled,
        current_mode: new.enabled && (fresh || old.current_mode != new.current_mode),
        position: new.enabled && (fresh || old.position != new.position),
        transform: new.enabled && (fresh || old.transform != new.transform),
        scale: new.enabled && (fresh || old.scale != new.scale),
        adaptive_sync: old.adaptive_sync != new.adaptive_sync,
    }
}

/// A mode the client picked: one the head offers, or a custom size and refresh.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ModeChoice {
    Offered(ModeInfo),
    Custom {
        width: i32,
        height: i32,
        refresh_mhz: i32,
    },
}

/// What one `zwlr_output_configuration_head_v1` asked for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeadRequest {
    pub mode: Option<ModeChoice>,
    pub position: Option<(i32, i32)>,
    pub transform: Option<Transform>,
    pub scale: Option<f64>,
    pub adaptive_sync: Option<bool>,
}

/// One head of a configuration: enabled with its request, or disabled.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestedHead {
    pub name: String,
    pub enabled: bool,
    pub request: HeadRequest,
}

/// What an applied configuration must lead to, checked afterwards.
#[derive(Clone, Debug, PartialEq)]
pub struct Planned {
    pub name: String,
    pub enabled: bool,
    pub mode: Option<ModeInfo>,
}

/// Turns a configuration into overrides, or says why it cannot be applied. Pure: `heads` is
/// the current state, `requested` the whole configuration (every head named once).
pub fn plan(
    heads: &[HeadState],
    requested: &[RequestedHead],
) -> Result<Vec<(Planned, OutputOverride)>, String> {
    let mut out = Vec::new();
    for req in requested {
        let Some(head) = heads.iter().find(|h| h.name == req.name) else {
            return Err(format!("{} is gone", req.name));
        };
        let name = &head.name;
        if !req.enabled {
            if !head.can_modeset {
                return Err(format!("{name} cannot be disabled"));
            }
            out.push((
                Planned {
                    name: name.clone(),
                    enabled: false,
                    mode: None,
                },
                OutputOverride {
                    enabled: Some(false),
                    ..Default::default()
                },
            ));
            continue;
        }
        let r = &req.request;
        let mode = match r.mode {
            Some(ModeChoice::Offered(m)) => Some(
                *head
                    .modes
                    .iter()
                    .find(|o| o.same(&m))
                    .ok_or_else(|| format!("{name} does not offer that mode"))?,
            ),
            Some(ModeChoice::Custom {
                width,
                height,
                refresh_mhz,
            }) => {
                let same_size = head
                    .modes
                    .iter()
                    .filter(|m| (m.width, m.height) == (width, height));
                let found = if refresh_mhz == 0 {
                    same_size.max_by_key(|m| m.refresh_mhz)
                } else {
                    same_size
                        .filter(|m| (m.refresh_mhz - refresh_mhz).abs() <= REFRESH_SLACK_MHZ)
                        .min_by_key(|m| (m.refresh_mhz - refresh_mhz).abs())
                };
                Some(*found.ok_or_else(|| {
                    format!(
                        "{name} has no {width}x{height}@{:.3} mode (custom modes are not supported)",
                        f64::from(refresh_mhz) / 1000.0
                    )
                })?)
            }
            None => None,
        };
        let current = head.current_mode.and_then(|i| head.modes.get(i));
        if !head.can_modeset && mode.is_some_and(|m| current.is_none_or(|c| !c.same(&m))) {
            return Err(format!("{name} cannot change its mode"));
        }
        if !head.can_modeset && r.transform.is_some_and(|t| t != Transform::Normal) {
            return Err(format!("{name} cannot be rotated"));
        }
        if let Some(scale) = r.scale
            && !(0.25..=8.0).contains(&scale)
        {
            return Err(format!(
                "scale {scale} of {name} is out of range (0.25..=8)"
            ));
        }
        if let Some((x, y)) = r.position
            && ![x, y].iter().all(|v| (-100_000..=100_000).contains(v))
        {
            return Err(format!("position of {name} is out of range"));
        }
        let vrr = match r.adaptive_sync {
            // Unchanged keeps the configured policy, so on-demand survives a round trip.
            Some(want) if want == head.adaptive_sync => None,
            Some(true) if !head.vrr_capable => {
                return Err(format!("{name} does not support adaptive sync"));
            }
            Some(true) => Some(VrrMode::On),
            Some(false) => Some(VrrMode::Off),
            None => None,
        };
        out.push((
            Planned {
                name: name.clone(),
                enabled: true,
                mode,
            },
            OutputOverride {
                enabled: Some(true),
                mode: mode.map(ModeInfo::spec),
                position: r.position,
                scale: r.scale,
                transform: r.transform,
                vrr,
            },
        ));
    }
    if !out.iter().any(|(p, _)| p.enabled) {
        return Err("at least one output must stay enabled".into());
    }
    Ok(out)
}

/// Whether the outputs ended up as planned: enabled state and mode.
pub fn verify(heads: &[HeadState], planned: &[Planned]) -> Result<(), String> {
    for p in planned {
        let head = heads.iter().find(|h| h.name == p.name);
        let enabled = head.is_some_and(|h| h.enabled);
        if enabled != p.enabled {
            return Err(format!(
                "{} did not become {}",
                p.name,
                if p.enabled { "enabled" } else { "disabled" }
            ));
        }
        if let (Some(want), Some(head)) = (p.mode, head) {
            let current = head.current_mode.and_then(|i| head.modes.get(i));
            if current.is_none_or(|c| !c.same(&want)) {
                return Err(format!("{} refused the mode", p.name));
            }
        }
    }
    Ok(())
}

fn transform_from_wl(t: WlTransform) -> Transform {
    match t {
        WlTransform::_90 => Transform::_90,
        WlTransform::_180 => Transform::_180,
        WlTransform::_270 => Transform::_270,
        WlTransform::Flipped => Transform::Flipped,
        WlTransform::Flipped90 => Transform::Flipped90,
        WlTransform::Flipped180 => Transform::Flipped180,
        WlTransform::Flipped270 => Transform::Flipped270,
        _ => Transform::Normal,
    }
}

/// One head object of one manager, with its mode objects in the order of `HeadState::modes`.
struct HeadObj {
    name: String,
    head: ZwlrOutputHeadV1,
    modes: Vec<ZwlrOutputModeV1>,
}

impl HeadObj {
    fn finish(&self) {
        for mode in &self.modes {
            mode.finished();
        }
        self.head.finished();
    }
}

struct ManagerEntry {
    manager: ZwlrOutputManagerV1,
    heads: Vec<HeadObj>,
}

pub struct OutputManagementState {
    _global: GlobalId,
    managers: Vec<ManagerEntry>,
    /// The heads may have changed; recomputed on the next loop turn when a client listens.
    pub dirty: bool,
    serial: u32,
    /// What the bound clients were last told.
    last: Vec<HeadState>,
}

impl OutputManagementState {
    pub fn new(dh: &DisplayHandle) -> Self {
        Self {
            _global: dh.create_global::<Aurora, ZwlrOutputManagerV1, _>(VERSION, ManagerGlobal),
            managers: Vec::new(),
            dirty: true,
            serial: 1,
            last: Vec::new(),
        }
    }
}

/// Creates a head and its modes for `manager` and sends everything about it.
fn send_head(
    dh: &DisplayHandle,
    client: &Client,
    manager: &ZwlrOutputManagerV1,
    state: &HeadState,
) -> Option<HeadObj> {
    let version = manager.version();
    let head = client
        .create_resource::<ZwlrOutputHeadV1, _, Aurora>(
            dh,
            version,
            HeadData {
                name: state.name.clone(),
            },
        )
        .ok()?;
    manager.head(&head);
    head.name(state.name.clone());
    head.description(state.description.clone());
    if state.physical_size.0 > 0 && state.physical_size.1 > 0 {
        head.physical_size(state.physical_size.0, state.physical_size.1);
    }
    let mut modes = Vec::with_capacity(state.modes.len());
    for info in &state.modes {
        let Ok(mode) = client.create_resource::<ZwlrOutputModeV1, _, Aurora>(
            dh,
            version,
            ModeData {
                head: state.name.clone(),
                mode: *info,
            },
        ) else {
            continue;
        };
        head.mode(&mode);
        mode.size(info.width, info.height);
        if info.refresh_mhz > 0 {
            mode.refresh(info.refresh_mhz);
        }
        if info.preferred {
            mode.preferred();
        }
        modes.push(mode);
    }
    if version >= zwlr_output_head_v1::EVT_MAKE_SINCE {
        head.make(state.make.clone());
        head.model(state.model.clone());
    }
    if version >= zwlr_output_head_v1::EVT_SERIAL_NUMBER_SINCE {
        head.serial_number(state.serial.clone());
    }
    let obj = HeadObj {
        name: state.name.clone(),
        head,
        modes,
    };
    let all = HeadChanges {
        recreate: false,
        enabled: true,
        current_mode: state.enabled,
        position: state.enabled,
        transform: state.enabled,
        scale: state.enabled,
        adaptive_sync: true,
    };
    send_changes(&obj, state, all);
    Some(obj)
}

fn send_changes(obj: &HeadObj, state: &HeadState, changes: HeadChanges) {
    let head = &obj.head;
    if changes.enabled {
        head.enabled(i32::from(state.enabled));
    }
    if changes.current_mode
        && let Some(mode) = state.current_mode.and_then(|i| obj.modes.get(i))
    {
        head.current_mode(mode);
    }
    if changes.position {
        head.position(state.position.0, state.position.1);
    }
    if changes.transform {
        head.transform(state.transform.into());
    }
    if changes.scale {
        head.scale(state.scale);
    }
    if changes.adaptive_sync && head.version() >= zwlr_output_head_v1::EVT_ADAPTIVE_SYNC_SINCE {
        head.adaptive_sync(if state.adaptive_sync {
            AdaptiveSyncState::Enabled
        } else {
            AdaptiveSyncState::Disabled
        });
    }
}

impl Aurora {
    /// Every connected output, enabled or not, sorted by name.
    pub fn output_heads(&self) -> Vec<HeadState> {
        let mut heads = match self.backend {
            Backend::Drm(_) => self.drm_heads(),
            Backend::Winit => Vec::new(),
        };
        for output in &self.wm.outputs {
            if !heads.iter().any(|h| h.name == output.name()) {
                heads.push(HeadState::from_output(output, false));
            }
        }
        heads.sort_by(|a, b| a.name.cmp(&b.name));
        heads
    }

    /// Runs once per loop turn: sends what changed to every bound manager.
    pub fn display_update(&mut self) {
        let om = &mut self.display.output_management;
        if !om.dirty {
            return;
        }
        om.dirty = false;
        if om.managers.is_empty() {
            // Recomputed in full at the next bind.
            om.last.clear();
            return;
        }
        let heads = self.output_heads();
        let om = &mut self.display.output_management;
        if heads == om.last {
            return;
        }
        om.serial = om.serial.wrapping_add(1).max(1);
        let dh = self.display_handle.clone();
        for entry in &mut om.managers {
            let Some(client) = entry.manager.client() else {
                continue;
            };
            // Gone heads.
            entry.heads.retain(|obj| {
                let keep = heads.iter().any(|h| h.name == obj.name);
                if !keep {
                    obj.finish();
                }
                keep
            });
            for state in &heads {
                let index = entry.heads.iter().position(|o| o.name == state.name);
                let old = om.last.iter().find(|h| h.name == state.name);
                match (index, old) {
                    (Some(i), Some(old)) => {
                        let changes = head_changes(old, state);
                        if changes.recreate {
                            entry.heads[i].finish();
                            entry.heads.remove(i);
                            if let Some(obj) = send_head(&dh, &client, &entry.manager, state) {
                                entry.heads.push(obj);
                            }
                        } else if changes.any() {
                            send_changes(&entry.heads[i], state, changes);
                        }
                    }
                    (index, _) => {
                        if let Some(i) = index {
                            entry.heads[i].finish();
                            entry.heads.remove(i);
                        }
                        if let Some(obj) = send_head(&dh, &client, &entry.manager, state) {
                            entry.heads.push(obj);
                        }
                    }
                }
            }
            entry.manager.done(om.serial);
        }
        om.last = heads;
    }

    /// `test` (`apply == false`) or `apply` of a whole configuration; the result event
    /// goes to `config`.
    fn output_configure(
        &mut self,
        config: &ZwlrOutputConfigurationV1,
        serial: u32,
        requested: Vec<RequestedHead>,
        apply: bool,
    ) {
        // A change not yet sent makes the client's view stale as well.
        self.display_update();
        if serial != self.display.output_management.serial {
            tracing::info!("output-management: configuration cancelled (outdated serial)");
            config.cancelled();
            return;
        }
        let heads = self.output_heads();
        if let Some(missing) = heads
            .iter()
            .find(|h| !requested.iter().any(|r| r.name == h.name))
        {
            config.post_error(
                zwlr_output_configuration_v1::Error::UnconfiguredHead,
                format!("head {} is not configured", missing.name),
            );
            return;
        }
        let what = if apply { "apply" } else { "test" };
        let planned = match plan(&heads, &requested) {
            Ok(planned) => planned,
            Err(err) => {
                tracing::info!("output-management: {what} failed: {err}");
                config.failed();
                return;
            }
        };
        if !apply {
            tracing::info!("output-management: test succeeded");
            config.succeeded();
            return;
        }
        let previous = self.display.overrides.clone();
        for (p, o) in &planned {
            let entry = self.display.overrides.entry(p.name.clone()).or_default();
            entry.enabled = o.enabled;
            if p.enabled {
                entry.mode = o.mode.or(entry.mode);
                entry.position = o.position.or(entry.position);
                entry.scale = o.scale.or(entry.scale);
                entry.transform = o.transform.or(entry.transform);
                entry.vrr = o.vrr.or(entry.vrr);
            }
        }
        self.apply_output_rules();
        let planned: Vec<Planned> = planned.into_iter().map(|(p, _)| p).collect();
        match verify(&self.output_heads(), &planned) {
            Ok(()) => {
                tracing::info!("output-management: applied {} head(s)", planned.len());
                config.succeeded();
            }
            Err(err) => {
                tracing::warn!("output-management: apply failed, reverting: {err}");
                self.display.overrides = previous;
                self.apply_output_rules();
                config.failed();
            }
        }
        self.display.output_management.dirty = true;
    }

    /// Recomputes the rules from config and overrides and applies them like a reload does.
    fn apply_output_rules(&mut self) {
        self.display_rules_changed();
        self.drm_apply_output_config();
        self.reapply_output_config();
        // VRR follows the rules at the next frame.
        self.queue_redraw_all();
    }
}

pub struct ManagerGlobal;
pub struct ManagerData;
pub struct HeadData {
    name: String,
}
pub struct ModeData {
    head: String,
    mode: ModeInfo,
}

#[derive(Default)]
struct ConfigInner {
    used: bool,
    /// Every head named so far: `Some` is enabled with that request, `None` disabled.
    heads: Vec<(String, Option<Arc<Mutex<HeadRequest>>>)>,
}

pub struct ConfigData {
    serial: u32,
    inner: Mutex<ConfigInner>,
}

pub struct ConfigHeadData {
    name: String,
    request: Arc<Mutex<HeadRequest>>,
}

impl GlobalDispatch2<ZwlrOutputManagerV1, Aurora> for ManagerGlobal {
    fn bind(
        &self,
        state: &mut Aurora,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        let manager = data_init.init(resource, ManagerData);
        // Anything pending goes to the managers that already listen first.
        state.display_update();
        if state.display.output_management.managers.is_empty() {
            state.display.output_management.last = state.output_heads();
        }
        let om = &mut state.display.output_management;
        let heads = om
            .last
            .iter()
            .filter_map(|h| send_head(dh, client, &manager, h))
            .collect();
        manager.done(om.serial);
        om.managers.push(ManagerEntry { manager, heads });
    }

    fn can_view(&self, client: &Client) -> bool {
        crate::sandbox::can_view(crate::sandbox::Privileged::OutputManagement, client)
    }
}

impl Dispatch2<ZwlrOutputManagerV1, Aurora> for ManagerData {
    fn request(
        &self,
        state: &mut Aurora,
        _client: &Client,
        resource: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    ConfigData {
                        serial,
                        inner: Mutex::default(),
                    },
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                state
                    .display
                    .output_management
                    .managers
                    .retain(|e| e.manager != *resource);
                resource.finished();
            }
            _ => {}
        }
    }

    fn destroyed(&self, state: &mut Aurora, _client: ClientId, resource: &ZwlrOutputManagerV1) {
        state
            .display
            .output_management
            .managers
            .retain(|e| e.manager != *resource);
    }
}

impl Dispatch2<ZwlrOutputHeadV1, Aurora> for HeadData {
    fn request(
        &self,
        _state: &mut Aurora,
        _client: &Client,
        _resource: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Aurora>,
    ) {
        // `release` only; events to a released head are dropped by wayland-server.
    }
}

impl Dispatch2<ZwlrOutputModeV1, Aurora> for ModeData {
    fn request(
        &self,
        _state: &mut Aurora,
        _client: &Client,
        _resource: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Aurora>,
    ) {
    }
}

fn head_name(head: &ZwlrOutputHeadV1) -> Option<String> {
    head.data::<HeadData>().map(|d| d.name.clone())
}

impl Dispatch2<ZwlrOutputConfigurationV1, Aurora> for ConfigData {
    fn request(
        &self,
        state: &mut Aurora,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        use zwlr_output_configuration_v1::{Error, Request};
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.used && !matches!(request, Request::Destroy) {
            resource.post_error(Error::AlreadyUsed, "the configuration was already used");
            return;
        }
        let (head, enable) = match request {
            Request::EnableHead { id, head } => (head, Some(id)),
            Request::DisableHead { head } => (head, None),
            Request::Apply | Request::Test => {
                inner.used = true;
                let requested = inner
                    .heads
                    .iter()
                    .map(|(name, req)| RequestedHead {
                        name: name.clone(),
                        enabled: req.is_some(),
                        request: req
                            .as_ref()
                            .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner).clone())
                            .unwrap_or_default(),
                    })
                    .collect();
                drop(inner);
                let apply = matches!(request, Request::Apply);
                state.output_configure(resource, self.serial, requested, apply);
                return;
            }
            _ => return,
        };
        let Some(name) = head_name(&head) else {
            return;
        };
        if inner.heads.iter().any(|(n, _)| *n == name) {
            resource.post_error(
                Error::AlreadyConfiguredHead,
                format!("head {name} is configured twice"),
            );
            return;
        }
        let request = enable.map(|id| {
            let request = Arc::new(Mutex::new(HeadRequest::default()));
            data_init.init(
                id,
                ConfigHeadData {
                    name: name.clone(),
                    request: request.clone(),
                },
            );
            request
        });
        inner.heads.push((name, request));
    }
}

impl Dispatch2<ZwlrOutputConfigurationHeadV1, Aurora> for ConfigHeadData {
    fn request(
        &self,
        _state: &mut Aurora,
        _client: &Client,
        resource: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Aurora>,
    ) {
        use zwlr_output_configuration_head_v1::{Error, Request};
        let mut req = self.request.lock().unwrap_or_else(PoisonError::into_inner);
        let already = |what: &str| {
            resource.post_error(Error::AlreadySet, format!("{what} is already set"));
        };
        match request {
            Request::SetMode { mode } => {
                if req.mode.is_some() {
                    return already("the mode");
                }
                match mode.data::<ModeData>() {
                    Some(data) if data.head == self.name => {
                        req.mode = Some(ModeChoice::Offered(data.mode));
                    }
                    _ => resource.post_error(Error::InvalidMode, "the mode is not this head's"),
                }
            }
            Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                if req.mode.is_some() {
                    return already("the mode");
                }
                if width <= 0 || height <= 0 || refresh < 0 {
                    resource.post_error(Error::InvalidCustomMode, "invalid custom mode");
                    return;
                }
                req.mode = Some(ModeChoice::Custom {
                    width,
                    height,
                    refresh_mhz: refresh,
                });
            }
            Request::SetPosition { x, y } => {
                if req.position.is_some() {
                    return already("the position");
                }
                req.position = Some((x, y));
            }
            Request::SetTransform { transform } => {
                if req.transform.is_some() {
                    return already("the transform");
                }
                match transform {
                    WEnum::Value(t) => req.transform = Some(transform_from_wl(t)),
                    WEnum::Unknown(_) => {
                        resource.post_error(Error::InvalidTransform, "unknown transform")
                    }
                }
            }
            Request::SetScale { scale } => {
                if req.scale.is_some() {
                    return already("the scale");
                }
                if !(scale.is_finite() && scale > 0.0) {
                    resource.post_error(Error::InvalidScale, "the scale must be positive");
                    return;
                }
                req.scale = Some(scale);
            }
            Request::SetAdaptiveSync { state } => {
                if req.adaptive_sync.is_some() {
                    return already("adaptive sync");
                }
                match state {
                    WEnum::Value(AdaptiveSyncState::Enabled) => req.adaptive_sync = Some(true),
                    WEnum::Value(AdaptiveSyncState::Disabled) => req.adaptive_sync = Some(false),
                    _ => resource.post_error(
                        Error::InvalidAdaptiveSyncState,
                        "unknown adaptive sync state",
                    ),
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: i32, height: i32, hz: i32) -> ModeInfo {
        ModeInfo {
            width,
            height,
            refresh_mhz: hz * 1000,
            preferred: false,
        }
    }

    fn head(name: &str) -> HeadState {
        HeadState {
            name: name.into(),
            description: format!("Make - Model - {name}"),
            make: "Make".into(),
            model: "Model".into(),
            serial: "1".into(),
            physical_size: (600, 340),
            modes: vec![
                mode(2560, 1440, 144),
                mode(2560, 1440, 60),
                mode(1920, 1080, 60),
            ],
            enabled: true,
            current_mode: Some(0),
            position: (0, 0),
            transform: Transform::Normal,
            scale: 1.25,
            adaptive_sync: false,
            can_modeset: true,
            vrr_capable: true,
        }
    }

    fn enable(name: &str, request: HeadRequest) -> RequestedHead {
        RequestedHead {
            name: name.into(),
            enabled: true,
            request,
        }
    }

    fn disable(name: &str) -> RequestedHead {
        RequestedHead {
            name: name.into(),
            enabled: false,
            request: HeadRequest::default(),
        }
    }

    #[test]
    fn unchanged_head_sends_nothing() {
        let h = head("DP-1");
        assert!(!head_changes(&h, &h.clone()).any());
    }

    #[test]
    fn changes_list_only_what_moved() {
        let old = head("DP-1");
        let mut new = old.clone();
        new.position = (2048, 0);
        new.adaptive_sync = true;
        let c = head_changes(&old, &new);
        assert!(c.position && c.adaptive_sync);
        assert!(!c.recreate && !c.enabled && !c.scale && !c.transform && !c.current_mode);
    }

    #[test]
    fn enabling_resends_every_enabled_property_and_mode_lists_recreate() {
        let mut old = head("DP-1");
        old.enabled = false;
        let new = head("DP-1");
        let c = head_changes(&old, &new);
        assert!(c.enabled && c.current_mode && c.position && c.transform && c.scale);
        let disabled = head_changes(&new, &old);
        assert!(disabled.enabled && !disabled.position && !disabled.scale);

        let mut fewer = head("DP-1");
        fewer.modes.pop();
        assert!(head_changes(&new, &fewer).recreate);
    }

    #[test]
    fn plan_turns_requests_into_overrides() {
        let heads = [head("DP-1"), head("DP-3")];
        let requested = [
            enable(
                "DP-1",
                HeadRequest {
                    mode: Some(ModeChoice::Offered(mode(2560, 1440, 60))),
                    position: Some((0, 0)),
                    scale: Some(1.0),
                    transform: Some(Transform::_90),
                    adaptive_sync: Some(true),
                },
            ),
            disable("DP-3"),
        ];
        let planned = plan(&heads, &requested).expect("valid");
        let (p, o) = &planned[0];
        assert_eq!(p.mode, Some(mode(2560, 1440, 60)));
        assert_eq!(o.enabled, Some(true));
        assert_eq!(
            o.mode,
            Some(ModeSpec {
                width: 2560,
                height: 1440,
                refresh_mhz: Some(60_000)
            })
        );
        assert_eq!(o.scale, Some(1.0));
        assert_eq!(o.transform, Some(Transform::_90));
        assert_eq!(o.vrr, Some(VrrMode::On));
        assert_eq!(planned[1].1.enabled, Some(false));
        assert_eq!(planned[1].1.mode, None);
    }

    #[test]
    fn unchanged_adaptive_sync_keeps_the_configured_policy() {
        let heads = [head("DP-1")];
        let request = HeadRequest {
            adaptive_sync: Some(false),
            ..Default::default()
        };
        let planned = plan(&heads, &[enable("DP-1", request)]).expect("valid");
        assert_eq!(planned[0].1.vrr, None);
        let mut no_vrr = head("DP-1");
        no_vrr.vrr_capable = false;
        let request = HeadRequest {
            adaptive_sync: Some(true),
            ..Default::default()
        };
        assert!(plan(&[no_vrr], &[enable("DP-1", request)]).is_err());
    }

    #[test]
    fn custom_modes_must_match_an_offered_one() {
        let heads = [head("DP-1")];
        let custom = |w, h, r| HeadRequest {
            mode: Some(ModeChoice::Custom {
                width: w,
                height: h,
                refresh_mhz: r,
            }),
            ..Default::default()
        };
        let got = |r| plan(&heads, &[enable("DP-1", r)]).map(|p| p[0].0.mode);
        assert_eq!(
            got(custom(2560, 1440, 59_951)),
            Ok(Some(mode(2560, 1440, 60)))
        );
        assert_eq!(got(custom(2560, 1440, 0)), Ok(Some(mode(2560, 1440, 144))));
        assert!(got(custom(2560, 1440, 120_000)).is_err());
        assert!(got(custom(800, 600, 0)).is_err());
    }

    #[test]
    fn plan_rejects_what_cannot_work() {
        let heads = [head("DP-1"), head("DP-3")];
        assert!(
            plan(&heads, &[disable("DP-1"), disable("DP-3")]).is_err(),
            "all off"
        );
        let bad_scale = HeadRequest {
            scale: Some(9.0),
            ..Default::default()
        };
        assert!(plan(&heads, &[enable("DP-1", bad_scale), disable("DP-3")]).is_err());
        assert!(plan(&heads, &[enable("HDMI-A-9", HeadRequest::default())]).is_err());
        let mut nested = head("winit");
        nested.can_modeset = false;
        assert!(plan(&[nested.clone()], &[disable("winit")]).is_err());
        let other_mode = HeadRequest {
            mode: Some(ModeChoice::Offered(mode(1920, 1080, 60))),
            ..Default::default()
        };
        assert!(plan(&[nested.clone()], &[enable("winit", other_mode)]).is_err());
        let rotate = HeadRequest {
            transform: Some(Transform::_90),
            ..Default::default()
        };
        assert!(plan(&[nested.clone()], &[enable("winit", rotate)]).is_err());
        let place = HeadRequest {
            position: Some((10, 20)),
            scale: Some(2.0),
            ..Default::default()
        };
        assert!(plan(&[nested], &[enable("winit", place)]).is_ok());
    }

    #[test]
    fn verify_checks_enabled_and_mode() {
        let mut heads = vec![head("DP-1"), head("DP-3")];
        let planned = [
            Planned {
                name: "DP-1".into(),
                enabled: true,
                mode: Some(mode(2560, 1440, 60)),
            },
            Planned {
                name: "DP-3".into(),
                enabled: false,
                mode: None,
            },
        ];
        assert!(verify(&heads, &planned).is_err(), "DP-1 still at 144");
        heads[0].current_mode = Some(1);
        assert!(verify(&heads, &planned).is_err(), "DP-3 still on");
        heads[1].enabled = false;
        assert_eq!(verify(&heads, &planned), Ok(()));
    }
}
