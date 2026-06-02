//! Core session types
//!
//! Defines the hierarchical session model:
//! - `Project` represents a git repository
//! - `WorktreeSession` represents a worktree session within a project

use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a project (git repository)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProjectId(Uuid);

impl ProjectId {
    /// Create a new random project ID
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Create from an existing UUID
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl Default for ProjectId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Use first 8 chars for display
        write!(f, "{}", &self.0.to_string()[..8])
    }
}

/// Unique identifier for a worktree session
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Create a new random session ID
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Create from an existing UUID
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Get the inner UUID
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Use first 8 chars for display
        write!(f, "{}", &self.0.to_string()[..8])
    }
}

/// Status of a worktree session
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Session is being created (worktree/tmux setup in progress)
    Creating,
    /// Session is running and active
    Running,
    /// Session has completed or been killed
    #[serde(alias = "paused")]
    Stopped,
    /// Cascade-merge is running `git merge` in this session's worktree.
    /// Transient: cleared as soon as the merge step completes, conflicts or
    /// not. `cleanup_stale_merging_sessions` resets any stragglers at startup
    /// in case the process died mid-merge.
    Merging,
    /// Cascade-merge hit a conflict in this session. Persists until the user
    /// runs `CascadeResume` (after resolving + committing) or `CascadeAbandon`,
    /// so the stalled session is still visible after restarting the TUI.
    CascadePaused,
    /// `git push` is running against this session's branch as part of a
    /// push-stack operation. Transient: cleared as soon as the push completes
    /// (success or failure). Same stale-cleanup treatment as `Merging` — a
    /// crash mid-push shouldn't leave the UI stuck.
    Pushing,
}

impl SessionStatus {
    /// Check if the session is active (creating, running, or mid-cascade).
    ///
    /// `Merging` and `CascadePaused` both mean the tmux session is still
    /// alive — the underlying program is running even if the worktree is
    /// temporarily locked by the cascade.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Creating | Self::Running | Self::Merging | Self::CascadePaused | Self::Pushing
        )
    }

    /// Check if the session can be attached to (Stopped sessions are allowed
    /// because get_attach_command will recreate the tmux session automatically).
    /// Cascade-involved sessions are attachable — users often attach to a
    /// `CascadePaused` session specifically to resolve conflicts there.
    pub fn can_attach(&self) -> bool {
        matches!(
            self,
            Self::Running | Self::Stopped | Self::Merging | Self::CascadePaused | Self::Pushing
        )
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Creating => write!(f, "creating"),
            Self::Running => write!(f, "running"),
            Self::Stopped => write!(f, "stopped"),
            Self::Merging => write!(f, "merging"),
            Self::CascadePaused => write!(f, "cascade_paused"),
            Self::Pushing => write!(f, "pushing"),
        }
    }
}

/// Sub-state of a Running Claude Code session, detected via pane content parsing.
/// This is ephemeral (not persisted) and only meaningful when SessionStatus == Running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Claude is actively generating output
    Working,
    /// Claude has finished and is at the input prompt
    Idle,
    /// Claude is waiting for user permission or input
    WaitingForInput,
    /// State could not be determined (non-Claude program, detection failure, etc.)
    Unknown,
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Working => write!(f, "working"),
            Self::Idle => write!(f, "idle"),
            Self::WaitingForInput => write!(f, "waiting"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Project represents a git repository (parent session)
///
/// A project is the top-level container that holds:
/// - Reference to the main git repository
/// - Collection of worktree sessions
/// - Project-level metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    /// Unique identifier
    pub id: ProjectId,
    /// Display name (typically repo directory name)
    pub name: String,
    /// Path to the main repository
    pub repo_path: PathBuf,
    /// Main branch name (e.g., "main", "master")
    pub main_branch: String,
    /// When the project was added
    pub created_at: DateTime<Utc>,
    /// Worktree sessions belonging to this project
    #[serde(default)]
    pub worktrees: Vec<SessionId>,
    /// Shell tmux session name (for project-level shell)
    #[serde(default)]
    pub shell_tmux_session_name: Option<String>,
}

impl Project {
    /// Create a new project
    pub fn new(
        name: impl Into<String>,
        repo_path: PathBuf,
        main_branch: impl Into<String>,
    ) -> Self {
        Self {
            id: ProjectId::new(),
            name: name.into(),
            repo_path,
            main_branch: main_branch.into(),
            created_at: Utc::now(),
            worktrees: Vec::new(),
            shell_tmux_session_name: None,
        }
    }

    /// Add a worktree session to this project
    pub fn add_worktree(&mut self, session_id: SessionId) {
        if !self.worktrees.contains(&session_id) {
            self.worktrees.push(session_id);
        }
    }

    /// Remove a worktree session from this project
    pub fn remove_worktree(&mut self, session_id: &SessionId) {
        self.worktrees.retain(|id| id != session_id);
    }
}

