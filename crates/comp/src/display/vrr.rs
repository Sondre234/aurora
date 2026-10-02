//! Variable refresh rate. `[[output]] vrr` picks the policy; the DRM render loop asks
//! `target` before every frame and flips the CRTC's `VRR_ENABLED` only when the answer
//! changes. `on-demand` follows sway and niri: on only while a fullscreen window is on the
//! output, so the desktop keeps a fixed refresh (no flicker on panels that dislike VRR at
//! low frame rates) and games get adaptive sync.
use crate::config::{OutputRule, VrrMode};

/// What the connector can do, from the DRM `vrr_capable` property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    Unsupported,
    /// Only with a full modeset (smithay reports this for HDMI, which flickers otherwise).
    RequiresModeset,
    Supported,
}

impl Capability {
    pub fn name(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::RequiresModeset => "requires-modeset",
            Self::Supported => "supported",
        }
    }
}

/// The policy for the output named `name` under `rules`.
pub fn mode_for(rules: &[OutputRule], name: &str) -> VrrMode {
    rules
        .iter()
        .find(|r| r.name == name)
        .map_or(VrrMode::Off, |r| r.vrr)
}

/// Whether VRR should be on now. `on-demand` stays off on connectors that need a modeset to
/// toggle it: blanking the screen on every fullscreen toggle is worse than no VRR (`on` still
/// works there, with one modeset when it is applied).
pub fn target(mode: VrrMode, fullscreen: bool, capability: Capability) -> bool {
    match (mode, capability) {
        (_, Capability::Unsupported) | (VrrMode::Off, _) => false,
        (VrrMode::On, _) => true,
        (VrrMode::OnDemand, Capability::RequiresModeset) => false,
        (VrrMode::OnDemand, Capability::Supported) => fullscreen,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_demand_follows_fullscreen_only_where_seamless() {
        use Capability::*;
        use VrrMode::*;
        let cases = [
            (Off, true, Supported, false),
            (On, false, Supported, true),
            (On, false, RequiresModeset, true),
            (On, true, Unsupported, false),
            (OnDemand, false, Supported, false),
            (OnDemand, true, Supported, true),
            (OnDemand, true, RequiresModeset, false),
            (OnDemand, true, Unsupported, false),
        ];
        for (mode, fullscreen, cap, want) in cases {
            assert_eq!(
                target(mode, fullscreen, cap),
                want,
                "{mode:?} {fullscreen} {cap:?}"
            );
        }
    }

    #[test]
    fn unconfigured_outputs_have_vrr_off() {
        let rule = OutputRule {
            name: "DP-1".into(),
            enabled: true,
            primary: false,
            position: None,
            mode: None,
            scale: None,
            vrr: VrrMode::OnDemand,
            transform: None,
        };
        assert_eq!(
            mode_for(std::slice::from_ref(&rule), "DP-1"),
            VrrMode::OnDemand
        );
        assert_eq!(mode_for(&[rule], "DP-2"), VrrMode::Off);
    }
}
