//! Persistent state storage
//!
//! Manages session state persistence in JSON format

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::session::{
    MultiRepoSession, MultiRepoSessionId, Project, ProjectId, SessionId, WorktreeSession,
};

use super::Config;

/// Persistent application state
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    /// All registered projects
    #[serde(default)]
    pub projects: HashMap<ProjectId, Project>,

    /// All worktree sessions
    #[serde(default)]
    pub sessions: HashMap<SessionId, WorktreeSession>,

    /// All multi-repo sessions
    #[serde(default)]
    pub multi_repo_sessions: HashMap<MultiRepoSessionId, MultiRepoSession>,

    /// Whether the user has seen the help screen
    #[serde(default)]
    pub seen_help: bool,

    /// Last selected project ID
    #[serde(default)]
    pub last_selected_project: Option<ProjectId>,

    /// Last selected session ID
    #[serde(default)]
    pub last_selected_session: Option<SessionId>,

    /// Persisted left pane width (percentage of terminal width)
    #[serde(default)]
    pub left_pane_pct: Option<u16>,

    /// Session where an in-flight cascade-merge hit a conflict and paused.
    /// While set, `CascadeResume` (or `CascadeAbandon`) is available; cleared
    /// once resume succeeds or the user abandons. Pairs with the affected
    /// session's `SessionStatus::CascadePaused`.
    #[serde(default)]
    pub cascade_paused_at: Option<SessionId>,

    /// Application version that last wrote this state
    #[serde(default)]
    pub version: String,

    /// Path to save state to (not serialized, set at load time)
    #[serde(skip)]
    state_path: Option<PathBuf>,
}