/// WorktreeSession represents a git worktree with an associated tmux session
///
/// Each worktree session:
/// - Belongs to a parent project
/// - Has its own git branch
/// - Has an isolated working directory
/// - Has a dedicated tmux session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeSession {
    /// Unique identifier
    pub id: SessionId,
    /// Parent project ID
    pub project_id: ProjectId,
    /// User-friendly title
    pub title: String,
    /// Git branch name
    pub branch: String,
    /// Path to the worktree directory
    pub worktree_path: PathBuf,
    /// Current status
    pub status: SessionStatus,
    /// Program running in the session (e.g., "claude", "aider")
    pub program: String,
    /// When the session was created
    pub created_at: DateTime<Utc>,
    /// When the session was last active
    pub last_active_at: DateTime<Utc>,
    /// Tmux session name (for tmux commands)
    pub tmux_session_name: String,
    /// Base commit for diff computation (branch point)
    #[serde(default)]
    pub base_commit: Option<String>,
    /// Shell tmux session name (for secondary shell sessions)
    #[serde(default)]
    pub shell_tmux_session_name: Option<String>,
    /// GitHub PR number (if a PR exists for this branch)
    #[serde(default)]
    pub pr_number: Option<u32>,
    /// GitHub PR URL
    #[serde(default)]
    pub pr_url: Option<String>,
    /// Whether the PR has been merged (kept for backward compat — derived from pr_state)
    #[serde(default)]
    pub pr_merged: bool,
    /// PR lifecycle state (open / closed / merged). None = unknown / no PR.
    #[serde(default)]
    pub pr_state: Option<crate::git::PrState>,
    /// Whether the PR is a draft
    #[serde(default)]
    pub pr_draft: bool,
    /// Label names attached to the PR (used for review-needed colouring)
    #[serde(default)]
    pub pr_labels: Vec<String>,
    /// GitHub `reviewDecision` for the PR (None when no PR or no decision data).
    #[serde(default)]
    pub review_decision: Option<crate::git::ReviewDecision>,
    /// Reviewer logins on the PR — the union of requested reviewers and
    /// submitted review authors. Empty when there's no PR or no reviewers.
    #[serde(default)]
    pub pr_reviewers: Vec<String>,
    /// Branch the PR targets, as reported by GitHub (e.g. `main` or another
    /// session's branch). Populated from `gh pr` JSON's `baseRefName`; used
    /// as the source of truth for PR-stack detection.
    #[serde(default)]
    pub pr_base_branch: Option<String>,
    /// Fallback parent link for PR-stack grouping, set when the session is
    /// created via the "add stacked session" hotkey and the PR doesn't yet
    /// exist. Once `pr_base_branch` resolves to an in-project session, that
    /// wins over this field.
    #[serde(default)]
    pub stack_parent_session_id: Option<SessionId>,
    /// Whether the session has unread output (agent finished but user hasn't attached)
    #[serde(default)]
    pub unread: bool,
    /// Manual section override. When set and matching a configured section,
    /// the session is pinned there regardless of predicate rules.
    #[serde(default)]
    pub section_override: Option<String>,
    /// Cached current section name (None = Other / catch-all). Updated by
    /// `apply_assignment`; used to detect transitions for `entered_section_at`.
    #[serde(default)]
    pub current_section: Option<String>,
    /// Timestamp the session entered its current section. Used for
    /// oldest-in-section-first sort order.
    #[serde(default = "chrono::Utc::now")]
    pub entered_section_at: DateTime<Utc>,
    /// Most recent time the user attached to this session. Drives the
    /// Alt+Tab-style MRU ordering in the in-tmux session picker.
    /// `None` for sessions never attached since adopting the field.
    #[serde(default)]
    pub last_attached_at: Option<DateTime<Utc>>,
}

impl WorktreeSession {
    /// Create a new worktree session
    pub fn new(
        project_id: ProjectId,
        title: impl Into<String>,
        branch: impl Into<String>,
        worktree_path: PathBuf,
        program: impl Into<String>,
    ) -> Self {
        let id = SessionId::new();
        let title = title.into();
        let now = Utc::now();

        // Generate tmux session name from ID (short, unique)
        let tmux_session_name = format!("cc-{}", &id.0.to_string()[..8]);

        Self {
            id,
            project_id,
            title,
            branch: branch.into(),
            worktree_path,
            status: SessionStatus::Running,
            program: program.into(),
            created_at: now,
            last_active_at: now,
            tmux_session_name,
            base_commit: None,
            shell_tmux_session_name: None,
            pr_number: None,
            pr_url: None,
            pr_merged: false,
            pr_state: None,
            pr_draft: false,
            pr_labels: Vec::new(),
            review_decision: None,
            pr_reviewers: Vec::new(),
            pr_base_branch: None,
            stack_parent_session_id: None,
            unread: false,
            section_override: None,
            current_section: None,
            entered_section_at: now,
            last_attached_at: None,
        }
    }

