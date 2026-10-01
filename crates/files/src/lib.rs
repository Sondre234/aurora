//! `aurora-files`: the Aurora file manager.
//!
//! One process per window, an ordinary xdg-toplevel Wayland client with an optional IPC
//! connection (theme, spawn); with no socket it still runs. The window is a single canvas
//! painted from a [`scene::Scene`]; every state change is a plain function on the pure model.
//!
//! Pure core: [`model`] (entries, sorting, filter, selection, history), [`names`] (name
//! validation, collision-free names), [`uri`] (`text/uri-list` and gnome-copied-files codecs),
//! [`trash`] (FreeDesktop trash), [`ops`] (planner, guards, executor), [`places`] (sidebar),
//! [`edit`] (one-line editor), [`view`] (geometry and hit testing).
//!
//! Glue: [`listing`] (directory reads on a worker), [`watch`] (debounced inotify), [`ipc`],
//! [`system`] (spawning, theme file, icon names), [`scene`] (painting), [`input`] (keymap and
//! pointer), [`actions`] (operations, clipboard, rename), [`app`] (the loop). The `qa-hooks`
//! feature adds [`testscript`], the `AURORA_FILES_TEST_SCRIPT` input hook.

pub mod actions;
pub mod app;
pub mod edit;
pub mod input;
pub mod ipc;
pub mod listing;
pub mod model;
pub mod names;
pub mod ops;
pub mod places;
pub mod scene;
pub mod system;
#[cfg(feature = "qa-hooks")]
pub mod testscript;
pub mod trash;
pub mod uri;
pub mod view;
pub mod watch;
