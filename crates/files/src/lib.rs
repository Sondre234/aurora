//! `aurora-files`: the Aurora file manager.
//!
//! One process per window, an ordinary xdg-toplevel Wayland client with an optional IPC
//! connection (theme, spawn). Module map: [`model`] (listing, sort, selection, history),
//! [`ops`] (executor), [`names`] (name validation and collision-free names), [`uri`] (clipboard codecs),
//! [`trash`] (FreeDesktop trash).

pub mod edit;
pub mod listing;
pub mod model;
pub mod names;
pub mod ops;
pub mod places;
pub mod trash;
pub mod uri;
pub mod watch;
