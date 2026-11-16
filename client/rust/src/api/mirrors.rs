//! flutter_rust_bridge mirrors of the shared `claude-commander-protocol` wire
//! types.
//!
//! A mirror tells frb the shape of a type defined in another crate so it can
//! generate the matching Dart class. The mirror MUST match the real type
//! field-for-field — frb emits a **compile error** otherwise — so these can't
//! silently drift from the protocol crate. The real types are `pub use`d here
//! both so `mirror(...)` resolves to them and so the API functions can return
//! them directly.
//!
//! Mirrors are added incrementally, per phase, as the UI needs each type.
//! Phase 1 needs the session-list shape (`SessionInfo` + the ids/enums it
//! embeds).

use chrono::{DateTime, Utc};
use flutter_rust_bridge::frb;
use uuid::Uuid;

pub use claude_commander_protocol::api::{
    BranchInfo, CodeHostStatus, CreateOptions, OperationKind, ProgramInfo, PullBlockReason,
    ServerStatus, SessionDetail, SessionInfo,
};
pub use claude_commander_protocol::github::{CloneJobId, GithubRepo};
pub use claude_commander_protocol::pr::{PrState, ReviewDecision};
pub use claude_commander_protocol::session::{AgentState, ProjectId, SessionId, SessionStatus};
pub use claude_commander_protocol::workspace::WorkspaceDef;
pub use claude_commander_protocol::ws::AttachKind;

use claude_commander_protocol::api::{
    AgentStatesSnapshot, DiffStat, OperationOutcome, OperationStatus, PreviewData, ProjectInfo,
    PullStatus, Snapshot,
};
use claude_commander_protocol::connection::ConnectionState;
use claude_commander_protocol::github::{CloneJob, CloneRequest, CloneStatus};
use claude_commander_protocol::hosting::CloneSource;
pub use claude_commander_protocol::hosting::{
    CodeHost, CodeHostProvider, HostedRepository, RepositoryListing, RepositoryVisibility,
};
use claude_commander_protocol::session::COMMANDER_SENTINEL_ID;
use claude_commander_protocol::workspace::SetWorkspacesRequest;

// All three id newtypes are a single `Uuid`, so one mirror covers them all.
#[frb(mirror(SessionId, ProjectId, CloneJobId))]
pub struct _Id(pub Uuid);

#[frb(mirror(SessionStatus))]
pub enum _SessionStatus {
    Creating,
    Running,
    Stopped,
    Merging,
    CascadePaused,
    Pushing,
}

#[frb(mirror(PrState))]
pub enum _PrState {
    Open,
    Closed,
    Merged,
}

#[frb(mirror(ReviewDecision))]
pub enum _ReviewDecision {
    ReviewRequired,
    Approved,
    ChangesRequested,
}

#[frb(mirror(SessionInfo))]
pub struct _SessionInfo {
    pub id: String,
    pub session_id: SessionId,
    pub title: String,
    pub branch: String,
    pub status: SessionStatus,
    pub program: String,
    pub pending_program: Option<String>,
    pub project_id: ProjectId,
    pub project_name: String,
    pub pr_number: Option<u32>,
    pub pr_url: Option<String>,
    pub pr_state: PrState,
    pub pr_draft: bool,
    pub pr_labels: Vec<String>,
    pub review_decision: Option<ReviewDecision>,
    pub pr_reviewers: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub unread: bool,
    pub stack_parent_session_id: Option<SessionId>,
    pub pr_base_branch: Option<String>,
    pub pr_merged: bool,
    pub current_section: Option<String>,
    pub section_override: Option<String>,
    pub entered_section_at: Option<DateTime<Utc>>,
    pub last_attached_at: Option<DateTime<Utc>>,
    pub worktree_path: String,
    pub tmux_session_name: String,
    pub keep_alive: bool,
}

// Phase 2 needs the detail shape: the session's live agent sub-state plus the
// diff summary and a pane snapshot. The mirror matches the Rust struct
// field-for-field (frb checks the struct, not the flattened JSON).
#[frb(mirror(AgentState))]
pub enum _AgentState {
    Working,
    Idle,
    WaitingForInput,
    Unknown,
}

#[frb(mirror(SessionDetail))]
pub struct _SessionDetail {
    pub info: SessionInfo,
    pub agent_state: AgentState,
    pub diff_stat: Option<String>,
    pub pane_content: Option<String>,
}

