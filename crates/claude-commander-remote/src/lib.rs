//! Adapter exposing `claude-commander-client`'s transport as a core backend.
//!
//! [`RemoteBackend`] implements core's
//! [`CommanderBackend`](claude_commander_core::backend::CommanderBackend) trait
//! over the pure HTTP/WebSocket [`RemoteClient`](claude_commander_client::RemoteClient),
//! so the same TUI that drives an in-process `LocalBackend` can drive a machine
//! across the network. The transport itself — the wire DTOs, per-route calls,
//! change-feed poller, connection state machine, and attach pump — lives in
//! `claude-commander-client` (which depends only on `claude-commander-protocol`,
//! not on core, so it also backs the mobile cdylib). This crate is a thin
//! shim: it delegates each trait method to the matching client method, maps the
//! client's `ClientError` onto core's
//! [`BackendError`](claude_commander_core::backend::BackendError) 1:1 (see
//! [`error`]), and rebuilds the few core-only return types (`ScanResult`) that
//! have no wire DTO. The attach seam is bridged in [`attach`].
//!
//! # Trait method → route
//!
//! | Trait method | HTTP |
//! |---|---|
//! | `snapshot` | `GET /api/workspace` |
//! | `agent_states(fresh)` | `GET /api/agent-states?fresh=` |
//! | `session_detail(q, lines)` | `GET /api/sessions/{q}/detail?lines=` (404 → `None`) |
//! | `preview(Session)` / `preview(Project)` | `GET /api/sessions/{id}/preview?lines=` / `GET /api/projects/{id}/preview` |
//! | `branch_diff` | `GET /api/sessions/{id}/branch-diff` (text) |
//! | `list_branches` | `GET /api/projects/{id}/branches?fetch=` |
//! | `create_options` | `GET /api/create-options` |
//! | `pending_comment_sessions` | `GET /api/comments/pending` |
//! | `create_session` | `POST /api/sessions` → `{id}` |
//! | `kill_session` / `restart_session` | `POST /api/sessions/{id}/kill` / `…/restart` |
//! | `restart_session_fresh` | `POST /api/sessions/{id}/restart-fresh` (no resume) |
//! | `delete_session` | `DELETE /api/sessions/{id}` |
//! | `rename_session` / `set_section` | `PATCH /api/sessions/{id}` (tagged `op`) |
//! | `set_session_base` | `POST /api/sessions/{id}/base` → outcome |
//! | `mark_read` | `POST /api/sessions/{id}/read` |
//! | `mark_unread` | `POST /api/sessions/unread` (batch) |
//! | `add_project` | `POST /api/projects` → `{id}` |
//! | `remove_project` | `DELETE /api/projects/{id}` |
//! | `scan_directory` | `POST /api/projects/scan` → `{path, workspace?}` |
//! | `cascade_merge` / `push_stack` | `POST /api/sessions/{id}/cascade` / `…/push-stack` |
//! | `cascade_resume` / `cascade_abandon` | `POST /api/cascade/resume` / `…/abandon` |
//! | `list_comments` / `open_review` | `GET /api/sessions/{id}/comments` / `…/review` |
//! | `refresh_review_if_changed` | `GET /api/sessions/{id}/review/refresh?prev_hash=` (204 → `None`) |
//! | `create_comment` / `delete_comment` | `POST` / `DELETE /api/sessions/{id}/comments[/{cid}]` |
//! | `apply_comments` | `POST /api/sessions/{id}/comments/apply` |
//! | `set_file_reviewed` | `POST /api/sessions/{id}/files/reviewed` |
//! | `reviewed_marks` | `GET /api/sessions/{id}/files/reviewed` |
//! | `fetch_diff_blob` | `GET /api/sessions/{id}/blob?side=&path=` |
//! | `attach` | `GET /ws/attach` (WebSocket; see [`attach`]) |
//!
//! # Change-feed + connection health
//!
//! The client crate's background poller polls the workspace + agent-state
//! snapshots on a fixed cadence, content-hashes them, and bumps a generation
//! counter (exposed via
//! [`CommanderBackend::change_feed`](claude_commander_core::backend::CommanderBackend::change_feed))
//! when they move. It also drives a
//! [`ConnectionState`](claude_commander_core::backend::ConnectionState) watch
//! (`Connecting` → `Connected` → `Degraded { reason }` with exponential
//! backoff), read by the TUI via [`RemoteBackend::connection_state`] /
//! [`RemoteBackend::connection_feed`] after an
//! [`as_any`](claude_commander_core::backend::CommanderBackend::as_any) downcast.

mod attach;
mod backend;
mod error;

pub use backend::RemoteBackend;

// Re-export the client crate's public knobs so this crate's downstream API
// (the TUI and `claude-commander` bin) is unchanged by the transport extraction.
pub use claude_commander_client::{
    BackoffConfig, ConnectionFeed, PollConfig, RemoteServerSpec, SecretString, backoff_delay,
};
