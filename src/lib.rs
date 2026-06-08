//! Claude Commander - A high-performance terminal UI for managing Claude coding sessions
//!
//! This crate provides an async-first, actor-based architecture for managing
//! multiple AI coding sessions through tmux and git worktrees.
//!
//! # Architecture
//!
//! The application is built around several key actors:
//! - **TUI Actor** - Handles terminal rendering and user input
//! - **SessionManager Actor** - Coordinates session lifecycle
//! - **TmuxActor** - Per-session tmux integration
//! - **GitActor** - Per-session git operations
//!
//! # Modules
//!
//! - [`session`] - Hierarchical session model (Projects and WorktreeSessions)
//! - [`tmux`] - Async tmux integration with caching
//! - [`git`] - Pure Rust git operations via gitoxide
//! - [`tui`] - Event-driven terminal UI with ratatui
//! - [`config`] - Configuration and state persistence
//! - [`error`] - Error types

pub mod api;
pub mod cli;
pub mod config;
pub mod error;
pub mod fuzzy;
pub mod git;
pub mod picker;
pub mod session;
pub mod tmux;
pub mod tui;

pub use config::keybindings::editor_trigger_bytes;
pub use config::{AppState, Config, StateStore};
pub use error::{Error, Result};
pub use session::{
    AgentState, Project, ProjectId, SessionId, SessionListItem, SessionStatus, WorktreeSession,
};
pub use tmux::{AttachResult, attach_to_session};

/// Application version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Application name
pub const APP_NAME: &str = env!("CARGO_PKG_NAME");