// ---------------------------------------------------------------------------
// Phase 2: the full server surface reaches Dart. Types with only plain fields
// (or unit enums) are mirrored directly, so the API fns return the protocol
// type unchanged. Types carrying data enums, tuples, maps, `PathBuf`, or `usize`
// are converted to plain-struct / unit-enum DTOs first (frb can't render those
// without the build_runner/freezed step — same rule as `api::review`).
// ---------------------------------------------------------------------------

// -- Direct mirrors (plain fields / unit enums) --

#[frb(mirror(ServerStatus))]
pub struct _ServerStatus {
    pub gh_available: bool,
    pub code_host: CodeHostStatus,
    pub tmux_ok: bool,
    pub version: String,
}

#[frb(mirror(CodeHostStatus))]
pub struct _CodeHostStatus {
    pub provider: CodeHostProvider,
    pub hostname: Option<String>,
    pub cli_available: bool,
}

#[frb(mirror(CodeHostProvider))]
pub enum _CodeHostProvider {
    Github,
    Gitlab,
}

/// A launch program option. Mirrored (not a DTO) so it can also be *constructed*
/// on the Dart side and passed straight back to `set_programs`.
#[frb(mirror(ProgramInfo))]
pub struct _ProgramInfo {
    pub label: String,
    pub command: String,
}

#[frb(mirror(CreateOptions))]
pub struct _CreateOptions {
    pub default_program: String,
    pub programs: Vec<ProgramInfo>,
    pub sections: Vec<String>,
}

#[frb(mirror(BranchInfo))]
pub struct _BranchInfo {
    pub name: String,
    pub is_remote: bool,
}

/// One user-defined workspace (`[[workspaces]]`). Mirrored rather than wrapped
/// so the Workspaces settings page can construct the list it sends back to
/// [`crate::api::simple::set_workspaces`] directly.
#[frb(mirror(WorkspaceDef))]
pub struct _WorkspaceDef {
    pub name: String,
}

#[frb(mirror(OperationKind))]
pub enum _OperationKind {
    Cascade,
    PushStack,
}

/// Which pane of a session to attach to (agent vs the paired shell).
#[frb(mirror(AttachKind))]
pub enum _AttachKind {
    Agent,
    Shell,
}

#[frb(mirror(PullBlockReason))]
pub enum _PullBlockReason {
    Dirty,
    Diverged,
    WorktreeConflict,
}

// -- DTOs (converted from types frb can't render directly) --

/// A project (git repo). `repo_path` is flattened `PathBuf` → `String` (matching
/// `SessionInfo.worktree_path`), which frb transfers cleanly.
pub struct ProjectInfoDto {
    pub id: ProjectId,
    pub name: String,
    pub repo_path: String,
    pub main_branch: String,
    pub session_ids: Vec<SessionId>,
    /// The repo's `origin` remote URL, or `None` when it has none (or when the
    /// server predates the field). The repo picker's "already added" badge runs
    /// this and a candidate's clone URL through
    /// [`crate::api::simple::canonical_repo_slug`] and compares the results —
    /// never the raw strings, since one repo has several spellings.
    pub origin_url: Option<String>,
    /// The workspace this project is tagged with, or `None` for the built-in
    /// Main workspace (and for a server that predates workspaces).
    pub workspace: Option<String>,
}

impl From<ProjectInfo> for ProjectInfoDto {
    fn from(p: ProjectInfo) -> Self {
        Self {
            id: p.id,
            name: p.name,
            repo_path: p.repo_path.to_string_lossy().into_owned(),
            main_branch: p.main_branch,
            session_ids: p.session_ids,
            origin_url: p.origin_url,
            workspace: p.workspace,
        }
    }
}

/// Which kind of [`OperationOutcomeDto`] this is (flattens the data-carrying
/// [`OperationOutcome`]). `Paused` only occurs for a cascade.
pub enum OperationOutcomeKind {
    Succeeded,
    Paused,
    Failed,
}

/// Terminal result of a stack operation. `detail` is the human summary
/// (`Succeeded`/`Paused`) or the error message (`Failed`).
pub struct OperationOutcomeDto {
    pub kind: OperationOutcomeKind,
    pub detail: String,
}

