//! Session manager - coordinates session lifecycle
//!
//! Handles the creation, restart, and termination of sessions,
//! coordinating between tmux and git operations.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, info, instrument, warn};

use crate::config::{AppState, ConfigStore, StateStore};
use crate::error::{Result, SessionError};
use crate::git::{DiffCache, DiffInfo, GitBackend, WorktreeManager};
use crate::session::{Project, ProjectId, SessionId, SessionStatus, WorktreeSession};
use crate::tmux::{CapturedContent, ContentCapture, StatusBarInfo, TmuxExecutor};

/// Result of scanning a directory for git repositories
#[derive(Debug, Clone)]
pub struct ScanResult {
    /// Number of new projects added
    pub added: usize,
    /// Number of repos skipped because they already existed
    pub skipped: usize,
}

mod cascade;
mod content;
mod lifecycle;
mod project_shell;
mod projects;
mod shell;
mod worktree_sync;

pub use cascade::{CascadeOutcome, PushStackOutcome};

#[cfg(test)]
mod tests;

/// Session manager coordinates all session operations
pub struct SessionManager {
    /// Shared configuration store (hot-reloaded)
    config_store: Arc<ConfigStore>,
    /// Concurrent-safe persistent state store
    pub store: Arc<StateStore>,
    /// Tmux executor
    pub tmux: TmuxExecutor,
    /// Content capture cache
    content_capture: ContentCapture,
    /// Diff cache for sessions
    diff_cache: DiffCache<SessionId>,
    /// Diff cache for projects
    project_diff_cache: DiffCache<ProjectId>,
    /// Tmux status-style string derived from theme
    tmux_status_style: String,
}

impl Clone for SessionManager {
    fn clone(&self) -> Self {
        Self {
            config_store: self.config_store.clone(),
            store: self.store.clone(),
            tmux: self.tmux.clone(),
            content_capture: self.content_capture.clone(),
            diff_cache: self.diff_cache.clone(),
            project_diff_cache: self.project_diff_cache.clone(),
            tmux_status_style: self.tmux_status_style.clone(),
        }
    }
}

impl SessionManager {
    /// Create a new session manager
    ///
    /// Note: `max_concurrent_tmux`, `capture_cache_ttl_ms`, and `diff_cache_ttl_ms`
    /// are read from the config at construction time and are **not** hot-reloaded.
    pub fn new(
        config_store: Arc<ConfigStore>,
        store: Arc<StateStore>,
        tmux_status_style: impl Into<String>,
    ) -> Self {
        let config = config_store.read();
        let tmux = TmuxExecutor::with_max_concurrent(config.max_concurrent_tmux);
        let content_capture = ContentCapture::with_ttl(
            tmux.clone(),
            std::time::Duration::from_millis(config.capture_cache_ttl_ms),
        );
        let diff_cache =
            DiffCache::with_ttl(std::time::Duration::from_millis(config.diff_cache_ttl_ms));
        let project_diff_cache =
            DiffCache::with_ttl(std::time::Duration::from_millis(config.diff_cache_ttl_ms));
        drop(config);

        Self {
            config_store,
            store,
            tmux,
            content_capture,
            diff_cache,
            project_diff_cache,
            tmux_status_style: tmux_status_style.into(),
        }
    }

    /// Check if tmux is available
    pub async fn check_tmux(&self) -> Result<()> {
        self.tmux.check_installed().await
    }

    /// Build a `StatusBarInfo` from session metadata
    pub fn status_bar_info(&self, session: &WorktreeSession, state: &AppState) -> StatusBarInfo {
        let project_name = state
            .get_project(&session.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        StatusBarInfo {
            branch: session.branch.clone(),
            pr_number: session.pr_number,
            pr_merged: session.pr_merged,
            status_style: self.tmux_status_style.clone(),
            is_shell: false,
            project_name,
        }
    }

    /// Generate branch name from title
    fn generate_branch_name(&self, title: &str) -> String {
        let sanitized = self.sanitize_name(title);

        let config = self.config_store.read();
        if config.branch_prefix.is_empty() {
            sanitized
        } else {
            format!("{}/{}", config.branch_prefix, sanitized)
        }
    }

    /// Sanitize a name for use as branch/directory name
    fn sanitize_name(&self, name: &str) -> String {
        sanitize_name(name)
    }
}

/// Sanitize a name for use as a branch/directory name.
///
/// Lowercases, replaces non-alphanumeric characters (except `-` and `_`) with
/// `-`, and trims leading/trailing `-`.
pub fn sanitize_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Decide whether the `[branch]` annotation should be shown next to a session
/// title.
///
/// Returns `Some(branch)` when the branch carries information beyond what
/// the title already conveys. Returns `None` when:
/// - the title is literally identical to the branch (the checkout-branch
///   flow sets these equal — no point rendering the same string twice), or
/// - the branch matches `sanitize_name(title)` either exactly or as the
///   last `/`-segment (so a configured `branch_prefix` like `"user/"` is
///   treated as noise).
pub fn display_branch<'a>(title: &str, branch: &'a str) -> Option<&'a str> {
    if title == branch {
        return None;
    }
    let sanitized = sanitize_name(title);
    if sanitized.is_empty() {
        return Some(branch);
    }
    let matches = branch == sanitized
        || branch
            .rsplit_once('/')
            .map(|(_, tail)| tail == sanitized)
            .unwrap_or(false);
    if matches { None } else { Some(branch) }
}