    /// Create a placeholder session in the `Creating` state.
    ///
    /// The worktree path is left empty because it doesn't exist yet;
    /// it will be filled in by `SessionManager::finalize_session`.
    pub fn new_creating(
        project_id: ProjectId,
        title: impl Into<String>,
        branch: impl Into<String>,
        program: impl Into<String>,
    ) -> Self {
        let id = SessionId::new();
        let title = title.into();
        let now = Utc::now();
        let tmux_session_name = format!("cc-{}", &id.0.to_string()[..8]);

        Self {
            id,
            project_id,
            title,
            branch: branch.into(),
            worktree_path: PathBuf::new(),
            status: SessionStatus::Creating,
            program: program.into(),
            created_at: now,
            last_active_at: now,
            tmux_session_name,
            base_commit: None,
            shell_tmux_session_name: None,
            pr_number: None,
            pr_url: None,
            pr_merged: false,
            pr_state: None,
            pr_draft: false,
            pr_labels: Vec::new(),
            review_decision: None,
            pr_reviewers: Vec::new(),
            pr_base_branch: None,
            stack_parent_session_id: None,
            unread: false,
            section_override: None,
            current_section: None,
            entered_section_at: now,
            last_attached_at: None,
        }
    }

    /// Update the session status
    pub fn set_status(&mut self, status: SessionStatus) {
        self.status = status;
        if status == SessionStatus::Running {
            self.last_active_at = Utc::now();
        }
    }

    /// Mark the session as active (update last_active_at)
    pub fn touch(&mut self) {
        self.last_active_at = Utc::now();
    }

    /// Record an attach event. Used by the in-tmux switcher to order
    /// sessions Alt+Tab-style by most-recently viewed.
    pub fn mark_attached(&mut self) {
        self.last_attached_at = Some(Utc::now());
    }

    /// Whether `name` refers to this session — either the primary tmux
    /// session or its paired shell session (toggled via Ctrl+\).
    pub fn matches_tmux_name(&self, name: &str) -> bool {
        self.tmux_session_name == name || self.shell_tmux_session_name.as_deref() == Some(name)
    }

    /// Check if this session matches a search query (fuzzy subsequence).
    pub fn matches_query(&self, query: &str) -> bool {
        self.fuzzy_score(query).is_some()
    }

    /// Best fuzzy score across title, branch, and program — or `None` if
    /// no field matches. Used by the palette to rank results.
    pub fn fuzzy_score(&self, query: &str) -> Option<i64> {
        [
            self.title.as_str(),
            self.branch.as_str(),
            self.program.as_str(),
        ]
        .iter()
        .filter_map(|s| crate::fuzzy::fuzzy_score(s, query))
        .max()
    }

    /// True when the session's PR is merged on GitHub. Honours the legacy
    /// `pr_merged` flag for state.json files written before `pr_state` existed.
    pub fn pr_is_merged(&self) -> bool {
        matches!(
            crate::git::effective_pr_state(self.pr_state, self.pr_merged),
            crate::git::PrState::Merged,
        )
    }
}

/// Resolve the stack parent of a session within its project.
///
/// `project_sessions` is expected to contain every session belonging to the
/// same project (including `session` itself — it's filtered out internally).
///
/// Resolution rules:
///
/// 1. If `session.pr_base_branch` is set, GitHub is the authoritative source.
///    Return the project session whose `branch` matches. If no session matches
///    (PR targets `main`, a deleted branch, etc.), the session is **not**
///    stacked — return `None`, even if `stack_parent_session_id` is set.
/// 2. If `session.pr_base_branch` is unset (no PR yet), fall back to the local
///    `stack_parent_session_id` hint set at creation time. Only honour it if
///    the referenced session still exists in the project.
/// 3. Otherwise, the session is not stacked.
pub fn resolve_stack_parent(
    session: &WorktreeSession,
    project_sessions: &[&WorktreeSession],
) -> Option<SessionId> {
    if let Some(base) = session.pr_base_branch.as_deref() {
        return project_sessions
            .iter()
            .find(|s| s.id != session.id && s.branch == base)
            .map(|s| s.id);
    }
    let parent_id = session.stack_parent_session_id?;
    project_sessions
        .iter()
        .any(|s| s.id == parent_id)
        .then_some(parent_id)
}

/// Walk up the stack chain starting from `session_id` to find the session at
/// the top of its stack.
///
/// Returns the leaf session: the member of this session's stack that has no
/// stacked children. If the selected session is unstacked (no descendants),
/// returns the session itself.
///
/// When a session has multiple direct stacked children (branching), the
/// walker prefers the most recently created one, so the "top" is
/// deterministic and matches what the user most likely intends.
pub fn stack_top(session_id: SessionId, project_sessions: &[&WorktreeSession]) -> SessionId {
    let mut current = session_id;
    // Bounded by number of sessions to avoid ever spinning on a corrupted cycle.
    for _ in 0..project_sessions.len() {
        let next_child = project_sessions
            .iter()
            .filter(|s| resolve_stack_parent(s, project_sessions) == Some(current))
            .max_by_key(|s| s.created_at);
        match next_child {
            Some(child) => current = child.id,
            None => return current,
        }
    }
    current
}