impl From<OperationOutcome> for OperationOutcomeDto {
    fn from(o: OperationOutcome) -> Self {
        match o {
            OperationOutcome::Succeeded { detail } => Self {
                kind: OperationOutcomeKind::Succeeded,
                detail,
            },
            OperationOutcome::Paused { detail } => Self {
                kind: OperationOutcomeKind::Paused,
                detail,
            },
            OperationOutcome::Failed { error } => Self {
                kind: OperationOutcomeKind::Failed,
                detail: error,
            },
        }
    }
}

/// One recent cascade / push-stack operation. `id` is monotonic per process.
pub struct OperationStatusDto {
    pub id: u64,
    pub kind: OperationKind,
    pub outcome: OperationOutcomeDto,
    pub finished_at: Option<DateTime<Utc>>,
}

impl From<OperationStatus> for OperationStatusDto {
    fn from(o: OperationStatus) -> Self {
        Self {
            id: o.id,
            kind: o.kind,
            outcome: o.outcome.into(),
            finished_at: o.finished_at,
        }
    }
}

/// Which kind of [`PullStatusDto`] this is (flattens the data-carrying
/// [`PullStatus`]); `blocked_reason` is populated only for `Blocked`.
pub enum PullStatusKind {
    Advanced,
    UpToDate,
    Blocked,
    SoftFail,
}

/// Outcome of a project's most recent background pull.
pub struct PullStatusDto {
    pub kind: PullStatusKind,
    pub blocked_reason: Option<PullBlockReason>,
}

impl From<PullStatus> for PullStatusDto {
    fn from(s: PullStatus) -> Self {
        match s {
            PullStatus::Advanced => Self {
                kind: PullStatusKind::Advanced,
                blocked_reason: None,
            },
            PullStatus::UpToDate => Self {
                kind: PullStatusKind::UpToDate,
                blocked_reason: None,
            },
            PullStatus::Blocked { reason } => Self {
                kind: PullStatusKind::Blocked,
                blocked_reason: Some(reason),
            },
            PullStatus::SoftFail => Self {
                kind: PullStatusKind::SoftFail,
                blocked_reason: None,
            },
        }
    }
}

/// One project's pull status — the flattened form of the snapshot's
/// `BTreeMap<ProjectId, PullStatus>` (frb doesn't render maps).
pub struct ProjectPullDto {
    pub project_id: ProjectId,
    pub status: PullStatusDto,
}

/// A single snapshot of everything the session tree renders. The `BTreeMap`
/// pull statuses are flattened to a `Vec`; every data enum is flattened above.
pub struct SnapshotDto {
    pub projects: Vec<ProjectInfoDto>,
    pub sessions: Vec<SessionInfo>,
    pub cascade_paused: Option<SessionId>,
    pub pending_comment_sessions: Vec<SessionId>,
    pub project_pull: Vec<ProjectPullDto>,
    pub operations: Vec<OperationStatusDto>,
    pub server: ServerStatus,
    /// This server's workspace definitions, in display order. Main is never
    /// among them — it is the untagged default (see [`Self::main_workspace`]).
    pub workspaces: Vec<WorkspaceDef>,
    /// Main's display label and colour, when this server has renamed it.
    pub main_workspace: Option<WorkspaceDef>,
    /// `startup_workspace` in its wire/TOML string form: `"last"`, `"main"`, or
    /// a workspace name. A string rather than a Dart enum because the protocol
    /// type carries data in its `Named` arm, and the two keywords are reserved
    /// names, so the string is unambiguous (`protocol::workspace`).
    pub startup_workspace: String,
}

impl From<Snapshot> for SnapshotDto {
    fn from(s: Snapshot) -> Self {
        Self {
            projects: s.projects.into_iter().map(Into::into).collect(),
            sessions: s.sessions,
            cascade_paused: s.cascade_paused,
            pending_comment_sessions: s.pending_comment_sessions,
            project_pull: s
                .project_pull
                .into_iter()
                .map(|(project_id, status)| ProjectPullDto {
                    project_id,
                    status: status.into(),
                })
                .collect(),
            operations: s.operations.into_iter().map(Into::into).collect(),
            server: s.server,
            workspaces: s.workspaces,
            main_workspace: s.main_workspace,
            startup_workspace: s.startup_workspace.into(),
        }
    }
}

/// One session's agent state (flattened from the snapshot's
/// `BTreeMap<SessionId, AgentState>`). The commander *sentinel* id is never
/// present here — [`AgentStatesSnapshotDto::from`] drops it.
pub struct AgentStateEntryDto {
    pub session_id: SessionId,
    pub state: AgentState,
}

