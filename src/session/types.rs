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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
}

impl SessionStatus {
    /// Check if the session is active (creating or running)
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Creating | Self::Running)
    }

    /// Check if the session can be attached to (Stopped sessions are allowed
    /// because get_attach_command will recreate the tmux session automatically)
    pub fn can_attach(&self) -> bool {
        matches!(self, Self::Running | Self::Stopped)
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Creating => write!(f, "creating"),
            Self::Running => write!(f, "running"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

/// Sub-state of a Running Claude Code session, detected via pane content parsing.
/// This is ephemeral (not persisted) and only meaningful when SessionStatus == Running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
            unread: false,
            section_override: None,
            current_section: None,
            entered_section_at: now,
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
            unread: false,
            section_override: None,
            current_section: None,
            entered_section_at: now,
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
}

/// Represents an item in the hierarchical session list
/// Used for UI display and navigation
#[derive(Debug, Clone)]
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
    },
    /// A section header (used only when config.sections is non-empty).
    /// Not selectable — navigation skips these rows.
    SectionHeader { name: String, count: usize },
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
        !matches!(self, Self::SectionHeader { .. } | Self::Spacer)
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
}
