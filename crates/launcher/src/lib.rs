//! `aurora-launcher`: the warm app launcher daemon.
//!
//! One long-lived process (performance.md rule 5): the UI surface is built once on an
//! Overlay layer with exclusive keyboard focus (not Top, which a fullscreen window hides),
//! starts unmapped, and is shown and hidden by `aurora-launcher toggle`. The app index
//! lives in memory, is rebuilt on directory changes, and ranking adds frecency (how often
//! and how recently) to a fuzzy match score. Apps are launched through the compositor's IPC
//! `Spawn` request, or directly when IPC is unavailable.
//!
//! Module map: [`fuzzy`] (scoring and highlight positions), [`entry`] (desktop entries,
//! `Exec` field codes), [`index`] (scan, diff, search), [`frecency`], [`system`] (session glue; icons live in
//! `aurora-icons`), [`watch`] (debounced directory watcher),
//! [`ipc`] (compositor connection), [`control`] (the `toggle` socket), [`view`] (widget
//! tree), [`app`] (the daemon), [`hook`] (documented LLM extension trait, unimplemented).

pub mod app;
pub mod control;
pub mod entry;
pub mod frecency;
pub mod fuzzy;
pub mod hook;
pub mod index;
pub mod ipc;
pub mod system;
pub mod view;
pub mod watch;