/// Bulk agent-state snapshot, map flattened to a `Vec` with the synthetic
/// commander sentinel entry stripped (its running-ness is `commander_running`).
pub struct AgentStatesSnapshotDto {
    pub states: Vec<AgentStateEntryDto>,
    pub commander_running: bool,
}

impl From<AgentStatesSnapshot> for AgentStatesSnapshotDto {
    fn from(s: AgentStatesSnapshot) -> Self {
        Self {
            states: s
                .states
                .into_iter()
                .filter(|(id, _)| *id != COMMANDER_SENTINEL_ID)
                .map(|(session_id, state)| AgentStateEntryDto { session_id, state })
                .collect(),
            commander_running: s.commander_running,
        }
    }
}

/// Numeric diff counts. `usize` → `u32` (matching `api::review`'s line numbers).
pub struct DiffStatDto {
    pub files_changed: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
}

impl From<DiffStat> for DiffStatDto {
    fn from(s: DiffStat) -> Self {
        Self {
            files_changed: s.files_changed as u32,
            lines_added: s.lines_added as u32,
            lines_removed: s.lines_removed as u32,
        }
    }
}

/// Preview payload for a session or project: pane/shell snapshots plus the diff
/// text, its one-line stat, and structured counts.
pub struct PreviewDataDto {
    pub pane: Option<String>,
    pub diff_text: String,
    pub diff_stat: Option<String>,
    pub stats: Option<DiffStatDto>,
    pub shell: Option<String>,
}

impl From<PreviewData> for PreviewDataDto {
    fn from(p: PreviewData) -> Self {
        Self {
            pane: p.pane,
            diff_text: p.diff_text,
            diff_stat: p.diff_stat,
            stats: p.stats.map(Into::into),
            shell: p.shell,
        }
    }
}

/// Which kind of [`ConnectionStateDto`] this is (flattens the data-carrying
/// [`ConnectionState`]); `reason` is populated only for `Degraded`.
pub enum ConnectionStateKind {
    Connecting,
    Connected,
    Degraded,
}

/// A backend's connection health, streamed over `connection_feed`.
pub struct ConnectionStateDto {
    pub kind: ConnectionStateKind,
    /// Short user-facing note (`Degraded` only); empty otherwise.
    pub reason: String,
}

