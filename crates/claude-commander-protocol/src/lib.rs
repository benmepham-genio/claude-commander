//! Shared wire contract for the `claude-commander-server` HTTP + WebSocket API.
//!
//! This crate is the single source of truth for the types that cross the
//! network boundary, so the server and every client (the mobile/desktop app, a
//! browser, the CLI) agree on the serde shape *by construction* rather than by
//! hand-maintained mirrors.
//!
//! It is deliberately dependency-light — `serde`, `serde_json`, `uuid` and
//! `chrono` (plus `ts-rs` under the off-by-default `ts` feature), no tmux,
//! git, or filesystem code — so it cross-compiles cleanly to mobile targets
//! (Android/iOS) where the heavyweight `claude-commander-core` crate cannot go.
//!
//! Every type here derives both `Serialize` and `Deserialize`: the server
//! serializes, clients deserialize (and vice-versa for request bodies and the
//! WebSocket control frames).
//!
//! Under the optional `ts` feature every wire type also derives `ts_rs::TS`, and
//! the browser page's TypeScript types (`web/src/generated/`) are exported from
//! them — see `ts_export.rs`. The feature is off by default, so no normal build
//! (and in particular not the Android cross-build) compiles ts-rs. A new wire
//! type gets the same `cfg_attr` derive and a line in `ts_export.rs`.

pub mod api;
pub mod comment;
pub mod config;
pub mod connection;
pub mod diff;
pub mod github;
pub mod hosting;
pub mod paste;
pub mod pr;
pub mod preview;
pub mod session;
pub mod session_editor;
pub mod workspace;
pub mod ws;

#[cfg(all(test, feature = "ts"))]
mod ts_export;

/// Maximum duration of an authenticated change request. Clients use their
/// normal request timeout, which must exceed this server-side wait.
pub const CHANGE_WAIT_SECS: u64 = 20;
