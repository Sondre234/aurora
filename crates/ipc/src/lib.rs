//! Typed IPC between the Aurora compositor (the hub) and its services.
//!
//! Pure `std` + `serde` + `postcard`: no Smithay, no Wayland, no async runtime, so every
//! service and `auroractl` can link it cheaply.
//!
//! # Transport
//!
//! A unix stream socket at [`socket_path`]. Each message is one [`Frame`]: a `u32`
//! little-endian length prefix, then the postcard encoding of the frame, capped at
//! [`MAX_FRAME`] (1 MiB). Use [`Decoder`] on nonblocking sockets (feed bytes, pop frames)
//! and [`encode`] for output; [`read_frame`] / [`write_frame`] are the blocking helpers.
//!
//! # Conversation
//!
//! 1. The client sends `Body::Hello`; the server answers with its own `Body::Hello`.
//!    Either side checks the other with [`check_hello`] and closes on a mismatch
//!    (the server may first send `Body::Error` with [`ErrorCode::VersionMismatch`]).
//! 2. The client sends `Body::Subscribe(topics)`. The server answers with
//!    `Event::Snapshot` (the full state, regardless of topics) and then deltas for the
//!    subscribed topics. `Request::GetSnapshot` fetches a snapshot at any time.
//! 3. A client sends `Body::Request` with a nonzero `id`; the server replies with
//!    `Body::Response` or `Body::Error` carrying the same `id`. Events use `id == 0`.
//!
//! # Evolution rule
//!
//! Postcard is not self-describing, so the wire format is positional. Therefore:
//! enums (`Body`, `Request`, `Response`, `Event`, `Topic`, `ErrorCode`, ...) only ever
//! grow at the END, variants are never reordered, removed or repurposed, and struct
//! fields are never reordered or removed (adding a field to a struct is a breaking
//! change for that struct and needs a [`PROTO_VERSION`] bump). A receiver that meets an
//! unknown variant gets a decode error for that frame only; the stream stays in sync
//! because the length prefix was honored. Anything breaking bumps [`PROTO_VERSION`],
//! and versions must match exactly.

mod codec;
mod path;
mod types;

#[cfg(test)]
mod tests;

pub use codec::{Decoder, FrameError, encode, encode_into, read_frame, write_frame};
pub use path::{socket_path, socket_path_from};
pub use types::*;

pub use aurora_theme::{Theme, ThemeSnapshot};

/// Bumped on every wire-incompatible change. Clients and server must match exactly.
pub const PROTO_VERSION: u32 = 1;

/// Largest accepted frame body in bytes (the length prefix excluded).
pub const MAX_FRAME: usize = 1024 * 1024;