impl From<ConnectionState> for ConnectionStateDto {
    fn from(s: ConnectionState) -> Self {
        match s {
            ConnectionState::Connecting => Self {
                kind: ConnectionStateKind::Connecting,
                reason: String::new(),
            },
            ConnectionState::Connected => Self {
                kind: ConnectionStateKind::Connected,
                reason: String::new(),
            },
            ConnectionState::Degraded { reason } => Self {
                kind: ConnectionStateKind::Degraded,
                reason,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// GitHub repo picker + repository clone (`claude_commander_protocol::github`).
//
// [`GithubRepo`] is all plain fields, so it is a direct mirror the picker can
// render unchanged. The clone types are not: [`CloneSource`] and [`CloneStatus`]
// carry data in their variants and [`CloneJob`] carries a `PathBuf`, so they get
// the same flattened-DTO treatment as `OperationOutcome`/`PullStatus` above.
// ---------------------------------------------------------------------------

/// One repo offered by the picker. Compare `clone_url` against a project's
/// `origin_url` via [`crate::api::simple::canonical_repo_slug`] to tell whether
/// it is already registered — `gh repo clone` honours the user's `git_protocol`,
/// so an added repo's origin is often `ssh://` where this reports `https://`.
#[frb(mirror(GithubRepo))]
pub struct _GithubRepo {
    pub full_name: String,
    pub owner: String,
    pub name: String,
    pub description: Option<String>,
    pub private: bool,
    pub fork: bool,
    pub archived: bool,
    pub default_branch: String,
    pub clone_url: String,
    pub ssh_url: String,
    pub pushed_at: Option<DateTime<Utc>>,
}

#[frb(mirror(CodeHost))]
pub struct _CodeHost {
    pub provider: CodeHostProvider,
    pub hostname: Option<String>,
}

#[frb(mirror(RepositoryVisibility))]
pub enum _RepositoryVisibility {
    Public,
    Internal,
    Private,
}

#[frb(mirror(HostedRepository))]
pub struct _HostedRepository {
    pub full_name: String,
    pub namespace: String,
    pub name: String,
    pub description: Option<String>,
    pub visibility: RepositoryVisibility,
    pub fork: bool,
    pub archived: bool,
    pub default_branch: Option<String>,
    pub clone_url: String,
    pub ssh_url: String,
    pub activity_at: Option<DateTime<Utc>>,
}

#[frb(mirror(RepositoryListing))]
pub struct _RepositoryListing {
    pub host: CodeHost,
    pub repositories: Vec<HostedRepository>,
}

/// Which kind of [`CloneSourceDto`] this is (flattens the data-carrying
/// [`CloneSource`]).
pub enum CloneSourceKind {
    Github,
    Gitlab,
    Url,
}

/// Where a clone should come from — the Dart-constructible form of
/// [`CloneSource`]. `value` is the `owner/name` slug for
/// [`CloneSourceKind::Github`] and the clone URL for [`CloneSourceKind::Url`].
///
/// The two arms stay distinct because they are different *invocations* server
/// side (`gh repo clone` vs `git clone`), not two spellings of one.
pub struct CloneSourceDto {
    pub kind: CloneSourceKind,
    pub value: String,
    pub hostname: Option<String>,
}

impl From<CloneSourceDto> for CloneSource {
    fn from(s: CloneSourceDto) -> Self {
        match s.kind {
            CloneSourceKind::Github => CloneSource::Github { full_name: s.value },
            CloneSourceKind::Gitlab => CloneSource::Gitlab {
                full_name: s.value,
                hostname: s.hostname,
            },
            CloneSourceKind::Url => CloneSource::Url { url: s.value },
        }
    }
}

/// Request body for [`crate::api::simple::start_clone`] — the Dart-constructible
/// form of [`CloneRequest`]. `dest_name: None` means "derive the directory name
/// from the source".
pub struct CloneRequestDto {
    pub source: CloneSourceDto,
    pub dest_name: Option<String>,
    /// The workspace to tag the cloned project with once it is registered —
    /// the app's active one. `None` lands it in Main.
    pub workspace: Option<String>,
}

impl From<CloneRequestDto> for CloneRequest {
    fn from(r: CloneRequestDto) -> Self {
        Self {
            source: r.source.into(),
            dest_name: r.dest_name,
            workspace: r.workspace,
        }
    }
}

/// Body for [`crate::api::simple::set_workspaces`] — the Dart-constructible
/// form of [`SetWorkspacesRequest`]. `startup_workspace` travels in its string
/// form (see [`SnapshotDto::startup_workspace`]); `None` for it or for `main`
/// leaves the server's current value untouched.
pub struct SetWorkspacesRequestDto {
    pub workspaces: Vec<WorkspaceDef>,
    pub main: Option<WorkspaceDef>,
    pub startup_workspace: Option<String>,
}

impl From<SetWorkspacesRequestDto> for SetWorkspacesRequest {
    fn from(r: SetWorkspacesRequestDto) -> Self {
        Self {
            workspaces: r.workspaces,
            main: r.main,
            startup_workspace: r.startup_workspace.map(Into::into),
        }
    }
}

/// Which kind of [`CloneStatusDto`] this is (flattens the data-carrying
/// [`CloneStatus`]).
///
/// `DestinationExists` is its own arm rather than a `Failed` message because it
/// is the one outcome a frontend can act on: whether the occupied directory is
/// already a git repo decides whether the sensible offer is "add that checkout as
/// a project" or "pick another name".
pub enum CloneStatusKind {
    Running,
    Succeeded,
    Failed,
    DestinationExists,
}

/// How a clone is going. Only the fields belonging to `kind` are populated.
pub struct CloneStatusDto {
    pub kind: CloneStatusKind,
    /// The registered project (`Succeeded` only).
    pub project_id: Option<ProjectId>,
    /// User-facing reason (`Failed` only); empty otherwise. Already redacted
    /// where it was built, so it never carries `user:token@` userinfo.
    pub message: String,
    /// The occupied destination path reported by `DestinationExists`; `None`
    /// otherwise.
    pub dest: Option<String>,
    /// Whether that occupied path is itself a git repo (`DestinationExists`
    /// only); `false` otherwise.
    pub is_git_repo: bool,
}

impl From<CloneStatus> for CloneStatusDto {
    fn from(s: CloneStatus) -> Self {
        // Start from the "no payload" shape so each arm only names what it has.
        let base = Self {
            kind: CloneStatusKind::Running,
            project_id: None,
            message: String::new(),
            dest: None,
            is_git_repo: false,
        };
        match s {
            CloneStatus::Running => base,
            CloneStatus::Succeeded { project_id } => Self {
                kind: CloneStatusKind::Succeeded,
                project_id: Some(project_id),
                ..base
            },
            CloneStatus::Failed { message } => Self {
                kind: CloneStatusKind::Failed,
                message,
                ..base
            },
            CloneStatus::DestinationExists { dest, is_git_repo } => Self {
                kind: CloneStatusKind::DestinationExists,
                dest: Some(dest.to_string_lossy().into_owned()),
                is_git_repo,
                ..base
            },
        }
    }
}

/// A clone in flight, as polled by a frontend. `dest` is flattened `PathBuf` →
/// `String` (as `ProjectInfoDto::repo_path` is).
///
/// The status carried by the job [`crate::api::simple::start_clone`] returns is
/// **not** terminal — every outcome is reported through
/// [`crate::api::simple::clone_job`].
pub struct CloneJobDto {
    pub id: CloneJobId,
    /// What to show the user as the source — the `owner/name` slug or the URL.
    pub source_label: String,
    /// Absolute destination path the clone is writing to.
    pub dest: String,
    pub status: CloneStatusDto,
}

impl From<CloneJob> for CloneJobDto {
    fn from(j: CloneJob) -> Self {
        Self {
            id: j.id,
            source_label: j.source_label,
            dest: j.dest.to_string_lossy().into_owned(),
            status: j.status.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn agent_states_dto_drops_commander_sentinel() {
        let real = SessionId::new();
        let mut states = BTreeMap::new();
        states.insert(real, AgentState::Working);
        states.insert(COMMANDER_SENTINEL_ID, AgentState::Idle);
        let dto: AgentStatesSnapshotDto = AgentStatesSnapshot {
            states,
            commander_running: true,
        }
        .into();
        assert_eq!(dto.states.len(), 1, "sentinel must be filtered out");
        assert_eq!(dto.states[0].session_id, real);
        assert!(dto.commander_running);
    }

    #[test]
    fn snapshot_dto_flattens_pull_map_and_operations() {
        let pid = ProjectId::new();
        let mut project_pull = BTreeMap::new();
        project_pull.insert(
            pid,
            PullStatus::Blocked {
                reason: PullBlockReason::Dirty,
            },
        );
        let snap = Snapshot {
            projects: vec![ProjectInfo {
                id: pid,
                name: "repo".into(),
                repo_path: PathBuf::from("/repo"),
                main_branch: "main".into(),
                session_ids: vec![],
                origin_url: None,
                workspace: None,
            }],
            sessions: vec![],
            cascade_paused: None,
            pending_comment_sessions: vec![],
            project_pull,
            operations: vec![OperationStatus {
                id: 3,
                kind: OperationKind::Cascade,
                outcome: OperationOutcome::Paused {
                    detail: "2 merged".into(),
                },
                finished_at: None,
            }],
            server: ServerStatus {
                gh_available: true,
                code_host: Default::default(),
                tmux_ok: true,
                version: "0.0.0".into(),
            },
            workspaces: Vec::new(),
            main_workspace: None,
            startup_workspace: Default::default(),
        };
        let dto: SnapshotDto = snap.into();
        assert_eq!(dto.projects[0].repo_path, "/repo");
        assert_eq!(dto.project_pull.len(), 1);
        assert_eq!(dto.project_pull[0].project_id, pid);
        assert!(matches!(
            dto.project_pull[0].status.kind,
            PullStatusKind::Blocked
        ));
        assert!(matches!(
            dto.project_pull[0].status.blocked_reason,
            Some(PullBlockReason::Dirty)
        ));
        assert_eq!(dto.operations[0].id, 3);
        assert!(matches!(
            dto.operations[0].outcome.kind,
            OperationOutcomeKind::Paused
        ));
        assert_eq!(dto.operations[0].outcome.detail, "2 merged");
    }

    /// The workspace fields are named explicitly in `From`, like every other
    /// field, so a dropped one would compile — hence a test per field.
    #[test]
    fn snapshot_dto_carries_the_workspace_config_and_project_tags() {
        use claude_commander_protocol::workspace::StartupWorkspace;
        let snap = Snapshot {
            projects: vec![ProjectInfo {
                id: ProjectId::new(),
                name: "repo".into(),
                repo_path: PathBuf::from("/repo"),
                main_branch: "main".into(),
                session_ids: vec![],
                origin_url: None,
                workspace: Some("Work".into()),
            }],
            sessions: vec![],
            cascade_paused: None,
            pending_comment_sessions: vec![],
            project_pull: BTreeMap::new(),
            operations: vec![],
            server: ServerStatus {
                gh_available: true,
                tmux_ok: true,
                code_host: Default::default(),
                version: "0.0.0".into(),
            },
            workspaces: vec![WorkspaceDef::named("Work")],
            main_workspace: Some(WorkspaceDef::named("Home")),
            startup_workspace: StartupWorkspace::Named("Work".into()),
        };
        let dto: SnapshotDto = snap.into();
        assert_eq!(dto.projects[0].workspace.as_deref(), Some("Work"));
        assert_eq!(dto.workspaces.len(), 1);
        assert_eq!(dto.main_workspace.map(|m| m.name).as_deref(), Some("Home"));
        assert_eq!(dto.startup_workspace, "Work");
    }

    /// The startup choice crosses the bridge as its wire string, and the two
    /// keywords must come back as the keyword arms rather than as names.
    #[test]
    fn set_workspaces_dto_parses_the_startup_string() {
        use claude_commander_protocol::workspace::StartupWorkspace;
        let req = |s: Option<&str>| -> SetWorkspacesRequest {
            SetWorkspacesRequestDto {
                workspaces: vec![WorkspaceDef::named("Work")],
                main: None,
                startup_workspace: s.map(str::to_string),
            }
            .into()
        };
        assert_eq!(req(None).startup_workspace, None);
        assert_eq!(
            req(Some("last")).startup_workspace,
            Some(StartupWorkspace::Last)
        );
        assert_eq!(
            req(Some("main")).startup_workspace,
            Some(StartupWorkspace::Main)
        );
        assert_eq!(
            req(Some("Work")).startup_workspace,
            Some(StartupWorkspace::Named("Work".into()))
        );
        assert_eq!(req(None).workspaces, vec![WorkspaceDef::named("Work")]);
    }

    /// A clone started from the app lands in its active workspace, so the DTO
    /// must hand the tag through rather than defaulting it.
    #[test]
    fn clone_request_dto_carries_the_workspace() {
        let req: CloneRequest = CloneRequestDto {
            source: CloneSourceDto {
                kind: CloneSourceKind::Github,
                value: "acme/widget".into(),
                hostname: None,
            },
            dest_name: None,
            workspace: Some("Work".into()),
        }
        .into();
        assert_eq!(req.workspace.as_deref(), Some("Work"));
    }

    /// The repo picker's "already added" badge reads `origin_url` off the
    /// snapshot's projects, so the flattening must carry it. `ProjectInfo` names
    /// its fields explicitly in `From`, which is why a missing field here
    /// compiles rather than failing — hence this test.
    #[test]
    fn project_dto_carries_the_origin_url() {
        let with_origin: ProjectInfoDto = ProjectInfo {
            id: ProjectId::new(),
            name: "repo".into(),
            repo_path: PathBuf::from("/repo"),
            main_branch: "main".into(),
            session_ids: vec![],
            origin_url: Some("git@github.com:sizeak/claude-commander.git".into()),
            workspace: None,
        }
        .into();
        assert_eq!(
            with_origin.origin_url.as_deref(),
            Some("git@github.com:sizeak/claude-commander.git")
        );

        // A repo with no remote (or an older server) stays `None` rather than
        // becoming an empty string the badge would have to special-case.
        let without: ProjectInfoDto = ProjectInfo {
            id: ProjectId::new(),
            name: "local".into(),
            repo_path: PathBuf::from("/local"),
            main_branch: "main".into(),
            session_ids: vec![],
            origin_url: None,
            workspace: None,
        }
        .into();
        assert_eq!(without.origin_url, None);
    }

    /// Every arm of the clone status, since the flattening is the only place the
    /// per-arm payloads can go missing.
    #[test]
    fn clone_status_dto_flattens_every_arm() {
        let running: CloneStatusDto = CloneStatus::Running.into();
        assert!(matches!(running.kind, CloneStatusKind::Running));
        assert_eq!(running.project_id, None);
        assert!(running.message.is_empty());
        assert_eq!(running.dest, None);
        assert!(!running.is_git_repo);

        let pid = ProjectId::new();
        let ok: CloneStatusDto = CloneStatus::Succeeded { project_id: pid }.into();
        assert!(matches!(ok.kind, CloneStatusKind::Succeeded));
        assert_eq!(ok.project_id, Some(pid));

        let failed: CloneStatusDto = CloneStatus::Failed {
            message: "fatal: repository not found".into(),
        }
        .into();
        assert!(matches!(failed.kind, CloneStatusKind::Failed));
        assert_eq!(failed.message, "fatal: repository not found");
        assert_eq!(failed.project_id, None);

        let occupied: CloneStatusDto = CloneStatus::DestinationExists {
            dest: PathBuf::from("/projects/repo"),
            is_git_repo: true,
        }
        .into();
        assert!(matches!(occupied.kind, CloneStatusKind::DestinationExists));
        assert_eq!(occupied.dest.as_deref(), Some("/projects/repo"));
        assert!(
            occupied.is_git_repo,
            "is_git_repo decides which recovery the UI offers, so it must survive"
        );
    }

    /// The job flattening: `PathBuf` → `String`, id and label untouched, and the
    /// nested status flattened rather than dropped.
    #[test]
    fn clone_job_dto_flattens_dest_and_status() {
        let id = CloneJobId::new();
        let pid = ProjectId::new();
        let dto: CloneJobDto = CloneJob {
            id,
            source_label: "sizeak/claude-commander".into(),
            dest: PathBuf::from("/projects/claude-commander"),
            status: CloneStatus::Succeeded { project_id: pid },
        }
        .into();
        assert_eq!(dto.id, id);
        assert_eq!(dto.source_label, "sizeak/claude-commander");
        assert_eq!(dto.dest, "/projects/claude-commander");
        assert!(matches!(dto.status.kind, CloneStatusKind::Succeeded));
        assert_eq!(dto.status.project_id, Some(pid));
    }

    /// The request direction: Dart builds the DTO, so the mapping onto the wire
    /// enum is what decides whether the server runs `gh repo clone` or `git
    /// clone`. Getting the arms crossed would send a slug to `git clone`.
    #[test]
    fn clone_request_dto_maps_onto_the_wire_source() {
        let github: CloneRequest = CloneRequestDto {
            source: CloneSourceDto {
                kind: CloneSourceKind::Github,
                value: "sizeak/claude-commander".into(),
                hostname: None,
            },
            dest_name: None,
            workspace: None,
        }
        .into();
        assert_eq!(
            github.source,
            CloneSource::Github {
                full_name: "sizeak/claude-commander".into()
            }
        );
        assert_eq!(github.dest_name, None);

        let gitlab: CloneRequest = CloneRequestDto {
            source: CloneSourceDto {
                kind: CloneSourceKind::Gitlab,
                value: "group/sub/project".into(),
                hostname: Some("gitlab.example.com".into()),
            },
            dest_name: None,
            workspace: None,
        }
        .into();
        assert_eq!(
            gitlab.source,
            CloneSource::Gitlab {
                full_name: "group/sub/project".into(),
                hostname: Some("gitlab.example.com".into()),
            }
        );

        let url: CloneRequest = CloneRequestDto {
            source: CloneSourceDto {
                kind: CloneSourceKind::Url,
                value: "https://github.com/sizeak/claude-commander.git".into(),
                hostname: None,
            },
            dest_name: Some("cc".into()),
            workspace: None,
        }
        .into();
        assert_eq!(
            url.source,
            CloneSource::Url {
                url: "https://github.com/sizeak/claude-commander.git".into()
            }
        );
        assert_eq!(url.dest_name.as_deref(), Some("cc"));
    }

    #[test]
    fn connection_state_dto_flattens_degraded_reason() {
        let dto: ConnectionStateDto = ConnectionState::Degraded {
            reason: "boom".into(),
        }
        .into();
        assert!(matches!(dto.kind, ConnectionStateKind::Degraded));
        assert_eq!(dto.reason, "boom");

        let ok: ConnectionStateDto = ConnectionState::Connected.into();
        assert!(matches!(ok.kind, ConnectionStateKind::Connected));
        assert!(ok.reason.is_empty());
    }
}
