//! Core session types
//!
//! Defines the hierarchical session model:
//! - `Project` represents a git repository
//! - `WorktreeSession` represents a worktree session within a project

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// Session identity + status are network wire types, so they live in the shared
// `claude-commander-protocol` crate (`Serialize + Deserialize`, mobile-safe).
// Re-exported here so `crate::session::{SessionId, SessionStatus, ...}` paths
// and the `WorktreeSession`/`Project` model below keep working unchanged.
pub use claude_commander_protocol::session::{AgentState, ProjectId, SessionId, SessionStatus};

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
    /// URL of the repository's `origin` remote, as git resolves it (after any
    /// `url.<base>.insteadOf` rewrite). `None` when the repo has no `origin` —
    /// a valid resting state, not a pending fetch.
    ///
    /// Persisted rather than derived: the only consumer, the project projection
    /// [`build_project_info_list`](crate::api), is a pure synchronous fold over
    /// `AppState` on the workspace-poll path, so deriving it would mean opening
    /// a `gix` repo per project per poll. Kept true to the repo by
    /// `SessionManager::sync_worktrees`, which already holds an open repo: it
    /// fills the field for projects added before it existed *and* corrects it
    /// when a repo is renamed, transferred, or has its remote re-pointed.
    #[serde(default)]
    pub origin_url: Option<String>,
    /// Name of the workspace this project is tagged with; `None` is the
    /// built-in Main workspace. A label only — every workspace shares this
    /// state file. An older binary's write drops it (the project falls back to
    /// Main); that risk was accepted, and [`Self::extra`] closes it from here on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Every field this binary does not model, kept verbatim so a newer
    /// binary's additions survive this one's writes. Must stay the last field
    /// and must stay `flatten`: serde routes only *unclaimed* keys here.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
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
            origin_url: None,
            workspace: None,
            extra: serde_json::Map::new(),
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
    /// Launch command saved for the next fresh restart. The live pane is unchanged.
    #[serde(default)]
    pub pending_program: Option<String>,
    /// When the session was created
    pub created_at: DateTime<Utc>,
    /// When the session was last active
    pub last_active_at: DateTime<Utc>,
    /// Tmux session name (for tmux commands)
    pub tmux_session_name: String,
    /// Base commit for diff computation (branch point). A *frozen* SHA captured
    /// at creation — kept only as a last-resort fallback for [`base_branch`].
    #[serde(default)]
    pub base_commit: Option<String>,
    /// The branch this session was forked from (a stack parent's branch, an
    /// explicit `--base-branch`, or the project's main branch). The review diff
    /// resolves its base against the *live* tip of this branch — mirroring how a
    /// GitHub PR diffs against the current state of its target — so the diff
    /// stays correct as the target advances or is merged back in. Falls back to
    /// the frozen [`base_commit`] when the branch can no longer be resolved.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Shell tmux session name (for secondary shell sessions)
    #[serde(default)]
    pub shell_tmux_session_name: Option<String>,
    /// Hosted review number (GitHub PR number or GitLab MR IID), if present.
    #[serde(default)]
    pub pr_number: Option<u32>,
    /// Hosted pull-request or merge-request URL.
    #[serde(default)]
    pub pr_url: Option<String>,
    /// Whether the review has been merged (kept for backward compat — derived from pr_state).
    #[serde(default)]
    pub pr_merged: bool,
    /// Review lifecycle state (open / closed / merged). None = unknown / absent.
    #[serde(default)]
    pub pr_state: Option<crate::git::PrState>,
    /// Whether the pull request or merge request is a draft.
    #[serde(default)]
    pub pr_draft: bool,
    /// Label names attached to the hosted review (used for review-needed colouring).
    #[serde(default)]
    pub pr_labels: Vec<String>,
    /// GitHub `reviewDecision`; `None` for GitLab and when no decision data exists.
    #[serde(default)]
    pub review_decision: Option<crate::git::ReviewDecision>,
    /// Reviewer usernames on the hosted review. Empty when absent or unassigned.
    #[serde(default)]
    pub pr_reviewers: Vec<String>,
    /// Target branch reported by the selected provider (e.g. `main` or another
    /// session's branch), used as the source of truth for stack detection.
    #[serde(default)]
    pub pr_base_branch: Option<String>,
    /// When the code-host check whose result the PR fields above reflect
    /// *started*. A check stamps this as it applies, and a result from a check
    /// that started earlier than the stored stamp is dropped (see
    /// [`Self::accept_pr_check`]) — so a background poll that was already in
    /// flight when `claude-commander refresh-pr` ran cannot overwrite the
    /// fresher data the refresh persisted. `None` = never checked (or written
    /// by a binary that predates the field), which any check may overwrite.
    ///
    /// Persisted but not client-visible (no wire DTO carries it), so a write
    /// that changes only this stamp does not wake the change-feed — see
    /// `AppState::client_visible`.
    #[serde(default)]
    pub pr_checked_at: Option<DateTime<Utc>>,
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
    /// Alt+Tab-style MRU ordering in the in-session switcher.
    /// `None` for sessions never attached since adopting the field.
    #[serde(default)]
    pub last_attached_at: Option<DateTime<Utc>>,
    /// User opt-out of auto-hibernation. When true, the background
    /// hibernation policy never stops this session regardless of how long it
    /// has been idle. Toggled per-session from the TUI/CLI.
    #[serde(default)]
    pub keep_alive: bool,
    /// When this session adopted its *current* branch name, if that happened
    /// after creation (a rename picked up by `reconcile_session_branches`).
    /// `None` means the session has held [`Self::branch`] since it was created.
    /// Feeds [`Self::branch_owned_since`], which bounds how far back a PR
    /// matched by branch *name* may be — see
    /// [`crate::git::check_pr_for_branch`].
    #[serde(default)]
    pub branch_adopted_at: Option<DateTime<Utc>>,
    /// Set when this session was stopped non-destructively — by the
    /// auto-hibernation policy or a manual kill that kept the worktree.
    /// Drives the wake path to resume the prior agent conversation *even
    /// when* the global `resume_session` config is off — stopping is only
    /// non-destructive with `--resume`. Cleared when the session is next
    /// recreated.
    #[serde(default)]
    pub hibernated: bool,
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
        let tmux_session_name = format!("cc-{}", &id.as_uuid().to_string()[..8]);

        Self {
            id,
            project_id,
            title,
            branch: branch.into(),
            worktree_path,
            status: SessionStatus::Running,
            program: program.into(),
            pending_program: None,
            created_at: now,
            last_active_at: now,
            tmux_session_name,
            base_commit: None,
            base_branch: None,
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
            pr_checked_at: None,
            stack_parent_session_id: None,
            unread: false,
            section_override: None,
            current_section: None,
            entered_section_at: now,
            last_attached_at: None,
            keep_alive: false,
            branch_adopted_at: None,
            hibernated: false,
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
        let tmux_session_name = format!("cc-{}", &id.as_uuid().to_string()[..8]);

        Self {
            id,
            project_id,
            title,
            branch: branch.into(),
            worktree_path: PathBuf::new(),
            status: SessionStatus::Creating,
            program: program.into(),
            pending_program: None,
            created_at: now,
            last_active_at: now,
            tmux_session_name,
            base_commit: None,
            base_branch: None,
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
            pr_checked_at: None,
            stack_parent_session_id: None,
            unread: false,
            section_override: None,
            current_section: None,
            entered_section_at: now,
            last_attached_at: None,
            keep_alive: false,
            branch_adopted_at: None,
            hibernated: false,
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
        claude_commander_viewmodel::session_score(&self.title, &self.branch, &self.program, query)
    }

    /// The instant from which this session has held its current branch name:
    /// [`Self::branch_adopted_at`] when a rename was adopted, else
    /// [`Self::created_at`]. Never earlier than creation, so a stale adoption
    /// stamp can't widen the window.
    ///
    /// A PR is matched to a session by branch *name*, so anything that already
    /// settled before this instant belongs to some earlier occupant of the name
    /// — see [`crate::git::check_pr_for_branch`].
    pub fn branch_owned_since(&self) -> DateTime<Utc> {
        self.branch_adopted_at
            .map_or(self.created_at, |adopted| adopted.max(self.created_at))
    }

    /// Clear every field populated by hosted pull/merge-request polling.
    /// Provider changes use this as one operation so metadata discovered by one
    /// CLI can never be interpreted or mutated through another provider.
    pub(crate) fn clear_review_metadata(&mut self) {
        self.pr_number = None;
        self.pr_url = None;
        self.pr_state = None;
        self.pr_draft = false;
        self.pr_labels.clear();
        self.pr_merged = false;
        self.review_decision = None;
        self.pr_reviewers.clear();
        self.pr_base_branch = None;
    }

    /// Whether a PR check that started at `checked_at` may write this session's
    /// review metadata, stamping it when it may. A check that started before
    /// the one already applied lost a race (it was in flight while a newer one
    /// finished), so its result is older than what is stored and is dropped.
    /// A tie applies: both checks started at once, so neither is staler.
    ///
    /// The stamp is wall-clock (state.json is shared between processes, so a
    /// monotonic clock cannot be compared), which means a backwards clock step
    /// makes checks look stale until the clock passes the stored stamp again.
    /// `now` is the current time, the stamp's sanity bound: no check can have
    /// started after it, so a stored stamp beyond `now` was written by a clock
    /// that ran ahead and is ignored, rather than freezing the session's PR
    /// data until real time catches up.
    pub(crate) fn accept_pr_check(
        &mut self,
        checked_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> bool {
        if self
            .pr_checked_at
            .is_some_and(|stored| stored > checked_at && stored <= now)
        {
            return false;
        }
        // Advanced on every accepted check, even one whose PR data is
        // unchanged, so each successful poll rewrites state.json
        // (`StateStore::mutate` skips the write only when the serialised state
        // is identical). It does so silently: the field is left out of
        // `AppState::client_visible`, so a stamp-only write wakes no
        // change-feed subscriber. The write itself is required: if
        // an unchanged result left the stamp where it was, a check that
        // started before this one but lands after it would still compare as
        // newer than the stored stamp and overwrite this result with older
        // host data.
        self.pr_checked_at = Some(checked_at);
        true
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

/// The read-only view of a session the stack-resolution and section-grouping
/// algorithms need. Implemented by both the in-process [`WorktreeSession`]
/// domain model and the wire [`SessionInfo`](claude_commander_protocol::api::SessionInfo)
/// DTO, so the same algorithms drive the local TUI tree and a remote client's
/// tree without the algorithms being duplicated per representation.
///
/// A trait (rather than a converting view-struct) keeps the hot builders
/// allocation-free — `stack_top`/`stack_root` are called per-session inside the
/// section-stacks builder, so the generic monomorphised form matters for the
/// tree-build perf budget.
pub trait SessionNode {
    fn node_id(&self) -> SessionId;
    fn node_branch(&self) -> &str;
    fn node_pr_base_branch(&self) -> Option<&str>;
    fn node_stack_parent_session_id(&self) -> Option<SessionId>;
    fn node_created_at(&self) -> DateTime<Utc>;
    /// Cached section the session currently sits in (`None` = In Progress).
    fn node_current_section(&self) -> Option<&str>;
    /// When the session entered its current section (drives in-section sort).
    fn node_entered_section_at(&self) -> DateTime<Utc>;
}

impl SessionNode for WorktreeSession {
    fn node_id(&self) -> SessionId {
        self.id
    }
    fn node_branch(&self) -> &str {
        &self.branch
    }
    fn node_pr_base_branch(&self) -> Option<&str> {
        self.pr_base_branch.as_deref()
    }
    fn node_stack_parent_session_id(&self) -> Option<SessionId> {
        self.stack_parent_session_id
    }
    fn node_created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    fn node_current_section(&self) -> Option<&str> {
        self.current_section.as_deref()
    }
    fn node_entered_section_at(&self) -> DateTime<Utc> {
        self.entered_section_at
    }
}

impl SessionNode for claude_commander_protocol::api::SessionInfo {
    fn node_id(&self) -> SessionId {
        self.session_id
    }
    fn node_branch(&self) -> &str {
        &self.branch
    }
    fn node_pr_base_branch(&self) -> Option<&str> {
        self.pr_base_branch.as_deref()
    }
    fn node_stack_parent_session_id(&self) -> Option<SessionId> {
        self.stack_parent_session_id
    }
    fn node_created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    fn node_current_section(&self) -> Option<&str> {
        self.current_section.as_deref()
    }
    fn node_entered_section_at(&self) -> DateTime<Utc> {
        // The wire DTO carries this as `Option` (older clients may omit it);
        // treat an absent value as the epoch default so ordering is total.
        self.entered_section_at.unwrap_or_default()
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
pub fn resolve_stack_parent<S: SessionNode>(
    session: &S,
    project_sessions: &[&S],
) -> Option<SessionId> {
    if let Some(base) = session.node_pr_base_branch() {
        return project_sessions
            .iter()
            .find(|s| s.node_id() != session.node_id() && s.node_branch() == base)
            .map(|s| s.node_id());
    }
    let parent_id = session.node_stack_parent_session_id()?;
    project_sessions
        .iter()
        .any(|s| s.node_id() == parent_id)
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
pub fn stack_top<S: SessionNode>(session_id: SessionId, project_sessions: &[&S]) -> SessionId {
    let mut current = session_id;
    // Bounded by number of sessions to avoid ever spinning on a corrupted cycle.
    for _ in 0..project_sessions.len() {
        let next_child = project_sessions
            .iter()
            .filter(|s| resolve_stack_parent(**s, project_sessions) == Some(current))
            .max_by_key(|s| s.node_created_at());
        match next_child {
            Some(child) => current = child.node_id(),
            None => return current,
        }
    }
    current
}

/// Why a request to retarget a session's stack base was refused.
///
/// Self-contained (no `#[from]` back into core's hierarchy), so the reason
/// reaches a frontend typed rather than as a formatted string — the server maps
/// the malformed variants to 400 and the state-conflict variants to 409.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetBaseRejection {
    #[error("the session no longer exists")]
    SessionNotFound,

    #[error("the chosen base session no longer exists")]
    ParentNotFound,

    #[error("a session can only be based on another session in the same project")]
    DifferentProject,

    #[error("a session cannot be based on itself")]
    SelfParent,

    #[error(
        "'{parent_title}' is stacked on this session — basing this session on it would create a cycle"
    )]
    WouldCycle { parent_title: String },

    #[error("'{branch}' is already the base of this session")]
    AlreadyBased { branch: String },

    #[error(
        "this session's pull request is {state} — restacking it would be undone by the next PR sync"
    )]
    PrNotOpen { state: &'static str },

    #[error(
        "a paused cascade is in progress for this project — resume or abandon it before restacking"
    )]
    CascadeInProgress,
}