/// Walk up the stack chain starting from `session_id` to find the session at
/// the bottom of its stack.
///
/// Returns the root session: the ancestor whose own `resolve_stack_parent`
/// returns `None`. If the selected session is unstacked, returns it unchanged.
/// Dual of [`stack_top`].
///
/// On fan-out (one parent with multiple children), every descendant resolves
/// to the same root, so callers can use this to group an entire stack
/// subgraph by a single identifier.
pub fn stack_root(session_id: SessionId, project_sessions: &[&WorktreeSession]) -> SessionId {
    let mut current = session_id;
    // Bounded by session count to be safe against any malformed cycle.
    for _ in 0..project_sessions.len() {
        let this = project_sessions.iter().find(|s| s.id == current);
        match this.and_then(|s| resolve_stack_parent(s, project_sessions)) {
            Some(parent) => current = parent,
            None => return current,
        }
    }
    current
}

/// Linearise a stack from its base, returning `[base, child, grandchild, …]`.
///
/// Walks downward the same way as `stack_top` — on each hop, the session whose
/// resolved parent is the current one, picking the most recently created when
/// a base has multiple direct children. Used to drive a cascade-merge that
/// propagates a merge commit up through the chain.
///
/// The starting `base_id` is always the first element of the returned vector,
/// even when it has no children.
pub fn stack_chain_from_base(
    base_id: SessionId,
    project_sessions: &[&WorktreeSession],
) -> Vec<SessionId> {
    let mut chain = vec![base_id];
    let mut current = base_id;
    for _ in 0..project_sessions.len() {
        let next_child = project_sessions
            .iter()
            .filter(|s| resolve_stack_parent(s, project_sessions) == Some(current))
            .max_by_key(|s| s.created_at);
        match next_child {
            Some(child) => {
                current = child.id;
                chain.push(current);
            }
            None => break,
        }
    }
    chain
}

/// Represents an item in the hierarchical session list
/// Used for UI display and navigation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionListItem {
    /// A project header
    Project {
        id: ProjectId,
        name: String,
        repo_path: PathBuf,
        main_branch: String,
        worktree_count: usize,
        /// When `true`, render indented one level deeper — used for project
        /// sub-headers nested under a section header.
        nested: bool,
    },
    /// A worktree session (indented under project)
    Worktree {
        id: SessionId,
        project_id: ProjectId,
        title: String,
        branch: String,
        status: SessionStatus,
        program: String,
        pr_number: Option<u32>,
        pr_url: Option<String>,
        pr_merged: bool,
        pr_state: Option<crate::git::PrState>,
        pr_draft: bool,
        pr_labels: Vec<String>,
        worktree_path: PathBuf,
        created_at: chrono::DateTime<chrono::Utc>,
        agent_state: Option<AgentState>,
        unread: bool,
        /// True when this row is a stacked child of the row directly above it,
        /// meaning it sits one indent deeper than a normal session row. Stack
        /// bases and unstacked sessions keep the normal indent and have this
        /// set to `false`.
        stacked_child: bool,
    },
    /// A section header (used only when config.sections is non-empty).
    SectionHeader {
        name: String,
        count: usize,
        collapsed: bool,
    },
    /// A blank spacer row for visual separation between sections.
    /// Not selectable.
    Spacer,
}

impl SessionListItem {
    /// Get a unique key for this item (for selection tracking)
    pub fn key(&self) -> String {
        match self {
            Self::Project { id, .. } => format!("project:{}", id),
            Self::Worktree { id, .. } => format!("worktree:{}", id),
            Self::SectionHeader { name, .. } => format!("section:{}", name),
            Self::Spacer => "spacer".to_string(),
        }
    }

    /// Check if this is a project item
    pub fn is_project(&self) -> bool {
        matches!(self, Self::Project { .. })
    }

    /// Check if this is a worktree item
    pub fn is_worktree(&self) -> bool {
        matches!(self, Self::Worktree { .. })
    }