impl AppState {
    /// Create a new empty state
    pub fn new() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            ..Default::default()
        }
    }

    /// Load state from the default location
    pub fn load() -> Result<Self> {
        let path = Config::state_file_path()?;
        Self::load_from(&path)
    }

    /// Load state from a specific path
    pub fn load_from(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            let mut state = Self::new();
            state.state_path = Some(path.clone());
            return Ok(state);
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::LoadFailed(format!("Failed to read state file: {}", e)))?;

        let mut state: AppState = serde_json::from_str(&content)
            .map_err(|e| ConfigError::LoadFailed(format!("Failed to parse state file: {}", e)))?;

        // Update version and remember path
        state.version = env!("CARGO_PKG_VERSION").to_string();
        state.state_path = Some(path.clone());

        Ok(state)
    }

    /// Save state to the remembered location (or default if none)
    pub fn save(&self) -> Result<()> {
        let path = match &self.state_path {
            Some(p) => p.clone(),
            None => Config::state_file_path()?,
        };
        self.save_to(&path)
    }

    /// Save state to a specific path
    pub fn save_to(&self, path: &PathBuf) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ConfigError::SaveFailed(format!("Failed to create state directory: {}", e))
            })?;
        }

        let content = serde_json::to_string_pretty(self)
            .map_err(|e| ConfigError::SaveFailed(format!("Failed to serialize state: {}", e)))?;

        std::fs::write(path, content)
            .map_err(|e| ConfigError::SaveFailed(format!("Failed to write state file: {}", e)))?;

        Ok(())
    }

    /// Add a project
    pub fn add_project(&mut self, project: Project) {
        self.projects.insert(project.id, project);
    }

    /// Remove a project and all its sessions
    pub fn remove_project(&mut self, project_id: &ProjectId) -> Option<Project> {
        if let Some(project) = self.projects.remove(project_id) {
            // Remove all associated sessions
            for session_id in &project.worktrees {
                self.sessions.remove(session_id);
            }
            Some(project)
        } else {
            None
        }
    }

    /// Get a project by ID
    pub fn get_project(&self, id: &ProjectId) -> Option<&Project> {
        self.projects.get(id)
    }

    /// Get a mutable reference to a project
    pub fn get_project_mut(&mut self, id: &ProjectId) -> Option<&mut Project> {
        self.projects.get_mut(id)
    }

    /// Add a session
    pub fn add_session(&mut self, session: WorktreeSession) {
        let project_id = session.project_id;
        let session_id = session.id;

        self.sessions.insert(session_id, session);

        // Add to parent project
        if let Some(project) = self.projects.get_mut(&project_id) {
            project.add_worktree(session_id);
        }
    }

    /// Remove a session
    pub fn remove_session(&mut self, session_id: &SessionId) -> Option<WorktreeSession> {
        if let Some(session) = self.sessions.remove(session_id) {
            // Remove from parent project
            if let Some(project) = self.projects.get_mut(&session.project_id) {
                project.remove_worktree(session_id);
            }
            Some(session)
        } else {
            None
        }
    }

    /// Get a session by ID
    pub fn get_session(&self, id: &SessionId) -> Option<&WorktreeSession> {
        self.sessions.get(id)
    }

    /// Get a mutable reference to a session
    pub fn get_session_mut(&mut self, id: &SessionId) -> Option<&mut WorktreeSession> {
        self.sessions.get_mut(id)
    }

    /// Get all sessions for a project
    pub fn get_project_sessions(&self, project_id: &ProjectId) -> Vec<&WorktreeSession> {
        self.sessions
            .values()
            .filter(|s| s.project_id == *project_id)
            .collect()
    }

    /// Get all active sessions
    pub fn get_active_sessions(&self) -> Vec<&WorktreeSession> {
        self.sessions
            .values()
            .filter(|s| s.status.is_active())
            .collect()
    }

    /// Count total sessions
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Count total projects
    pub fn project_count(&self) -> usize {
        self.projects.len()
    }

    /// Add a multi-repo session
    pub fn add_multi_repo_session(&mut self, session: MultiRepoSession) {
        self.multi_repo_sessions.insert(session.id, session);
    }

    /// Remove a multi-repo session
    pub fn remove_multi_repo_session(
        &mut self,
        id: &MultiRepoSessionId,
    ) -> Option<MultiRepoSession> {
        self.multi_repo_sessions.remove(id)
    }

    /// Get a multi-repo session by ID
    pub fn get_multi_repo_session(&self, id: &MultiRepoSessionId) -> Option<&MultiRepoSession> {
        self.multi_repo_sessions.get(id)
    }

    /// Get a mutable reference to a multi-repo session
    pub fn get_multi_repo_session_mut(
        &mut self,
        id: &MultiRepoSessionId,
    ) -> Option<&mut MultiRepoSession> {
        self.multi_repo_sessions.get_mut(id)
    }

    /// Count total multi-repo sessions
    pub fn multi_repo_session_count(&self) -> usize {
        self.multi_repo_sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn create_test_project() -> Project {
        Project::new("test-project", PathBuf::from("/tmp/test"), "main")
    }

    fn create_test_session(project_id: ProjectId) -> WorktreeSession {
        WorktreeSession::new(
            project_id,
            "Test Session",
            "test-branch",
            PathBuf::from("/tmp/worktree"),
            "claude",
        )
    }

    fn create_test_session_with_status(
        project_id: ProjectId,
        status: crate::session::SessionStatus,
    ) -> WorktreeSession {
        let mut session = create_test_session(project_id);
        session.set_status(status);
        session
    }

    #[test]
    fn test_new_state() {
        let state = AppState::new();
        assert!(state.projects.is_empty());
        assert!(state.sessions.is_empty());
        assert!(!state.seen_help);
    }

    #[test]
    fn test_add_remove_project() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;

        state.add_project(project);
        assert_eq!(state.project_count(), 1);

        let removed = state.remove_project(&project_id);
        assert!(removed.is_some());
        assert_eq!(state.project_count(), 0);
    }

    #[test]
    fn test_add_remove_session() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let session = create_test_session(project_id);
        let session_id = session.id;

        state.add_session(session);
        assert_eq!(state.session_count(), 1);

        // Check session is linked to project
        let project = state.get_project(&project_id).unwrap();
        assert!(project.worktrees.contains(&session_id));

        // Remove session
        let removed = state.remove_session(&session_id);
        assert!(removed.is_some());
        assert_eq!(state.session_count(), 0);

        // Check session is unlinked from project
        let project = state.get_project(&project_id).unwrap();
        assert!(!project.worktrees.contains(&session_id));
    }

    #[test]
    fn test_section_fields_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let mut session = create_test_session(project_id);
        session.section_override = Some("Needs Review".to_string());
        session.current_section = Some("In Progress".to_string());
        let stamp = session.entered_section_at;
        let session_id = session.id;
        state.add_session(session);

        state.save_to(&state_path).unwrap();
        let loaded = AppState::load_from(&state_path).unwrap();
        let loaded_session = loaded.get_session(&session_id).unwrap();

        assert_eq!(
            loaded_session.section_override.as_deref(),
            Some("Needs Review")
        );
        assert_eq!(
            loaded_session.current_section.as_deref(),
            Some("In Progress")
        );
        assert_eq!(loaded_session.entered_section_at, stamp);
    }

    #[test]
    fn test_save_load_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let session = create_test_session(project_id);
        state.add_session(session);

        state.save_to(&state_path).unwrap();

        let loaded = AppState::load_from(&state_path).unwrap();
        assert_eq!(loaded.project_count(), 1);
        assert_eq!(loaded.session_count(), 1);
    }

    #[test]
    fn test_get_project_sessions() {
        let mut state = AppState::new();

        let project1 = create_test_project();
        let project1_id = project1.id;
        state.add_project(project1);

        let mut project2 = create_test_project();
        project2.name = "other-project".to_string();
        let project2_id = project2.id;
        state.add_project(project2);

        // Add sessions to project1
        state.add_session(create_test_session(project1_id));
        state.add_session(create_test_session(project1_id));

        // Add session to project2
        state.add_session(create_test_session(project2_id));

        let p1_sessions = state.get_project_sessions(&project1_id);
        assert_eq!(p1_sessions.len(), 2);

        let p2_sessions = state.get_project_sessions(&project2_id);
        assert_eq!(p2_sessions.len(), 1);
    }

    #[test]
    fn test_remove_project_cascades_sessions() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let s1 = create_test_session(project_id);
        let s1_id = s1.id;
        let s2 = create_test_session(project_id);
        let s2_id = s2.id;
        let s3 = create_test_session(project_id);
        let s3_id = s3.id;
        state.add_session(s1);
        state.add_session(s2);
        state.add_session(s3);

        assert_eq!(state.session_count(), 3);
        state.remove_project(&project_id);
        assert_eq!(state.session_count(), 0);
        assert!(state.get_session(&s1_id).is_none());
        assert!(state.get_session(&s2_id).is_none());
        assert!(state.get_session(&s3_id).is_none());
    }

    #[test]
    fn test_remove_project_only_cascades_own_sessions() {
        let mut state = AppState::new();

        let project_a = create_test_project();
        let a_id = project_a.id;
        state.add_project(project_a);

        let mut project_b = create_test_project();
        project_b.name = "other".to_string();
        let b_id = project_b.id;
        state.add_project(project_b);

        state.add_session(create_test_session(a_id));
        state.add_session(create_test_session(a_id));
        let b_session = create_test_session(b_id);
        let b_session_id = b_session.id;
        state.add_session(b_session);

        state.remove_project(&a_id);
        assert_eq!(state.session_count(), 1);
        assert!(state.get_session(&b_session_id).is_some());
    }

    #[test]
    fn test_add_session_bidirectional_link() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let session = create_test_session(project_id);
        let session_id = session.id;
        state.add_session(session);

        assert!(state.sessions.contains_key(&session_id));
        assert!(
            state
                .get_project(&project_id)
                .unwrap()
                .worktrees
                .contains(&session_id)
        );
    }

    #[test]
    fn test_remove_session_bidirectional_unlink() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let session = create_test_session(project_id);
        let session_id = session.id;
        state.add_session(session);

        state.remove_session(&session_id);
        assert!(state.sessions.is_empty());
        assert!(state.get_project(&project_id).unwrap().worktrees.is_empty());
    }

    #[test]
    fn test_add_session_nonexistent_project_no_panic() {
        let mut state = AppState::new();
        let orphan_project_id = ProjectId::new();
        let session = create_test_session(orphan_project_id);
        let session_id = session.id;

        state.add_session(session);
        assert_eq!(state.session_count(), 1);
        assert!(state.get_session(&session_id).is_some());
        assert!(state.get_project(&orphan_project_id).is_none());
    }

    #[test]
    fn test_get_active_sessions_filters_correctly() {
        use crate::session::SessionStatus;

        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        state.add_session(create_test_session_with_status(
            project_id,
            SessionStatus::Running,
        ));
        state.add_session(create_test_session_with_status(
            project_id,
            SessionStatus::Stopped,
        ));

        let active = state.get_active_sessions();
        assert_eq!(active.len(), 1);
        assert!(active.iter().all(|s| s.status != SessionStatus::Stopped));
    }

    #[test]
    fn test_get_project_sessions_empty_for_unknown_project() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let sessions = state.get_project_sessions(&project_id);
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_remove_nonexistent_session_returns_none() {
        let mut state = AppState::new();
        assert!(state.remove_session(&SessionId::new()).is_none());
    }

    #[test]
    fn test_remove_nonexistent_project_returns_none() {
        let mut state = AppState::new();
        assert!(state.remove_project(&ProjectId::new()).is_none());
    }

    #[test]
    fn test_left_pane_pct_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        let mut state = AppState::new();
        state.left_pane_pct = Some(42);
        state.save_to(&state_path).unwrap();

        let loaded = AppState::load_from(&state_path).unwrap();
        assert_eq!(loaded.left_pane_pct, Some(42));
    }

    #[test]
    fn test_left_pane_pct_defaults_to_none() {
        let state = AppState::new();
        assert_eq!(state.left_pane_pct, None);
    }

    #[test]
    fn test_left_pane_pct_missing_from_json_defaults_to_none() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        // Write JSON without left_pane_pct field
        std::fs::write(&state_path, r#"{"seen_help": true, "version": "0.1.0"}"#).unwrap();

        let loaded = AppState::load_from(&state_path).unwrap();
        assert_eq!(loaded.left_pane_pct, None);
        assert!(loaded.seen_help);
    }

    // --- Multi-repo session tests ---

    #[test]
    fn test_add_remove_multi_repo_session() {
        let mut state = AppState::new();
        let session =
            crate::session::MultiRepoSession::new_creating("Update deps", "update-deps", "claude");
        let id = session.id;

        state.add_multi_repo_session(session);
        assert_eq!(state.multi_repo_session_count(), 1);
        assert!(state.get_multi_repo_session(&id).is_some());

        let removed = state.remove_multi_repo_session(&id);
        assert!(removed.is_some());
        assert_eq!(state.multi_repo_session_count(), 0);
    }

    #[test]
    fn test_multi_repo_session_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        let mut state = AppState::new();
        let mut session = crate::session::MultiRepoSession::new_creating(
            "Cross-repo fix",
            "cross-repo-fix",
            "claude",
        );
        session.repos.push(crate::session::MultiRepoEntry {
            project_id: ProjectId::new(),
            worktree_path: PathBuf::from("/tmp/repo-a"),
            base_commit: Some("abc123".to_string()),
        });
        let id = session.id;
        state.add_multi_repo_session(session);

        state.save_to(&state_path).unwrap();
        let loaded = AppState::load_from(&state_path).unwrap();
        assert_eq!(loaded.multi_repo_session_count(), 1);
        let loaded_session = loaded.get_multi_repo_session(&id).unwrap();
        assert_eq!(loaded_session.title, "Cross-repo fix");
        assert_eq!(loaded_session.repos.len(), 1);
        assert_eq!(
            loaded_session.repos[0].base_commit,
            Some("abc123".to_string())
        );
    }

    #[test]
    fn test_multi_repo_sessions_missing_from_json_defaults_to_empty() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        // Write JSON without multi_repo_sessions field
        std::fs::write(&state_path, r#"{"version": "0.1.0"}"#).unwrap();

        let loaded = AppState::load_from(&state_path).unwrap();
        assert_eq!(loaded.multi_repo_session_count(), 0);
    }

    /// A multi-repo session must be findable in `multi_repo_sessions` by its
    /// `tmux_session_name`. The restart-by-tmux-name path in `lifecycle.rs`
    /// performs exactly this lookup; if it ever drops back to only searching
    /// `state.sessions`, multi-repo sessions silently fail to restart when
    /// their Claude process exits.
    #[test]
    fn test_multi_repo_session_findable_by_tmux_name() {
        let mut state = AppState::new();
        let session =
            crate::session::MultiRepoSession::new_creating("Cross-repo", "cross-repo", "claude");
        let tmux_name = session.tmux_session_name.clone();
        let id = session.id;
        state.add_multi_repo_session(session);

        let found = state
            .multi_repo_sessions
            .values()
            .find(|s| s.tmux_session_name == tmux_name);
        assert!(found.is_some(), "multi-repo session not found by tmux name");
        assert_eq!(found.unwrap().id, id);

        // And a single-repo session lookup with the same name must NOT match.
        let found_in_sessions = state
            .sessions
            .values()
            .find(|s| s.tmux_session_name == tmux_name);
        assert!(found_in_sessions.is_none());
    }

    /// `cc-mr-` and `cc-` namespaces must not collide: a single-repo session
    /// lookup by tmux name must never match a multi-repo session, and
    /// vice-versa.
    #[test]
    fn test_session_tmux_namespaces_disjoint() {
        let mut state = AppState::new();

        let mr = crate::session::MultiRepoSession::new_creating("a", "a", "claude");
        let mr_tmux = mr.tmux_session_name.clone();
        state.add_multi_repo_session(mr);

        let sr = crate::session::WorktreeSession::new(
            ProjectId::new(),
            "b",
            "b",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        let sr_tmux = sr.tmux_session_name.clone();
        state.add_session(sr);

        // mr_tmux only matches in multi_repo_sessions.
        assert!(
            state
                .multi_repo_sessions
                .values()
                .any(|s| s.tmux_session_name == mr_tmux)
        );
        assert!(
            !state
                .sessions
                .values()
                .any(|s| s.tmux_session_name == mr_tmux)
        );

        // sr_tmux only matches in sessions.
        assert!(
            state
                .sessions
                .values()
                .any(|s| s.tmux_session_name == sr_tmux)
        );
        assert!(
            !state
                .multi_repo_sessions
                .values()
                .any(|s| s.tmux_session_name == sr_tmux)
        );
    }

    #[test]
    fn test_remove_nonexistent_multi_repo_session_returns_none() {
        let mut state = AppState::new();
        assert!(
            state
                .remove_multi_repo_session(&crate::session::MultiRepoSessionId::new())
                .is_none()
        );
    }

    // --- Cascade-paused-at tests ---

    #[test]
    fn test_cascade_paused_at_defaults_to_none() {
        let state = AppState::new();
        assert!(state.cascade_paused_at.is_none());
    }

    #[test]
    fn test_cascade_paused_at_roundtrips_through_json() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        let mut state = AppState::new();
        let sid = SessionId::new();
        state.cascade_paused_at = Some(sid);
        state.save_to(&state_path).unwrap();

        let loaded = AppState::load_from(&state_path).unwrap();
        assert_eq!(loaded.cascade_paused_at, Some(sid));
    }

    #[test]
    fn test_cascade_paused_at_missing_from_json_defaults_to_none() {
        // A state file written before this feature must still load cleanly.
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");
        std::fs::write(&state_path, r#"{"seen_help": true, "version": "0.1.0"}"#).unwrap();
        let loaded = AppState::load_from(&state_path).unwrap();
        assert!(loaded.cascade_paused_at.is_none());
    }
}
