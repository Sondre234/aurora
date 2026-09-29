//! Decides how a new window is placed. Only the built-in heuristics for now; config window
//! rules join them in a later step.
use aurora_layout::Constraints;

pub struct Attrs {
    pub has_parent: bool,
    pub constraints: Constraints,
}

pub struct Decision {
    pub floating: bool,
}

pub fn evaluate(attrs: &Attrs) -> Decision {
    let Constraints { min, max } = attrs.constraints;
    // Dialogs and fixed-size windows (splash screens, prompts) do not belong in the tree.
    let fixed = min.w > 0 && min.h > 0 && min == max;
    Decision {
        floating: attrs.has_parent || fixed,
    }
}
