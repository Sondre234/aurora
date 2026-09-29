//! Decides how a new window is placed: the built-in heuristics first, then the config's
//! window rules in order (later matches override earlier ones).
use aurora_layout::Constraints;

use crate::config::WindowRule;

pub struct Attrs<'a> {
    pub has_parent: bool,
    pub constraints: Constraints,
    pub app_id: &'a str,
    pub title: &'a str,
}

pub struct Decision {
    pub floating: bool,
    pub workspace: Option<u32>,
}

pub fn evaluate(attrs: &Attrs, rules: &[WindowRule]) -> Decision {
    let Constraints { min, max } = attrs.constraints;
    // Dialogs and fixed-size windows (splash screens, prompts) do not belong in the tree.
    let fixed = min.w > 0 && min.h > 0 && min == max;
    let mut decision = Decision {
        floating: attrs.has_parent || fixed,
        workspace: None,
    };
    for rule in rules.iter().filter(|r| matches(r, attrs)) {
        if let Some(floating) = rule.floating {
            decision.floating = floating;
        }
        if rule.workspace.is_some() {
            decision.workspace = rule.workspace;
        }
    }
    decision
}

/// Every given matcher must match. `class` is X11 only, so a rule naming one never matches
/// a Wayland window here.
fn matches(rule: &WindowRule, attrs: &Attrs) -> bool {
    rule.class.is_none()
        && rule
            .app_id
            .as_ref()
            .is_none_or(|g| g.is_match(attrs.app_id))
        && rule.title.as_ref().is_none_or(|g| g.is_match(attrs.title))
}
