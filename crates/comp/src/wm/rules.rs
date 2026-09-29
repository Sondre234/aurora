//! Decides how a new window is placed: the built-in heuristics first, then the config's
//! window rules in order (later matches override earlier ones).
use aurora_layout::Constraints;

use crate::config::WindowRule;

pub struct Attrs<'a> {
    pub has_parent: bool,
    pub constraints: Constraints,
    pub app_id: &'a str,
    pub title: &'a str,
    /// X11 window: `app_id` holds its WM_CLASS class, which only `class` rules match.
    pub x11: bool,
}

pub struct Decision {
    pub floating: bool,
    pub workspace: Option<u32>,
    /// Connector name; the window goes to the workspace that output shows (unless `workspace` is set).
    pub output: Option<String>,
    /// Content size for a floating window.
    pub size: Option<(i32, i32)>,
    pub fullscreen: bool,
}

pub fn evaluate(attrs: &Attrs, rules: &[WindowRule]) -> Decision {
    let Constraints { min, max } = attrs.constraints;
    // Dialogs and fixed-size windows (splash screens, prompts) do not belong in the tree.
    let fixed = min.w > 0 && min.h > 0 && min == max;
    let mut decision = Decision {
        floating: attrs.has_parent || fixed,
        workspace: None,
        output: None,
        size: None,
        fullscreen: false,
    };
    for rule in rules.iter().filter(|r| matches(r, attrs)) {
        if let Some(floating) = rule.floating {
            decision.floating = floating;
        }
        if rule.workspace.is_some() {
            decision.workspace = rule.workspace;
        }
        if rule.output.is_some() {
            decision.output.clone_from(&rule.output);
        }
        if rule.size.is_some() {
            decision.size = rule.size;
        }
        if let Some(fullscreen) = rule.fullscreen {
            decision.fullscreen = fullscreen;
        }
    }
    decision
}

/// Every given matcher must match. `class` is X11 only and `app_id` Wayland only.
fn matches(rule: &WindowRule, attrs: &Attrs) -> bool {
    rule.class
        .as_ref()
        .is_none_or(|g| attrs.x11 && g.is_match(attrs.app_id))
        && rule
            .app_id
            .as_ref()
            .is_none_or(|g| !attrs.x11 && g.is_match(attrs.app_id))
        && rule.title.as_ref().is_none_or(|g| g.is_match(attrs.title))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Glob;
    use aurora_layout::Size;

    fn rule(app_id: &str) -> WindowRule {
        WindowRule {
            app_id: Some(Glob::new(app_id)),
            title: None,
            class: None,
            floating: None,
            workspace: None,
            output: None,
            size: None,
            fullscreen: None,
        }
    }

    #[test]
    fn heuristics_then_rules_in_order() {
        let attrs = |constraints| Attrs {
            has_parent: false,
            constraints,
            app_id: "pavucontrol",
            title: "",
            x11: false,
        };
        let fixed = Constraints {
            min: Size { w: 300, h: 200 },
            max: Size { w: 300, h: 200 },
        };
        assert!(evaluate(&attrs(fixed), &[]).floating);
        assert!(!evaluate(&attrs(Constraints::default()), &[]).floating);

        let mut float = rule("pavu*");
        float.floating = Some(true);
        float.size = Some((640, 480));
        let mut tile = rule("pavucontrol");
        tile.floating = Some(false);
        tile.fullscreen = Some(true);
        let d = evaluate(&attrs(Constraints::default()), &[float, tile]);
        assert!(!d.floating && d.fullscreen);
        assert_eq!(d.size, Some((640, 480)));

        // Class rules see X11 windows only; app_id rules Wayland ones only.
        let mut by_class = rule("unused");
        by_class.app_id = None;
        by_class.class = Some(Glob::new("steam_app_*"));
        by_class.workspace = Some(5);
        let x11 = Attrs {
            x11: true,
            app_id: "steam_app_7",
            ..attrs(Constraints::default())
        };
        assert_eq!(evaluate(&x11, &[by_class.clone()]).workspace, Some(5));
        assert_eq!(
            evaluate(&attrs(Constraints::default()), &[by_class]).workspace,
            None
        );
    }
}