    /// Whether navigation/selection should land on this row.
    pub fn is_selectable(&self) -> bool {
        !matches!(self, Self::Spacer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id_display() {
        let id = SessionId::new();
        let display = id.to_string();
        assert_eq!(display.len(), 8);
    }

    #[test]
    fn test_project_id_display() {
        let id = ProjectId::new();
        let display = id.to_string();
        assert_eq!(display.len(), 8);
    }

    #[test]
    fn test_session_status_can_attach() {
        assert!(SessionStatus::Running.can_attach());
        assert!(SessionStatus::Stopped.can_attach());
    }

    #[test]
    fn test_project_worktree_management() {
        let mut project = Project::new("test", PathBuf::from("/tmp/test"), "main");
        let session_id = SessionId::new();

        project.add_worktree(session_id);
        assert_eq!(project.worktrees.len(), 1);

        // Adding same ID again should not duplicate
        project.add_worktree(session_id);
        assert_eq!(project.worktrees.len(), 1);

        project.remove_worktree(&session_id);
        assert!(project.worktrees.is_empty());
    }

    #[test]
    fn test_worktree_session_creation() {
        let project_id = ProjectId::new();
        let session = WorktreeSession::new(
            project_id,
            "Feature Auth",
            "feature-auth",
            PathBuf::from("/tmp/worktree"),
            "claude",
        );

        assert_eq!(session.project_id, project_id);
        assert_eq!(session.title, "Feature Auth");
        assert_eq!(session.branch, "feature-auth");
        assert_eq!(session.program, "claude");
        assert!(session.tmux_session_name.starts_with("cc-"));
        assert_eq!(session.status, SessionStatus::Running);
    }

    #[test]
    fn test_pr_is_merged() {
        let mut session = WorktreeSession::new(
            ProjectId::new(),
            "x",
            "b",
            PathBuf::from("/tmp/x"),
            "claude",
        );

        // Default (no PR info): not merged.
        assert!(!session.pr_is_merged());

        // Explicit pr_state == Merged → merged.
        session.pr_state = Some(crate::git::PrState::Merged);
        session.pr_merged = false;
        assert!(session.pr_is_merged());

        // Backward compat: pre-pr_state state.json with pr_merged=true.
        session.pr_state = None;
        session.pr_merged = true;
        assert!(session.pr_is_merged());

        // Explicit state wins over the legacy bool when they disagree.
        session.pr_state = Some(crate::git::PrState::Open);
        session.pr_merged = true;
        assert!(!session.pr_is_merged());

        // Explicit Open with bool false → not merged.
        session.pr_state = Some(crate::git::PrState::Open);
        session.pr_merged = false;
        assert!(!session.pr_is_merged());
    }

    #[test]
    fn test_session_matches_query() {
        let session = WorktreeSession::new(
            ProjectId::new(),
            "Feature Authentication",
            "feature-auth",
            PathBuf::from("/tmp"),
            "claude",
        );

        assert!(session.matches_query("auth"));
        assert!(session.matches_query("AUTH")); // case insensitive
        assert!(session.matches_query("feature"));
        assert!(session.matches_query("claude"));
        assert!(!session.matches_query("unrelated"));
    }

    #[test]
    fn test_session_matches_query_fuzzy_subsequence() {
        // The palette matcher is subsequence-based — "andr2" should match
        // "android-record-2" even though those chars aren't contiguous.
        let session = WorktreeSession::new(
            ProjectId::new(),
            "android-record-2",
            "android-record-2",
            PathBuf::from("/tmp"),
            "claude",
        );
        assert!(session.matches_query("andr2"));
        assert!(session.matches_query("rec2"));
        // Out-of-order chars must still fail.
        assert!(!session.matches_query("2andr"));
    }

    #[test]
    fn test_fuzzy_score_ranks_title_over_branch() {
        // The title matches more tightly than the branch, so the best
        // (max) score should come from the title.
        let session = WorktreeSession::new(
            ProjectId::new(),
            "payments",
            "wip-long-branch-name-payments-fix",
            PathBuf::from("/tmp"),
            "claude",
        );
        let title_only = crate::fuzzy::fuzzy_score("payments", "payments").unwrap();
        let combined = session.fuzzy_score("payments").unwrap();
        assert_eq!(combined, title_only);
    }

    #[test]
    fn test_session_list_item_key() {
        let project_id = ProjectId::new();
        let session_id = SessionId::new();

        let project_item = SessionListItem::Project {
            id: project_id,
            name: "test".to_string(),
            repo_path: PathBuf::from("/tmp"),
            main_branch: "main".to_string(),
            worktree_count: 0,
            nested: false,
        };

        let worktree_item = SessionListItem::Worktree {
            id: session_id,
            project_id,
            title: "test".to_string(),
            branch: "test".to_string(),
            status: SessionStatus::Running,
            program: "claude".to_string(),
            pr_number: None,
            pr_url: None,
            pr_merged: false,
            pr_state: None,
            pr_draft: false,
            pr_labels: Vec::new(),
            worktree_path: PathBuf::from("/tmp/wt"),
            created_at: chrono::Utc::now(),
            agent_state: None,
            unread: false,
            stacked_child: false,
        };

        assert!(project_item.key().starts_with("project:"));
        assert!(worktree_item.key().starts_with("worktree:"));
        assert!(project_item.is_project());
        assert!(worktree_item.is_worktree());
    }

    #[test]
    fn test_is_active() {
        assert!(SessionStatus::Creating.is_active());
        assert!(SessionStatus::Running.is_active());
        assert!(!SessionStatus::Stopped.is_active());
    }

    #[test]
    fn test_creating_cannot_attach() {
        assert!(!SessionStatus::Creating.can_attach());
    }

    #[test]
    fn test_session_status_display() {
        assert_eq!(format!("{}", SessionStatus::Creating), "creating");
        assert_eq!(format!("{}", SessionStatus::Running), "running");
        assert_eq!(format!("{}", SessionStatus::Stopped), "stopped");
    }

    #[test]
    fn test_session_status_paused_alias_deserializes_to_stopped() {
        // Backward compat: state.json files written before pause/resume removal
        // contain `"paused"` which should deserialize as Stopped.
        let status: SessionStatus = serde_json::from_str("\"paused\"").unwrap();
        assert_eq!(status, SessionStatus::Stopped);
    }

    #[test]
    fn test_new_creating_session() {
        let project_id = ProjectId::new();
        let session =
            WorktreeSession::new_creating(project_id, "Feature Auth", "feature-auth", "claude");

        assert_eq!(session.project_id, project_id);
        assert_eq!(session.title, "Feature Auth");
        assert_eq!(session.branch, "feature-auth");
        assert_eq!(session.program, "claude");
        assert_eq!(session.status, SessionStatus::Creating);
        assert_eq!(session.worktree_path, PathBuf::new());
        assert!(session.tmux_session_name.starts_with("cc-"));
    }

    #[test]
    fn test_set_status_running_updates_last_active() {
        let project_id = ProjectId::new();
        let mut session = WorktreeSession::new(
            project_id,
            "Test",
            "branch",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        let before = session.last_active_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        session.set_status(SessionStatus::Running);
        assert!(session.last_active_at > before);
    }

    #[test]
    fn test_set_status_stopped_does_not_update_last_active() {
        let project_id = ProjectId::new();
        let mut session = WorktreeSession::new(
            project_id,
            "Test",
            "branch",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        let before = session.last_active_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        session.set_status(SessionStatus::Stopped);
        assert_eq!(session.last_active_at, before);
    }

    #[test]
    fn test_touch_updates_last_active() {
        let project_id = ProjectId::new();
        let mut session = WorktreeSession::new(
            project_id,
            "Test",
            "branch",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        let before = session.last_active_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        session.touch();
        assert!(session.last_active_at > before);
    }

    #[test]
    fn test_tmux_session_name_format() {
        let session = WorktreeSession::new(
            ProjectId::new(),
            "Test",
            "branch",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        assert!(session.tmux_session_name.starts_with("cc-"));
        assert_eq!(session.tmux_session_name.len(), 11); // "cc-" (3) + 8 hex chars
    }

    #[test]
    fn test_matches_query_empty_string() {
        let session = WorktreeSession::new(
            ProjectId::new(),
            "Anything",
            "any-branch",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        assert!(session.matches_query(""));
    }

    #[test]
    fn test_project_add_multiple_unique_worktrees() {
        let mut project = Project::new("test", PathBuf::from("/tmp/test"), "main");
        let ids: Vec<SessionId> = (0..3).map(|_| SessionId::new()).collect();
        for &id in &ids {
            project.add_worktree(id);
        }
        assert_eq!(project.worktrees.len(), 3);
    }

    #[test]
    fn test_project_remove_nonexistent_worktree() {
        let mut project = Project::new("test", PathBuf::from("/tmp/test"), "main");
        let existing = SessionId::new();
        project.add_worktree(existing);

        project.remove_worktree(&SessionId::new());
        assert_eq!(project.worktrees.len(), 1);
    }

    #[test]
    fn test_session_list_item_predicates_negative() {
        let project_item = SessionListItem::Project {
            id: ProjectId::new(),
            name: "test".to_string(),
            repo_path: PathBuf::from("/tmp"),
            main_branch: "main".to_string(),
            worktree_count: 0,
            nested: false,
        };
        let worktree_item = SessionListItem::Worktree {
            id: SessionId::new(),
            project_id: ProjectId::new(),
            title: "test".to_string(),
            branch: "test".to_string(),
            status: SessionStatus::Running,
            program: "claude".to_string(),
            pr_number: None,
            pr_url: None,
            pr_merged: false,
            pr_state: None,
            pr_draft: false,
            pr_labels: Vec::new(),
            worktree_path: PathBuf::from("/tmp/wt"),
            created_at: chrono::Utc::now(),
            agent_state: None,
            unread: false,
            stacked_child: false,
        };

        assert!(!project_item.is_worktree());
        assert!(!worktree_item.is_project());
    }

    #[test]
    fn test_agent_state_display() {
        assert_eq!(format!("{}", AgentState::Working), "working");
        assert_eq!(format!("{}", AgentState::Idle), "idle");
        assert_eq!(format!("{}", AgentState::WaitingForInput), "waiting");
        assert_eq!(format!("{}", AgentState::Unknown), "unknown");
    }

    use chrono::Duration as ChronoDuration;

    fn session_with(
        branch: &str,
        pr_base: Option<&str>,
        stack_parent: Option<SessionId>,
    ) -> WorktreeSession {
        let mut s = WorktreeSession::new(
            ProjectId::new(),
            "t",
            branch,
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        s.pr_base_branch = pr_base.map(str::to_string);
        s.stack_parent_session_id = stack_parent;
        s
    }

    #[test]
    fn resolve_stack_parent_pr_base_matches_other_session() {
        // Rule 1: pr_base_branch matches a sibling's branch → that's the parent.
        let parent = session_with("base-branch", None, None);
        let child = session_with("child-branch", Some("base-branch"), None);
        let all = [&parent, &child];
        assert_eq!(resolve_stack_parent(&child, &all), Some(parent.id));
    }

    #[test]
    fn resolve_stack_parent_pr_base_matches_main_returns_none() {
        // When pr_base_branch names main (or any branch not owned by a session),
        // the session is a stack root targeting main — no stack parent, even
        // though the local `stack_parent_session_id` hint is set.
        let bogus_hint = SessionId::new();
        let s = session_with("feature", Some("main"), Some(bogus_hint));
        assert_eq!(resolve_stack_parent(&s, &[&s]), None);
    }

    #[test]
    fn resolve_stack_parent_falls_back_to_local_link_when_no_pr() {
        // Rule 3: no PR yet → use the local stack_parent_session_id hint.
        let parent = session_with("base", None, None);
        let child = session_with("child", None, Some(parent.id));
        let all = [&parent, &child];
        assert_eq!(resolve_stack_parent(&child, &all), Some(parent.id));
    }

    #[test]
    fn resolve_stack_parent_ignores_orphaned_local_link() {
        // If stack_parent_session_id refers to a deleted session, treat as unstacked.
        let orphaned_id = SessionId::new();
        let s = session_with("child", None, Some(orphaned_id));
        assert_eq!(resolve_stack_parent(&s, &[&s]), None);
    }

    #[test]
    fn resolve_stack_parent_pr_base_beats_local_link() {
        // When both are set, pr_base_branch (GitHub) wins over the local link.
        let real_parent = session_with("real-base", None, None);
        let fake_parent = session_with("fake-base", None, None);
        let child = session_with("c", Some("real-base"), Some(fake_parent.id));
        let all = [&real_parent, &fake_parent, &child];
        assert_eq!(resolve_stack_parent(&child, &all), Some(real_parent.id));
    }

    #[test]
    fn resolve_stack_parent_unstacked_session_returns_none() {
        let s = session_with("solo", None, None);
        assert_eq!(resolve_stack_parent(&s, &[&s]), None);
    }

    #[test]
    fn stack_top_on_unstacked_session_returns_self() {
        let s = session_with("solo", None, None);
        assert_eq!(stack_top(s.id, &[&s]), s.id);
    }

    #[test]
    fn stack_top_walks_from_base_to_leaf() {
        let base = session_with("base", None, None);
        let mid = session_with("mid", None, Some(base.id));
        let top = session_with("top", None, Some(mid.id));
        let all = [&base, &mid, &top];
        assert_eq!(stack_top(base.id, &all), top.id);
    }

    #[test]
    fn stack_top_from_middle_of_stack_returns_leaf() {
        // Selecting any session in the stack returns the same top.
        let base = session_with("base", None, None);
        let mid = session_with("mid", None, Some(base.id));
        let top = session_with("top", None, Some(mid.id));
        let all = [&base, &mid, &top];
        assert_eq!(stack_top(mid.id, &all), top.id);
        assert_eq!(stack_top(top.id, &all), top.id);
    }

    #[test]
    fn stack_top_with_branching_prefers_most_recent_child() {
        // When a base has multiple direct children, the walker follows the
        // newest one so the user ends up stacked on the branch they most
        // recently worked on.
        let base = session_with("base", None, None);
        let mut older_child = session_with("older", None, Some(base.id));
        older_child.created_at = Utc::now() - ChronoDuration::hours(2);
        let mut newer_child = session_with("newer", None, Some(base.id));
        newer_child.created_at = Utc::now();
        let all = [&base, &older_child, &newer_child];
        assert_eq!(stack_top(base.id, &all), newer_child.id);
    }

    #[test]
    fn stack_chain_from_base_linear_three_levels() {
        let base = session_with("base", None, None);
        let mid = session_with("mid", None, Some(base.id));
        let top = session_with("top", None, Some(mid.id));
        let all = [&base, &mid, &top];
        assert_eq!(
            stack_chain_from_base(base.id, &all),
            vec![base.id, mid.id, top.id]
        );
    }

    #[test]
    fn stack_chain_from_base_unstacked_returns_singleton() {
        let solo = session_with("solo", None, None);
        assert_eq!(stack_chain_from_base(solo.id, &[&solo]), vec![solo.id]);
    }

    #[test]
    fn stack_chain_from_base_with_branching_picks_newest_chain() {
        // mirrors stack_top's tiebreak so both helpers stay consistent
        let base = session_with("base", None, None);
        let mut older_child = session_with("older", None, Some(base.id));
        older_child.created_at = Utc::now() - ChronoDuration::hours(2);
        let mut newer_child = session_with("newer", None, Some(base.id));
        newer_child.created_at = Utc::now();
        let all = [&base, &older_child, &newer_child];
        assert_eq!(
            stack_chain_from_base(base.id, &all),
            vec![base.id, newer_child.id]
        );
    }

    #[test]
    fn stack_chain_from_base_missing_session_still_yields_base() {
        // Defensive: even if the base id isn't in project_sessions, the
        // returned chain starts with it (the caller may look up the session
        // separately).
        let base_id = SessionId::new();
        assert_eq!(stack_chain_from_base(base_id, &[]), vec![base_id]);
    }

    #[test]
    fn stack_root_on_unstacked_session_returns_self() {
        let s = session_with("solo", None, None);
        assert_eq!(stack_root(s.id, &[&s]), s.id);
    }

    #[test]
    fn stack_root_walks_up_chain() {
        let base = session_with("base", None, None);
        let mid = session_with("mid", None, Some(base.id));
        let top = session_with("top", None, Some(mid.id));
        let all = [&base, &mid, &top];
        assert_eq!(stack_root(top.id, &all), base.id);
        assert_eq!(stack_root(mid.id, &all), base.id);
        assert_eq!(stack_root(base.id, &all), base.id);
    }

    #[test]
    fn stack_root_with_fan_out_returns_same_root_for_all_descendants() {
        // A→B and A→C; both branches share A as their root.
        let base = session_with("base", None, None);
        let mut a = session_with("a", None, Some(base.id));
        a.created_at = Utc::now() - ChronoDuration::hours(2);
        let mut b = session_with("b", None, Some(base.id));
        b.created_at = Utc::now();
        let all = [&base, &a, &b];
        assert_eq!(stack_root(a.id, &all), base.id);
        assert_eq!(stack_root(b.id, &all), base.id);
    }

    #[test]
    fn stack_root_missing_session_returns_starting_id() {
        // Defensive: walking from an id not in the slice just returns that id.
        let phantom = SessionId::new();
        assert_eq!(stack_root(phantom, &[]), phantom);
    }

    #[test]
    fn resolve_stack_parent_does_not_match_self_by_branch() {
        // If pr_base_branch somehow equals the session's own branch, don't
        // return self as the parent.
        let mut s = session_with("same", None, None);
        s.pr_base_branch = Some("same".to_string());
        assert_eq!(resolve_stack_parent(&s, &[&s]), None);
    }

    #[test]
    fn serde_round_trip_worktree_session_new_fields_default_when_absent() {
        // Old state.json written before this feature must deserialize cleanly
        // with the new optional fields defaulting to None.
        let id = SessionId::new();
        let project_id = ProjectId::new();
        let json = serde_json::json!({
            "id": id,
            "project_id": project_id,
            "title": "t",
            "branch": "b",
            "worktree_path": "/tmp/wt",
            "status": "running",
            "program": "claude",
            "created_at": "2024-01-01T00:00:00Z",
            "last_active_at": "2024-01-01T00:00:00Z",
            "tmux_session_name": "cc-abcd1234",
        });
        let s: WorktreeSession = serde_json::from_value(json).unwrap();
        assert_eq!(s.pr_base_branch, None);
        assert_eq!(s.stack_parent_session_id, None);
    }

    #[test]
    fn serde_round_trip_worktree_session_new_fields_persist() {
        let parent_id = SessionId::new();
        let mut s = WorktreeSession::new(
            ProjectId::new(),
            "t",
            "b",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        s.pr_base_branch = Some("base".to_string());
        s.stack_parent_session_id = Some(parent_id);

        let json = serde_json::to_string(&s).unwrap();
        let roundtripped: WorktreeSession = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.pr_base_branch.as_deref(), Some("base"));
        assert_eq!(roundtripped.stack_parent_session_id, Some(parent_id));
    }

    #[test]
    fn project_id_from_uuid_round_trips() {
        // Kills the mutant that replaces ProjectId::from_uuid with
        // Default::default(): Default uses Uuid::new_v4(), so the inner UUID
        // would not match the one we pass in.
        let uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        let id = ProjectId::from_uuid(uuid);
        assert_eq!(id.0, uuid);

        // And the nil UUID — Default would never produce this.
        let nil = ProjectId::from_uuid(Uuid::nil());
        assert_eq!(nil.0, Uuid::nil());
    }

    #[test]
    fn session_id_from_uuid_and_as_uuid_round_trip() {
        // Kills two mutants together:
        //   * `from_uuid -> Self with Default::default()` — Default would
        //     generate a fresh v4 UUID, so the round-trip would fail.
        //   * `as_uuid -> &Uuid with Box::leak(Box::new(Default::default()))`
        //     — the leaked value would be Uuid::nil(), not the wrapped UUID.
        let uuid = Uuid::from_u128(0xdead_beef_0000_0000_0000_0000_dead_beef);
        let id = SessionId::from_uuid(uuid);
        assert_eq!(*id.as_uuid(), uuid);

        // Reference identity: as_uuid must borrow the inner field, not return
        // a reference to a freshly leaked value.
        assert!(std::ptr::eq(id.as_uuid(), &id.0));

        // Nil UUID also round-trips — pins down both mutants in a second case.
        let nil = SessionId::from_uuid(Uuid::nil());
        assert_eq!(*nil.as_uuid(), Uuid::nil());
    }

    #[test]
    fn mark_attached_sets_last_attached_at() {
        // Kills the mutant that replaces mark_attached's body with `()`:
        // the field would stay None instead of being set to Some(now).
        let mut session = WorktreeSession::new(
            ProjectId::new(),
            "Test",
            "branch",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        assert!(session.last_attached_at.is_none());

        let before = Utc::now();
        session.mark_attached();
        let after = Utc::now();

        let stamp = session
            .last_attached_at
            .expect("mark_attached must set last_attached_at");
        assert!(stamp >= before);
        assert!(stamp <= after);
    }

    #[test]
    fn session_list_item_spacer_is_not_selectable() {
        // Kills the mutant that makes is_selectable always return true:
        // Spacer rows are never selectable.
        assert!(!SessionListItem::Spacer.is_selectable());

        // And the positive cases still hold, so the assertion above is the
        // discriminating one.
        let project = SessionListItem::Project {
            id: ProjectId::new(),
            name: "p".to_string(),
            repo_path: PathBuf::from("/tmp"),
            main_branch: "main".to_string(),
            worktree_count: 0,
            nested: false,
        };
        assert!(project.is_selectable());
    }
}
