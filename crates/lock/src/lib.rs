//! `aurora-lock`: the screen locker, an `ext-session-lock` client.
//!
//! Flow: take the single-instance guard, request the lock, wait for the compositor's
//! `locked`, put one lock surface on every output (following hotplug), show a clock and a
//! password prompt, check passwords through an [`auth::Authenticator`] on a worker thread,
//! and unlock only after a successful check. Every other way out leaves the session
//! locked, which is the compositor's contract for a dead lock client.
//!
//! Log contract (never the password): `lock-client: locking | locked | unlocked`.

pub mod app;
pub mod auth;
pub mod clock;
pub mod instance;
pub mod prompt;
pub mod secret;
pub mod view;

#[cfg(test)]
mod auth_tests;
