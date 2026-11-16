//! Pure HTTP + WebSocket transport for `claude-commander-server`.
//!
//! [`RemoteClient`] speaks the shared wire DTOs from `claude-commander-protocol`
//! against the server's `/api` surface and its `/ws/attach` WebSocket, and
//! classifies every failure into the transport-neutral [`ClientError`]
//! categories. A background [`Poller`] drives a change-feed generation counter
//! and a [`ConnectionState`] state machine (with exponential [`backoff`]).
//!
//! This crate depends **only** on `claude-commander-protocol` plus network
//! crates — never on `claude-commander-core` — so it cross-compiles cleanly to
//! mobile targets (the Flutter cdylib) as well as backing the desktop TUI's
//! remote backend via the thin `claude-commander-remote` adapter.
//!
//! # Layering
//!
//! - `claude-commander-remote` wraps a [`RemoteClient`] + [`Poller`] and
//!   implements core's `CommanderBackend` trait, mapping [`ClientError`] →
//!   `BackendError` and this crate's [`AttachConnection`] → core's attach seam.
//! - The mobile cdylib (Phase 2) calls [`RemoteClient`] directly.
//!
//! [`backoff`]: crate::backoff_delay

mod attach;
mod backoff;
mod client;
mod error;
mod poller;
mod spec;

pub use attach::{
    AttachConnection, AttachEnd, AttachRefresher, AttachResizer, AttachStreams, AttachTerminator,
};
pub use backoff::{BackoffConfig, backoff_delay};
pub use client::RemoteClient;
pub use error::{ClientError, ClientResult};
pub use poller::{ConnectionFeed, PollConfig, Poller, spawn_poller};
pub use spec::{RemoteServerSpec, SecretString};

/// The connection-health enum the [`Poller`] drives. Re-exported from
/// `claude-commander-protocol` (which owns the definition) and, in turn, by
/// `claude-commander-core::backend`, so all three crates share one type.
pub use claude_commander_protocol::connection::ConnectionState;

#[cfg(test)]
mod test_server;
