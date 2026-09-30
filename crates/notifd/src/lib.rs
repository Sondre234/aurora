//! `aurora-notifd`: the Aurora notification daemon.
//!
//! Pure, unit-tested core: [`markup`] (body stripping), [`hints`] (hint parsing and
//! image conversion), [`state`] (ids, replace, expiry, stacking limits, history, close
//! reasons), [`layout`] (stack positions and slide animation), [`toast`] (styling and
//! widget tree), [`icon`] (PNG lookup and decode), [`ipc_client`] (theme and focus link).
//! Effectful glue: [`dbus`] (zbus server) and [`app`] (ui runtime).

pub mod app;
pub mod cli;
pub mod dbus;
pub mod hints;
pub mod icon;
pub mod ipc_client;
pub mod layout;
pub mod markup;
pub mod state;
pub mod toast;