/// Whether `candidate` sits anywhere *below* `ancestor` in the stack — i.e.
/// walking `candidate`'s parents eventually reaches `ancestor`.
///
/// This is the cycle guard for retargeting a session's base: making `ancestor`
/// stack onto one of its own descendants would close a loop. That is not
/// cosmetic — [`crate::session::board::build_session_order`] has no iteration
/// bound and treats a cycle as having no root, so every member of the cycle is
/// dropped from the board and session list entirely.
///
/// Bounded by `project_sessions.len()` the same way [`stack_root`] is, so an
/// already-corrupted cycle terminates instead of spinning. Returns `false` when
/// `candidate == ancestor` — identity is checked separately by the caller so
/// the two rejections can be reported distinctly.
pub fn is_descendant_of<S: SessionNode>(
    candidate: SessionId,
    ancestor: SessionId,
    project_sessions: &[&S],
) -> bool {
    let mut current = candidate;
    for _ in 0..project_sessions.len() {
        let Some(parent) = project_sessions
            .iter()
            .find(|s| s.node_id() == current)
            .and_then(|s| resolve_stack_parent(*s, project_sessions))
        else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        current = parent;
    }
    false
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
pub fn stack_root<S: SessionNode>(session_id: SessionId, project_sessions: &[&S]) -> SessionId {
    let mut current = session_id;
    // Bounded by session count to be safe against any malformed cycle.
    for _ in 0..project_sessions.len() {
        let this = project_sessions.iter().find(|s| s.node_id() == current);
        match this.and_then(|s| resolve_stack_parent(*s, project_sessions)) {
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
pub fn stack_chain_from_base<S: SessionNode>(
    base_id: SessionId,
    project_sessions: &[&S],
) -> Vec<SessionId> {
    let mut chain = vec![base_id];
    let mut current = base_id;
    for _ in 0..project_sessions.len() {
        let next_child = project_sessions
            .iter()
            .filter(|s| resolve_stack_parent(**s, project_sessions) == Some(current))
            .max_by_key(|s| s.node_created_at());
        match next_child {
            Some(child) => {
                current = child.node_id();
                chain.push(current);
            }
            None => break,
        }
    }
    chain
}

/// A row in the session list, or a card on the board.
///
/// The board draws only `Worktree` rows (grouped under its own columns and
/// sidebar); the list views additionally render the header, spacer and recents
/// variants. Everything downstream — widgets, hit-testing, action gating —
/// pattern-matches on these variants, so a row's kind is what decides both how
/// it draws and whether it is selectable (see
/// [`is_selectable`](Self::is_selectable)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionListItem {
    /// A project header (list views). When `nested`, it renders indented one
    /// level as a project sub-header under a section header.
    Project {
        id: ProjectId,
        name: String,
        repo_path: PathBuf,
        main_branch: String,
        worktree_count: usize,
        nested: bool,
    },
    /// A worktree session (a card row under its project)
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
        /// True when the user has opted this session out of auto-hibernation
        /// (the keep-alive toggle). Surfaced as a tree-row marker so the flag is
        /// visible without toggling it to find out.
        keep_alive: bool,
        /// True while a background `git lfs pull` is materialising this
        /// session's LFS content (the worktree was created with smudging
        /// skipped). Drives the `⇣ LFS` row marker. Sourced from
        /// `UiState::lfs_pull_in_flight`, not persisted.
        lfs_pulling: bool,
        /// True when this row is a stacked child of the row directly above it,
        /// meaning it sits one indent deeper than a normal session row. Stack
        /// bases and unstacked sessions keep the normal indent and have this
        /// set to `false`.
        stacked_child: bool,
    },
    /// A per-backend header grouping one server's projects and sessions (list
    /// views). Emitted only when more than one backend is configured — a lone
    /// local backend is suppressed. Carries connection health + an optional
    /// version-mismatch warning for live status rendering.
    ServerHeader {
        backend: crate::backend::BackendId,
        name: String,
        connection: crate::backend::ConnectionState,
        version_warning: Option<crate::backend::VersionMismatch>,
    },
    /// A section header (list section views; used when sections are active).
    SectionHeader {
        name: String,
        count: usize,
        collapsed: bool,
        /// Advisory WIP limit resolved from config. `None` = no limit.
        max_sessions: Option<u32>,
    },
    /// A blank spacer row for visual separation between sections. Not selectable.
    Spacer,
    /// Label row heading the "Recent" block at the very top of the list.
    /// A view over recently-attached sessions, independent of any backend.
    /// Not selectable — the cursor skips it.
    RecentsHeader,
    /// A recent-session shortcut row in the top "Recent" block. It mirrors a
    /// real [`Worktree`](Self::Worktree) row that still appears in its normal
    /// place below; it carries the backend-qualified [`SessionRef`] so
    /// selection and actions resolve to that same session. Its displayed
    /// number and project colour are looked up from the real row at render
    /// time, so they always match the session's usual position.
    RecentSession {
        session: crate::backend::SessionRef,
        project_id: ProjectId,
        title: String,
        status: SessionStatus,
        agent_state: Option<AgentState>,
        unread: bool,
        /// The following fields mirror the real [`Worktree`](Self::Worktree) row
        /// this shortcut points at, so the recents row renders identically. See
        /// the `Worktree` variant for the individual field meanings.
        branch: String,
        program: String,
        keep_alive: bool,
        lfs_pulling: bool,
        pr_number: Option<u32>,
        pr_url: Option<String>,
        pr_merged: bool,
        pr_state: Option<crate::git::PrState>,
        pr_draft: bool,
        pr_labels: Vec<String>,
    },
}

impl SessionListItem {
    /// A unique key for this item (for selection tracking across rebuilds).
    pub fn key(&self) -> String {
        match self {
            Self::Project { id, .. } => format!("project:{}", id),
            Self::Worktree { id, .. } => format!("worktree:{}", id),
            Self::ServerHeader { backend, .. } => format!("server:{}", backend.0),
            Self::SectionHeader { name, .. } => format!("section:{}", name),
            Self::Spacer => "spacer".to_string(),
            Self::RecentsHeader => "recents-header".to_string(),
            Self::RecentSession { session, .. } => format!("recent:{}", session.id),
        }
    }

    /// Whether this is a project header.
    pub fn is_project(&self) -> bool {
        matches!(self, Self::Project { .. })
    }

    /// Whether this is a worktree session row.
    pub fn is_worktree(&self) -> bool {
        matches!(self, Self::Worktree { .. })
    }

    /// Whether navigation/selection should land on this row (everything but a
    /// spacer).
    pub fn is_selectable(&self) -> bool {
        !matches!(self, Self::Spacer | Self::RecentsHeader)
    }

    /// Whether this row begins a group — a project, section, or server header.
    /// Group-jump navigation (`NextGroup`/`PreviousGroup`) moves between these.
    pub fn is_group_header(&self) -> bool {
        matches!(
            self,
            Self::Project { .. } | Self::SectionHeader { .. } | Self::ServerHeader { .. }
        )
    }

    /// Whether this row is a per-backend server header.
    pub fn is_server_header(&self) -> bool {
        matches!(self, Self::ServerHeader { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `state.json` project written before workspaces existed is in Main.
    #[test]
    fn a_project_without_a_workspace_field_is_in_main() {
        let json = r#"{
            "id": "1b4e28ba-2fa1-11d2-883f-b9a761bde3fb",
            "name": "repo", "repo_path": "/repo", "main_branch": "main",
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let project: Project = serde_json::from_str(json).unwrap();
        assert_eq!(project.workspace, None);
        assert!(project.extra.is_empty());
    }

    /// Fields this binary doesn't know (written by a *newer* one) survive a
    /// load/save round trip instead of being dropped by our next write. That is
    /// what `Project::extra` is for — `workspace` itself was lost this way by
    /// every binary that predates it.
    #[test]
    fn unknown_project_fields_survive_a_round_trip() {
        let json = r#"{
            "id": "1b4e28ba-2fa1-11d2-883f-b9a761bde3fb",
            "name": "repo", "repo_path": "/repo", "main_branch": "main",
            "created_at": "2026-01-01T00:00:00Z",
            "workspace": "Work",
            "future_flag": true,
            "future_table": {"nested": [1, 2, 3]}
        }"#;
        let project: Project = serde_json::from_str(json).unwrap();
        assert_eq!(project.workspace.as_deref(), Some("Work"));
        let out: serde_json::Value = serde_json::to_value(&project).unwrap();
        assert_eq!(out["workspace"], "Work");
        assert_eq!(out["future_flag"], true);
        assert_eq!(out["future_table"]["nested"][2], 3);
        // Known fields are not duplicated into the catch-all.
        assert!(!project.extra.contains_key("workspace"));
        assert!(!project.extra.contains_key("name"));
    }

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
    fn project_deserialises_without_origin_url() {
        // A `state.json` written by a binary that predates `origin_url` (or by
        // an older binary still running alongside this one) has no such key.
        // It must load with the field absent, not fail and wipe the project.
        let json = r#"{
            "id": "1b4e28ba-2fa1-11d2-883f-b9a761bde3fb",
            "name": "r",
            "repo_path": "/repos/r",
            "main_branch": "main",
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let project: Project = serde_json::from_str(json).unwrap();
        assert_eq!(project.name, "r");
        assert_eq!(project.origin_url, None);
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

    /// `pr_checked_at` is new in state.json, which older binaries also write: a
    /// record without it loads as "never checked" rather than being rejected.
    #[test]
    fn session_without_pr_checked_at_deserializes() {
        let session = WorktreeSession::new(
            ProjectId::new(),
            "x",
            "b",
            PathBuf::from("/tmp/x"),
            "claude",
        );
        let mut value = serde_json::to_value(&session).unwrap();
        value.as_object_mut().unwrap().remove("pr_checked_at");
        let back: WorktreeSession = serde_json::from_value(value).unwrap();
        assert!(back.pr_checked_at.is_none());
    }

    #[test]
    fn accept_pr_check_drops_only_strictly_older_checks() {
        let mut session = WorktreeSession::new(
            ProjectId::new(),
            "x",
            "b",
            PathBuf::from("/tmp/x"),
            "claude",
        );
        let t = Utc::now();
        assert!(
            session.accept_pr_check(t, t),
            "an unchecked session accepts"
        );
        assert!(session.accept_pr_check(t, t), "a tie applies");
        assert!(!session.accept_pr_check(t - chrono::Duration::seconds(1), t));
        assert_eq!(
            session.pr_checked_at,
            Some(t),
            "a dropped check leaves the stamp"
        );
        let later = t + chrono::Duration::seconds(1);
        assert!(session.accept_pr_check(later, later));
        assert_eq!(session.pr_checked_at, Some(later));
    }

    /// A stamp later than the current time was written under a clock that ran
    /// ahead and has since been corrected. Honouring it would freeze the
    /// session's PR data until real time caught up, so it counts as stale.
    #[test]
    fn accept_pr_check_ignores_a_stored_stamp_from_the_future() {
        let mut session = WorktreeSession::new(
            ProjectId::new(),
            "x",
            "b",
            PathBuf::from("/tmp/x"),
            "claude",
        );
        let now = Utc::now();
        let ahead = now + chrono::Duration::hours(3);
        assert!(session.accept_pr_check(ahead, ahead));
        assert!(session.accept_pr_check(now, now), "a future stamp is stale");
        assert_eq!(session.pr_checked_at, Some(now));
    }

    #[test]
    fn test_branch_owned_since_defaults_to_creation() {
        let session = WorktreeSession::new(
            ProjectId::new(),
            "x",
            "b",
            PathBuf::from("/tmp/x"),
            "claude",
        );
        assert!(session.branch_adopted_at.is_none());
        assert_eq!(session.branch_owned_since(), session.created_at);
    }

    #[test]
    fn test_branch_owned_since_uses_adoption_when_later() {
        // A rename adopted mid-life moves the window forward: PRs that settled
        // on the new name before the adoption belong to its previous occupant.
        let mut session = WorktreeSession::new(
            ProjectId::new(),
            "x",
            "b",
            PathBuf::from("/tmp/x"),
            "claude",
        );
        let adopted = session.created_at + chrono::Duration::days(7);
        session.branch_adopted_at = Some(adopted);
        assert_eq!(session.branch_owned_since(), adopted);
    }

    #[test]
    fn test_branch_owned_since_never_predates_creation() {
        // A nonsensical stamp (clock step, hand-edited state.json) must not widen
        // the window back past creation.
        let mut session = WorktreeSession::new(
            ProjectId::new(),
            "x",
            "b",
            PathBuf::from("/tmp/x"),
            "claude",
        );
        session.branch_adopted_at = Some(session.created_at - chrono::Duration::days(30));
        assert_eq!(session.branch_owned_since(), session.created_at);
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
        let title_only = claude_commander_viewmodel::fuzzy_score("payments", "payments").unwrap();
        let combined = session.fuzzy_score("payments").unwrap();
        assert_eq!(combined, title_only);
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
        assert_eq!(
            stack_chain_from_base::<WorktreeSession>(base_id, &[]),
            vec![base_id]
        );
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
        assert_eq!(stack_root::<WorktreeSession>(phantom, &[]), phantom);
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
        // Hibernation fields must default for pre-feature state.json files.
        assert!(!s.keep_alive);
        assert!(!s.hibernated);
    }

    #[test]
    fn serde_round_trip_hibernation_fields_persist() {
        let mut s = WorktreeSession::new(
            ProjectId::new(),
            "t",
            "b",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        s.keep_alive = true;
        s.hibernated = true;

        let json = serde_json::to_string(&s).unwrap();
        let roundtripped: WorktreeSession = serde_json::from_str(&json).unwrap();
        assert!(roundtripped.keep_alive);
        assert!(roundtripped.hibernated);
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

    // `ProjectId`/`SessionId` `from_uuid`/`as_uuid` round-trip tests (which need
    // access to the private inner field) live in `claude-commander-protocol`,
    // where the types are now defined.

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
    fn is_descendant_of_walks_the_whole_chain() {
        let a = session_with("a", None, None);
        let b = session_with("b", None, Some(a.id));
        let c = session_with("c", None, Some(b.id));
        let sessions = [&a, &b, &c];

        assert!(is_descendant_of(c.id, a.id, &sessions), "grandchild of A");
        assert!(is_descendant_of(b.id, a.id, &sessions));
        // Not symmetric, and identity is not descent.
        assert!(!is_descendant_of(a.id, c.id, &sessions));
        assert!(!is_descendant_of(a.id, a.id, &sessions));
    }

    #[test]
    fn is_descendant_of_is_false_for_an_unrelated_session() {
        let a = session_with("a", None, None);
        let b = session_with("b", None, Some(a.id));
        let solo = session_with("solo", None, None);
        let sessions = [&a, &b, &solo];

        assert!(!is_descendant_of(b.id, solo.id, &sessions));
        assert!(!is_descendant_of(solo.id, a.id, &sessions));
    }

    /// An already-corrupted cycle must terminate rather than spin — the walk is
    /// bounded by the session count, exactly like `stack_root`.
    #[test]
    fn is_descendant_of_terminates_on_a_corrupted_cycle() {
        let mut a = session_with("a", None, None);
        let mut b = session_with("b", None, None);
        a.stack_parent_session_id = Some(b.id);
        b.stack_parent_session_id = Some(a.id);
        let solo = session_with("solo", None, None);
        let sessions = [&a, &b, &solo];

        // Reachable within the cycle, and the unreachable query still returns.
        assert!(is_descendant_of(a.id, b.id, &sessions));
        assert!(!is_descendant_of(a.id, solo.id, &sessions));
    }
}

/// The stack-resolution helpers are generic over [`SessionNode`]; the local
/// [`WorktreeSession`] path is covered by the tests above. These prove the same
/// algorithms drive the wire [`SessionInfo`] DTO identically, so a remote
/// client builds the same session tree from the network — the whole point of
/// making the helpers generic in Phase C.
#[cfg(test)]
mod session_info_stack_node_tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};
    use claude_commander_protocol::api::SessionInfo;

    /// Minimal `SessionInfo` with a fixed project, so only the stack-relevant
    /// fields (branch / pr_base_branch / stack_parent / created_at) vary.
    fn info(branch: &str, created_offset_secs: i64) -> SessionInfo {
        let session_id = SessionId::new();
        SessionInfo {
            id: session_id.as_uuid().to_string(),
            session_id,
            title: branch.to_string(),
            branch: branch.to_string(),
            status: SessionStatus::Running,
            program: "claude".to_string(),
            pending_program: None,
            project_id: ProjectId::new(),
            project_name: "p".to_string(),
            pr_number: None,
            pr_url: None,
            pr_state: crate::git::PrState::Open,
            pr_draft: false,
            pr_labels: Vec::new(),
            review_decision: None,
            pr_reviewers: Vec::new(),
            created_at: Utc::now() + ChronoDuration::seconds(created_offset_secs),
            unread: false,
            stack_parent_session_id: None,
            pr_base_branch: None,
            pr_merged: false,
            current_section: None,
            section_override: None,
            entered_section_at: None,
            last_attached_at: None,
            keep_alive: false,
            worktree_path: String::new(),
            tmux_session_name: String::new(),
        }
    }

    #[test]
    fn resolve_stack_parent_matches_pr_base_branch_on_session_info() {
        let base = info("base-br", 0);
        let mut child = info("child-br", 10);
        child.pr_base_branch = Some("base-br".to_string());
        let sessions = [&base, &child];
        assert_eq!(
            resolve_stack_parent(&child, &sessions),
            Some(base.session_id),
            "SessionInfo PR base should resolve to the matching session, as it does for WorktreeSession"
        );
        assert_eq!(
            resolve_stack_parent(&base, &sessions),
            None,
            "the base itself is unstacked"
        );
    }

    #[test]
    fn resolve_stack_parent_falls_back_to_local_link_on_session_info() {
        let base = info("base-br", 0);
        let mut child = info("child-br", 10);
        child.stack_parent_session_id = Some(base.session_id);
        let sessions = [&base, &child];
        assert_eq!(
            resolve_stack_parent(&child, &sessions),
            Some(base.session_id)
        );
    }

    #[test]
    fn stack_root_and_chain_walk_session_info_dtos() {
        // base <- mid <- leaf via local links.
        let base = info("base", 0);
        let mut mid = info("mid", 10);
        mid.stack_parent_session_id = Some(base.session_id);
        let mut leaf = info("leaf", 20);
        leaf.stack_parent_session_id = Some(mid.session_id);
        let sessions = [&base, &mid, &leaf];

        assert_eq!(stack_root(leaf.session_id, &sessions), base.session_id);
        assert_eq!(stack_top(base.session_id, &sessions), leaf.session_id);
        assert_eq!(
            stack_chain_from_base(base.session_id, &sessions),
            vec![base.session_id, mid.session_id, leaf.session_id]
        );
    }
}
