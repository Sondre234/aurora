//! Display control: monitor power (DPMS), gamma, VRR, tearing, and the protocols that let privileged clients
//! drive the outputs. The backend-specific half (what "off" means on DRM) lives in
//! `backend/drm/display.rs`; everything here is backend independent.
use std::collections::BTreeMap;

use smithay::{output::Output, reexports::wayland_server::DisplayHandle};

use crate::{
    config::{Config, OutputRule},
    state::Aurora,
};

pub mod gamma;
pub mod output_management;
pub mod power;
pub mod rules;
pub mod tearing;
pub mod vrr;

/// Protocol globals and bookkeeping of the display stream, one field of `Aurora`.
pub struct DisplayState {
    pub power: power::PowerState,
    pub gamma: gamma::GammaState,
    pub tearing: tearing::TearingState,
    pub output_management: output_management::OutputManagementState,
    /// Runtime changes from wlr-output-management, by output name, until the next reload.
    pub overrides: BTreeMap<String, rules::OutputOverride>,
    /// The config's output rules with the overrides applied; what `output_rule` reads.
    pub rules: Vec<OutputRule>,
}

impl DisplayState {
    pub fn new(dh: &DisplayHandle, config: &Config) -> Self {
        Self {
            power: power::PowerState::new(dh),
            gamma: gamma::GammaState::new(dh),
            tearing: tearing::TearingState::new(dh),
            output_management: output_management::OutputManagementState::new(dh),
            overrides: BTreeMap::new(),
            rules: config.outputs.clone(),
        }
    }
}

impl Aurora {
    /// The config was (re)loaded: it wins over every runtime override.
    pub fn display_config_reloaded(&mut self) {
        if !self.display.overrides.is_empty() {
            tracing::info!(
                "output-management: reload drops runtime changes to {} output(s)",
                self.display.overrides.len()
            );
        }
        self.display.overrides.clear();
        self.display_rules_changed();
    }

    /// Recomputes the rules in force after the overrides changed.
    pub fn display_rules_changed(&mut self) {
        self.display.rules = rules::effective(&self.config.outputs, &self.display.overrides);
    }

    /// A new output joined the layout (called from `add_output`).
    pub fn display_output_added(&mut self, _output: &Output) {
        self.display.output_management.dirty = true;
    }

    /// An output is going away and is still alive (called from `wm_output_removed`).
    pub fn display_output_removed(&mut self, output: &Output) {
        self.power_output_removed(output);
        self.gamma_output_removed(output);
        self.display.output_management.dirty = true;
    }
}
