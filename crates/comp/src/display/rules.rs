//! The output rules in force: the config's `[[output]]` entries with the runtime overrides
//! from wlr-output-management on top. Overrides live until the next config reload, where the
//! file wins again.
use std::collections::BTreeMap;

use smithay::utils::Transform;

use crate::config::{ModeSpec, OutputRule, VrrMode};

/// What a client changed for one output; `None` keeps the config's value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputOverride {
    pub enabled: Option<bool>,
    pub mode: Option<ModeSpec>,
    pub position: Option<(i32, i32)>,
    pub scale: Option<f64>,
    pub transform: Option<Transform>,
    pub vrr: Option<VrrMode>,
}

/// A rule that changes nothing, for an output the config does not mention.
pub fn blank(name: &str) -> OutputRule {
    OutputRule {
        name: name.to_string(),
        enabled: true,
        primary: false,
        position: None,
        mode: None,
        scale: None,
        vrr: VrrMode::Off,
        transform: None,
    }
}

/// The config's rules with `overrides` applied, config order first, then outputs only the
/// overrides know.
pub fn effective(
    config: &[OutputRule],
    overrides: &BTreeMap<String, OutputOverride>,
) -> Vec<OutputRule> {
    let mut rules = config.to_vec();
    for (name, o) in overrides {
        let index = match rules.iter().position(|r| r.name == *name) {
            Some(i) => i,
            None => {
                rules.push(blank(name));
                rules.len() - 1
            }
        };
        let rule = &mut rules[index];
        if let Some(v) = o.enabled {
            rule.enabled = v;
        }
        if let Some(v) = o.mode {
            rule.mode = Some(v);
        }
        if let Some(v) = o.position {
            rule.position = Some(v);
        }
        if let Some(v) = o.scale {
            rule.scale = Some(v);
        }
        if let Some(v) = o.transform {
            rule.transform = Some(v);
        }
        if let Some(v) = o.vrr {
            rule.vrr = v;
        }
    }
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_win_and_unknown_outputs_get_a_rule() {
        let mut dp1 = blank("DP-1");
        dp1.scale = Some(1.25);
        dp1.position = Some((0, 0));
        dp1.vrr = VrrMode::OnDemand;
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "DP-1".to_string(),
            OutputOverride {
                position: Some((100, 0)),
                transform: Some(Transform::_90),
                ..Default::default()
            },
        );
        overrides.insert(
            "HDMI-A-1".to_string(),
            OutputOverride {
                enabled: Some(false),
                ..Default::default()
            },
        );
        let rules = effective(&[dp1], &overrides);
        assert_eq!(rules.len(), 2);
        let dp1 = &rules[0];
        assert_eq!(dp1.position, Some((100, 0)));
        assert_eq!(dp1.scale, Some(1.25), "untouched values stay");
        assert_eq!(dp1.vrr, VrrMode::OnDemand);
        assert_eq!(dp1.transform, Some(Transform::_90));
        assert_eq!(rules[1].name, "HDMI-A-1");
        assert!(!rules[1].enabled);
    }

    #[test]
    fn no_overrides_is_the_config() {
        let rules = effective(&[blank("A"), blank("B")], &BTreeMap::new());
        let names: Vec<_> = rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
    }
}
