//! Claude Commander - managing Claude coding sessions over tmux and git worktrees
//!
//! This crate is the frontend-agnostic library: an async-first architecture for
//! managing multiple AI coding sessions through tmux and git worktrees, with no
//! terminal UI of its own. The ratatui frontend lives in `claude-commander-tui`,
//! and the HTTP/WebSocket frontend in `claude-commander-server`; both drive this
//! crate through [`api::CommanderService`] and the
//! [`backend::CommanderBackend`] trait. Keep it that way — see the layering
//! notes in CLAUDE.md.
//!
//! # Architecture
//!
//! - **[`api::CommanderService`]** - the single coordination layer every
//!   frontend calls, owning the session manager and the state/config stores
//! - **[`session::SessionManager`]** - session lifecycle (create/restart/delete)
//! - **[`tmux`]** - per-session tmux integration
//! - **[`git`]** - per-session git operations
//!
//! # Modules
//!
//! - [`session`] - Hierarchical session model (Projects and WorktreeSessions)
//! - [`tmux`] - Async tmux integration with caching
//! - [`git`] - Pure Rust git operations via gitoxide
//! - [`backend`] - The `CommanderBackend` trait a frontend talks to, local or remote
//! - [`config`] - Configuration and state persistence
//! - [`term_caps`] - Terminal colour capability detection
//! - [`error`] - Error types

pub mod agent;
pub mod api;
pub mod backend;
pub mod cli;
pub mod clipboard;
pub mod commander;
pub mod comment;
pub mod config;
pub mod conversation;
pub mod error;
pub mod git;
pub mod paste_image;
pub mod reviewed;
pub mod session;
pub mod telemetry;
pub mod term_caps;
pub mod tmux;

pub use config::keybindings::editor_trigger_bytes;
pub use config::{AppState, Config, StateStore};
pub use error::{Error, Result};
pub use session::{
    AgentState, Project, ProjectId, SessionId, SessionListItem, SessionStatus, WorktreeSession,
};
pub use tmux::{AttachResult, attach_to_session};

/// This library's version, i.e. the `claude-commander-core` crate's. Reported
/// as telemetry's `lib_version` and compared against a remote server's build.
/// It is *not* the frontend's version — a frontend (the TUI binary, the mobile
/// client) reports its own; see [`telemetry::FrontendInfo`].
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod file_events;
mod singleflight;
