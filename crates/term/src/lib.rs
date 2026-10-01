//! `aurora-term`: the Aurora terminal emulator.
//!
//! Module map: [`cli`] (arguments), [`keys`] (key and paste encoding), [`mouse`] (mouse
//! reports), [`colors`] (theme-derived color tables), more modules are added per step.

pub mod cli;
pub mod colors;
pub mod keys;
pub mod mouse;
