//! HTTP API request/response DTOs.
//!
//! These mirror the JSON bodies the server's `/api` surface accepts and returns.
//! They embed the lower-level wire types from this crate's other modules
//! (`session`, `pr`, `diff`, `comment`), so a client deserializes a whole
//! session/review payload with no hand-maintained mirror.
//!
//! Construction helpers that need the server's domain model (e.g. building a
//! [`SessionInfo`] from a `WorktreeSession`, or validating program flags) live
//! in `claude-commander-core`, since they depend on types that can't cross the
//! network. These are plain data.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::comment::{Comment, CommentSide};
use crate::diff::ParsedDiff;
use crate::pr::{PrState, ReviewDecision};
use crate::session::{AgentState, ProjectId, SessionId, SessionStatus};
use crate::workspace::{StartupWorkspace, WorkspaceDef};

/// A session as returned by the list/find/detail endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionInfo {
    pub id: String,
    pub session_id: SessionId,
    pub title: String,
    pub branch: String,
    pub status: SessionStatus,
    pub program: String,
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
    // Fields below are additive; older clients that don't send them still
    // deserialize (`#[serde(default)]`). They carry everything the TUI tree
    // builders read from a `WorktreeSession` so a remote client can render the
    // full session tree (stacks, sections, unread/attach state) from the wire.
    //
    // FLUTTER: new SessionInfo fields — mirror them in the Dart model.
    /// Whether the session has unread output (agent finished, user hasn't
    /// attached since).
    #[serde(default)]
    pub unread: bool,
    /// Local stack-parent hint set at creation (before a PR exists). See
    /// `pr_base_branch` for the GitHub-authoritative link.
    #[serde(default)]
    pub stack_parent_session_id: Option<SessionId>,
    /// Branch the PR targets, per GitHub — source of truth for stack grouping.
    #[serde(default)]
    pub pr_base_branch: Option<String>,
    /// Legacy merged flag; prefer `pr_state == Merged`. Kept for tree builders
    /// that read it directly.
    #[serde(default)]
    pub pr_merged: bool,
    /// Cached section name the session currently sits in (`None` = catch-all).
    #[serde(default)]
    pub current_section: Option<String>,
    /// Manual section pin, when set and matching a configured section.
    #[serde(default)]
    pub section_override: Option<String>,
    /// When the session entered its current section (drives in-section sort).
    #[serde(default)]
    pub entered_section_at: Option<DateTime<Utc>>,
    /// Most recent attach time, for the MRU session switcher ordering.
    #[serde(default)]
    pub last_attached_at: Option<DateTime<Utc>>,
    /// Whether the session is exempt from idle hibernation (shows the anchor
    /// marker). Additive with default so pre-hibernation payloads parse.
    /// FLUTTER: mirror lags; field is #[serde(default)].
    #[serde(default)]
    pub keep_alive: bool,
    /// Absolute path to the session's git worktree.
    #[serde(default)]
    /// Server-side worktree path, as a display string. A `String` (not
    /// `PathBuf`) so frb mirrors can transfer it; serde wire form is identical.
    pub worktree_path: String,
    /// tmux session name backing this session.
    #[serde(default)]
    pub tmux_session_name: String,
}

/// A session plus its live detail: agent sub-state, diff summary, and a pane
/// snapshot. `info` is flattened so the JSON is a single object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionDetail {
    #[serde(flatten)]
    pub info: SessionInfo,
    pub agent_state: AgentState,
    pub diff_stat: Option<String>,
    pub pane_content: Option<String>,
}

/// Request to stage a new comment on a session's review diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct NewComment {
    pub file: String,
    pub side: CommentSide,
    pub line_range: (usize, usize),
    pub snippet: String,
    pub comment: String,
}

/// Request to toggle (or set) a file's reviewed mark. Carries only the display
/// path — the server resolves the file in the *current* review diff and hashes
/// that, so clients never echo (or cache) the full `FileDiff` and a mark can't
/// be recorded against a stale copy of the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ToggleReviewed {
    pub display_path: String,
    /// The state the client wants, from what *it* shows; absent toggles. A
    /// view whose copy of the file is older than the stored mark (set by the
    /// CLI meanwhile) would otherwise flip that mark off when the user meant
    /// to mark the file. Additive: an older server ignores it and toggles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub reviewed: Option<bool>,
}

/// Which side of a diff a binary blob fetch refers to: the base ("before") or
/// the working tree ("after").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    Old,
    New,
}

/// Result of opening the review view: the parsed diff plus the session's
/// (re-anchored) comments and the base they were computed against.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ReviewSnapshot {
    pub base: String,
    pub diff: ParsedDiff,
    pub comments: Vec<Comment>,
    /// Display paths of files still marked reviewed (stale marks pruned).
    pub reviewed: Vec<String>,
    /// xxh3 hash of the raw unified diff this snapshot was built from, so an
    /// open review view can cheaply tell whether a re-compose actually changed
    /// anything before rebuilding.
    ///
    /// Typed `unknown` in the generated TypeScript: a u64 hash routinely exceeds
    /// 2^53, so `JSON.parse` rounds it to the nearest double, and a browser that
    /// echoed it back as `prev_hash` would never match — every refresh would
    /// re-send the whole snapshot. The wire stays a JSON number (the Rust and
    /// Flutter clients read it losslessly). A web client must not
    /// treat it as a number; it only matters once the page uses refresh-by-hash,
    /// which will need the raw digits (or a string form) rather than this field.
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub content_hash: u64,
    /// Comments discarded while building this snapshot because the file they
    /// were written against had left the diff entirely (the change was
    /// reverted), leaving nothing to anchor them to and no row to render them
    /// on. Reported so a frontend can tell the user their feedback was dropped
    /// instead of silently deleting it.
    ///
    /// `#[serde(default)]`: an older server omits the field entirely.
    #[serde(default)]
    pub dropped_comments: Vec<Comment>,
    /// The raw unified diff [`Self::diff`] was parsed from, so a client can
    /// re-parse it locally with a richer model than [`ParsedDiff`] carries
    /// (`\ No newline at EOF`, CRLF, rename similarity) and lay it out itself.
    /// This is what lets the Flutter client render word-diff emphasis,
    /// side-by-side and context expansion in its own cdylib rather than asking
    /// the server for pre-rendered rows.
    ///
    /// `#[serde(default)]` + `Option`: an older server omits the field, and a
    /// client that gets `None` falls back to converting [`Self::diff`] — which
    /// is lossier, but still enough to lay out. An empty diff is
    /// `Some(String::new())`, never `None`.
    #[serde(default)]
    pub raw: Option<String>,
}

/// Options for creating a session (request body for `POST /sessions`). Optional
/// fields default to absent so a minimal `{project_path, title}` body is valid.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CreateSessionOpts {
    pub project_path: PathBuf,
    pub title: String,
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub initial_prompt: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub section: Option<String>,
    /// Local stack-parent hint: fork this new session's branch from the parent
    /// session's branch and inject the PR-base context at launch. Set by the
    /// "new stacked session" flow. Additive; older clients omit it.
    #[serde(default)]
    pub stack_parent: Option<SessionId>,
}

/// A project (git repository) as returned by the workspace/list endpoints.
///
/// FLUTTER: mirror this DTO in the Dart model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProjectInfo {
    pub id: ProjectId,
    pub name: String,
    pub repo_path: PathBuf,
    pub main_branch: String,
    /// Session ids belonging to this project (order as stored).
    pub session_ids: Vec<SessionId>,
    /// URL of the repo's `origin` remote, or `None` when it has none. Clients
    /// compare it against a candidate clone URL via
    /// [`canonical_repo_slug`](crate::github::canonical_repo_slug) to tell which
    /// repos are already registered — never by string equality, since the same
    /// repo has several spellings. Additive; older servers omit it.
    #[serde(default)]
    pub origin_url: Option<String>,
    /// Name of the workspace this project is tagged with; `None` is the
    /// built-in Main workspace. Additive: an older server omits it (every
    /// project reads as Main), and an older client ignores it.
    ///
    /// FLUTTER: mirror this field.
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Body for `POST /projects`, `POST /projects/ensure` and `POST
/// /projects/scan` (where `path` is the directory to scan).
///
/// `workspace` tags a *newly registered* project (so it lands in the caller's
/// active workspace); `ensure` and `scan` never re-tag a project that already
/// exists.
/// Additive: an older client sends only `path`, and the project lands in Main.
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct AddProjectRequest {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// Why a background fast-forward of a project's main branch was held back.
/// Mirrors core's `git::BlockReason` across the wire.
///
/// FLUTTER: mirror this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum PullBlockReason {
    Dirty,
    Diverged,
    WorktreeConflict,
}

/// Outcome of the most recent background pull for a project. Mirrors core's
/// `git::PullOutcome`.
///
/// FLUTTER: mirror this enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PullStatus {
    /// Fast-forward applied.
    Advanced,
    /// Already up to date.
    UpToDate,
    /// Held back with a user-visible reason (drives the project row badge).
    Blocked { reason: PullBlockReason },
    /// Soft failure (network / no remote / fetch error); no badge.
    SoftFail,
}

/// Which long-running stack operation an [`OperationStatus`] describes.
///
/// FLUTTER: mirror this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Cascade,
    PushStack,
}

/// Terminal result of a stack operation recorded in the service's ledger.
///
/// FLUTTER: mirror this enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OperationOutcome {
    /// Completed cleanly. `detail` is a short human summary (e.g. "3 merged").
    Succeeded { detail: String },
    /// A cascade paused on a merge conflict at a session. `detail` summarises.
    Paused { detail: String },
    /// Failed with an error message.
    Failed { error: String },
}

/// One entry in the service's in-memory ring ledger of recent cascade /
/// push-stack operations, surfaced through [`Snapshot`].
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct OperationStatus {
    /// Monotonic id assigned by the service (stable for the process lifetime).
    pub id: u64,
    pub kind: OperationKind,
    pub outcome: OperationOutcome,
    /// When the operation finished. `None` while still running (Phase A always
    /// runs to completion, so this is currently always `Some`).
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

/// Health of the server's local environment, for the client status bar.
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ServerStatus {
    /// Whether the `gh` CLI is installed and runnable.
    pub gh_available: bool,
    /// Whether tmux is available.
    pub tmux_ok: bool,
    /// Server crate version (`CARGO_PKG_VERSION`).
    pub version: String,
}

/// A single snapshot of everything the session tree needs to render: projects,
/// sessions, cascade state, pending-comment indicators, per-project pull status,
/// the recent-operations ledger, and server health.
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Snapshot {
    pub projects: Vec<ProjectInfo>,
    pub sessions: Vec<SessionInfo>,
    /// The session a paused cascade is stalled at, if any.
    #[serde(default)]
    pub cascade_paused: Option<SessionId>,
    /// Sessions with at least one not-yet-applied review comment.
    #[serde(default)]
    pub pending_comment_sessions: Vec<SessionId>,
    /// Most recent background-pull outcome per project. Populated by the core
    /// background loop (Phase D); empty in Phase A.
    #[serde(default)]
    pub project_pull: BTreeMap<ProjectId, PullStatus>,
    /// Recent cascade / push-stack operations, newest last.
    #[serde(default)]
    pub operations: Vec<OperationStatus>,
    pub server: ServerStatus,
    /// This server's workspace definitions, in configured order. Additive: an
    /// older server omits it (it has only Main).
    ///
    /// FLUTTER: mirror this field.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceDef>,
    /// Label of this server's built-in Main workspace; `None` means
    /// the default label ([`MAIN_WORKSPACE_LABEL`](crate::workspace::MAIN_WORKSPACE_LABEL)).
    ///
    /// FLUTTER: mirror this field.
    #[serde(default)]
    pub main_workspace: Option<WorkspaceDef>,
    /// This server's configured `startup_workspace`.
    ///
    /// FLUTTER: mirror this field.
    #[serde(default)]
    pub startup_workspace: StartupWorkspace,
}

/// Bulk agent-state snapshot for active sessions.
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct AgentStatesSnapshot {
    /// Per-session agent state, keyed by session id. NOTE: when a commander is
    /// running, this map also carries one synthetic entry under the commander
    /// *sentinel* id — a fixed reserved `SessionId` that does not correspond to
    /// any real session/worktree. Clients that iterate `states` to render
    /// per-session rows must skip it; it exists only to drive the commander
    /// chip's own state. Its running-ness is also surfaced via
    /// `commander_running` below.
    pub states: BTreeMap<SessionId, AgentState>,
    /// Whether a commander agent process appears to be running anywhere (used
    /// by the client to distinguish "no data yet" from "nothing running").
    pub commander_running: bool,
}

/// Preview payload for a session or project: the agent pane snapshot, the diff
/// text and its stat line, and the shell pane snapshot. Fields are `None`/empty
/// when not applicable (projects have no agent pane; some targets have no shell).
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PreviewData {
    #[serde(default)]
    pub pane: Option<String>,
    pub diff_text: String,
    /// Human-readable one-line summary (e.g. `"3 file(s), +12 -4 lines"`).
    #[serde(default)]
    pub diff_stat: Option<String>,
    /// Structured diff counts, so a client can rebuild its own diff view/stat
    /// widget rather than re-parsing `diff_text`. `None` when there is no diff.
    #[serde(default)]
    pub stats: Option<DiffStat>,
    #[serde(default)]
    pub shell: Option<String>,
}

/// Numeric diff counts carried alongside [`PreviewData`].
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DiffStat {
    pub files_changed: usize,
    pub lines_added: usize,
    pub lines_removed: usize,
}

/// A launch program option for the new-session picker.
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ProgramInfo {
    pub label: String,
    pub command: String,
}

/// Body for `PUT /config/programs`: replaces the server's configured program
/// list wholesale. A dedicated endpoint (rather than `PATCH /config`) keeps the
/// general config-patch allow-list locked down while still letting a client edit
/// the picker list.
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SetProgramsRequest {
    pub programs: Vec<ProgramInfo>,
}

/// Options for the new-session dialog: the default program, the configured
/// program list, and the configured section names.
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CreateOptions {
    pub default_program: String,
    pub programs: Vec<ProgramInfo>,
    /// Configured section names (for the section picker).
    pub sections: Vec<String>,
}

/// A git branch as returned by the branch picker.
///
/// FLUTTER: mirror this DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct BranchInfo {
    pub name: String,
    pub is_remote: bool,
}

/// Request body for renaming a session (`PATCH /sessions/{id}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RenameSession {
    pub title: String,
}

/// Request body for moving a session to a section (`PATCH /sessions/{id}`).
/// `section: None` clears the manual override and re-runs predicate assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SetSection {
    #[serde(default)]
    pub section: Option<String>,
}

/// Request body for retargeting a session's stack base
/// (`POST /sessions/{id}/base`). `parent_session_id: None` unstacks the session
/// onto the project's main branch.
///
/// Only the *parent* crosses the wire, never a branch name: the host derives the
/// branch from that session (or the project's main branch), so a client can
/// never disagree with it about where the session actually landed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SetSessionBase {
    #[serde(default)]
    pub parent_session_id: Option<SessionId>,
}

/// What happened to the session's pull request during a base retarget.
///
/// Three-way rather than a bool: "there was no PR" and "we tried and failed"
/// need different words, because only the second leaves work for the user. A
/// failed edit is not cosmetic — the next PR sync overwrites the local mirror
/// from GitHub, so an unreported failure silently reverts the retarget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrRetarget {
    /// The session has no PR, so there was nothing to retarget.
    NoPr,
    /// `gh pr edit --base` succeeded; the new base is durable on GitHub.
    Retargeted { pr_number: u32 },
    /// `gh pr edit --base` failed. The local metadata moved, but the next PR
    /// sync will revert it unless the user fixes the base on GitHub.
    Failed { pr_number: u32, message: String },
}

/// Result of retargeting a session's stack base.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SetSessionBaseOutcome {
    /// The branch the session is now based on.
    pub new_base_branch: String,
    /// The branch it was based on before, when one was known.
    #[serde(default)]
    pub old_base_branch: Option<String>,
    pub pr: PrRetarget,
}

/// Request body for changing a session's launch program (`PATCH /sessions/{id}`).
/// The new program is the command that will be relaunched in the pane; the
/// owning host relaunches the agent fresh so it takes effect.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ChangeProgram {
    pub program: String,
}

/// `PATCH /sessions/{id}` body: rename a session, move it to a section
/// (`section: null` clears the manual override), or change its launch program.
/// Tagged by `op` so a section clear (`null`) is unambiguous.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PatchSession {
    Rename(RenameSession),
    SetSection(SetSection),
    ChangeProgram(ChangeProgram),
}

/// `POST /sessions/unread` body: the session ids (full UUIDs) to flag unread.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct MarkUnread {
    pub ids: Vec<String>,
}

/// `201` body of the create routes (`POST /sessions`, `/projects`,
/// `/projects/ensure`, `/sessions/{id}/comments`): the new resource's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CreatedId<T> {
    pub id: T,
}

/// `POST /sessions/{id}/files/reviewed` response: the file's reviewed mark
/// after the toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ReviewedToggle {
    pub reviewed: bool,
}

/// `GET /sessions/{id}/files/reviewed` element: one stored reviewed mark, as
/// persisted — *not* pruned against the current diff, since that would cost a
/// compose per poll. A mark only applies to the copy of the file whose diff
/// hashes to `hash`; `claude_commander_viewmodel::review` holds the hash and
/// the rules for applying a polled mark, shared by every frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedFileMark {
    /// Display path of the marked file.
    pub file: String,
    /// Hex hunk hash of the file at mark time.
    pub hash: String,
}

/// `POST /projects/scan` response: how many repositories under the scanned
/// directory were newly registered, and how many were already known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ScanResponse {
    pub added: usize,
    pub skipped: usize,
}

/// `POST /config/reload` response: `true` when the on-disk config differed
/// from the live one and was re-read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigReloaded {
    pub reloaded: bool,
}

/// `POST /sessions/{id}/paste-image` response: the absolute path the image was
/// written to *on the server* (the path injected into the pane).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PastedImage {
    pub path: String,
}

/// The uniform error envelope every non-2xx JSON response carries:
/// `{"error": {"kind", "message"}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ApiErrorBody {
    pub error: ApiErrorDetail,
}

/// The inside of [`ApiErrorBody`]. `kind` is a short machine-readable category
/// (`session`, `tmux`, `git`, `config`, `io`, `tts`, `auth`, `request`, ...);
/// `message` is safe to show the user and never carries a credential.
///
/// `kind` defaults to empty when absent, so a client still surfaces `message`
/// from an error body that omits it rather than failing the whole parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ApiErrorDetail {
    #[serde(default)]
    pub kind: String,
    pub message: String,
}

impl ApiErrorBody {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ApiErrorDetail {
                kind: kind.into(),
                message: message.into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The small response envelopes are built by the server and parsed by every
    /// client, so their exact JSON is pinned here — these were ad-hoc `json!`
    /// literals before they were typed, and the move must not change a byte.
    #[test]
    fn response_envelopes_have_the_pinned_wire_shape() {
        let sid = SessionId::from_uuid(uuid::Uuid::from_u128(1));
        assert_eq!(
            serde_json::to_string(&CreatedId { id: sid }).unwrap(),
            r#"{"id":"00000000-0000-0000-0000-000000000001"}"#
        );
        assert_eq!(
            serde_json::to_string(&ReviewedToggle { reviewed: true }).unwrap(),
            r#"{"reviewed":true}"#
        );
        assert_eq!(
            serde_json::to_string(&ReviewedFileMark {
                file: "src/a.rs".into(),
                hash: "00000000000000ff".into()
            })
            .unwrap(),
            r#"{"file":"src/a.rs","hash":"00000000000000ff"}"#
        );
        assert_eq!(
            serde_json::to_string(&ScanResponse {
                added: 2,
                skipped: 1
            })
            .unwrap(),
            r#"{"added":2,"skipped":1}"#
        );
        assert_eq!(
            serde_json::to_string(&ConfigReloaded { reloaded: false }).unwrap(),
            r#"{"reloaded":false}"#
        );
        assert_eq!(
            serde_json::to_string(&PastedImage {
                path: "/srv/x.png".into()
            })
            .unwrap(),
            r#"{"path":"/srv/x.png"}"#
        );
        assert_eq!(
            serde_json::to_string(&ApiErrorBody::new("session", "nope")).unwrap(),
            r#"{"error":{"kind":"session","message":"nope"}}"#
        );
    }

    #[test]
    fn patch_session_and_mark_unread_have_the_pinned_wire_shape() {
        let cases = [
            (
                PatchSession::Rename(RenameSession { title: "t".into() }),
                r#"{"op":"rename","title":"t"}"#,
            ),
            (
                PatchSession::SetSection(SetSection { section: None }),
                r#"{"op":"set_section","section":null}"#,
            ),
            (
                PatchSession::ChangeProgram(ChangeProgram {
                    program: "claude".into(),
                }),
                r#"{"op":"change_program","program":"claude"}"#,
            ),
        ];
        for (patch, json) in cases {
            assert_eq!(serde_json::to_string(&patch).unwrap(), json);
            let back: PatchSession = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
        assert_eq!(
            serde_json::to_string(&MarkUnread {
                ids: vec!["a".into()]
            })
            .unwrap(),
            r#"{"ids":["a"]}"#
        );
    }

    #[test]
    fn create_session_opts_minimal_body_deserializes() {
        // The optional fields are `#[serde(default)]`, so a minimal body is valid.
        let opts: CreateSessionOpts =
            serde_json::from_str(r#"{"project_path":"/repo","title":"x"}"#).unwrap();
        assert_eq!(opts.title, "x");
        assert!(opts.program.is_none());
        assert!(opts.effort.is_none());
        // The additive `stack_parent` field defaults to absent for old bodies.
        assert!(opts.stack_parent.is_none());
    }

    #[test]
    fn create_session_opts_stack_parent_round_trips() {
        let parent = SessionId::new();
        let opts = CreateSessionOpts {
            project_path: PathBuf::from("/repo"),
            title: "child".to_string(),
            program: None,
            initial_prompt: None,
            effort: None,
            mode: None,
            model: None,
            base_branch: None,
            section: None,
            stack_parent: Some(parent),
        };
        let json = serde_json::to_string(&opts).unwrap();
        let back: CreateSessionOpts = serde_json::from_str(&json).unwrap();
        assert_eq!(back.stack_parent, Some(parent));
    }

    #[test]
    fn new_comment_requires_all_fields() {
        // Unlike CreateSessionOpts, NewComment fields are all required.
        assert!(serde_json::from_str::<NewComment>(r#"{"file":"a.rs"}"#).is_err());
        let c: NewComment = serde_json::from_str(
            r#"{"file":"a.rs","side":"new","line_range":[1,2],"snippet":"x","comment":"y"}"#,
        )
        .unwrap();
        assert_eq!(c.side, CommentSide::New);
        assert_eq!(c.line_range, (1, 2));
    }

    #[test]
    fn agent_states_serialize_deterministically_regardless_of_insertion_order() {
        // Remote clients hash the raw response bytes to detect change; a map
        // whose key order varied between polls would defeat that diffing and
        // degrade polling to "always changed" (BTreeMap pins the order).
        let ids: Vec<SessionId> = (0..8).map(|_| SessionId::new()).collect();
        let forward: BTreeMap<SessionId, AgentState> =
            ids.iter().map(|id| (*id, AgentState::Idle)).collect();
        let reverse: BTreeMap<SessionId, AgentState> =
            ids.iter().rev().map(|id| (*id, AgentState::Idle)).collect();
        let a = serde_json::to_vec(&AgentStatesSnapshot {
            states: forward,
            commander_running: true,
        })
        .unwrap();
        let b = serde_json::to_vec(&AgentStatesSnapshot {
            states: reverse,
            commander_running: true,
        })
        .unwrap();
        assert_eq!(a, b, "insertion order leaked into the wire bytes");
    }

    #[test]
    fn diff_side_wire_form() {
        assert_eq!(serde_json::to_string(&DiffSide::Old).unwrap(), r#""old""#);
        assert_eq!(
            serde_json::from_str::<DiffSide>(r#""new""#).unwrap(),
            DiffSide::New
        );
    }

    /// A `SessionInfo` JSON written before the additive fields existed must
    /// still deserialize, with the new fields defaulting.
    #[test]
    fn session_info_back_compat_defaults_new_fields() {
        let json = r#"{
            "id": "abc",
            "session_id": "00000000-0000-0000-0000-000000000000",
            "title": "t",
            "branch": "b",
            "status": "running",
            "program": "claude",
            "project_id": "00000000-0000-0000-0000-000000000000",
            "project_name": "p",
            "pr_number": null,
            "pr_url": null,
            "pr_state": "open",
            "pr_draft": false,
            "pr_labels": [],
            "review_decision": null,
            "pr_reviewers": [],
            "created_at": "2024-01-01T00:00:00Z"
        }"#;
        let info: SessionInfo = serde_json::from_str(json).unwrap();
        assert!(!info.unread);
        assert!(info.stack_parent_session_id.is_none());
        assert!(info.pr_base_branch.is_none());
        assert!(!info.pr_merged);
        assert!(info.current_section.is_none());
        assert!(info.section_override.is_none());
        assert!(info.entered_section_at.is_none());
        assert!(info.last_attached_at.is_none());
        assert_eq!(info.worktree_path, String::new());
        assert_eq!(info.tmux_session_name, "");
    }

    #[test]
    fn pull_status_wire_forms() {
        assert_eq!(
            serde_json::to_string(&PullStatus::Advanced).unwrap(),
            r#"{"kind":"advanced"}"#
        );
        let blocked = PullStatus::Blocked {
            reason: PullBlockReason::Dirty,
        };
        let json = serde_json::to_string(&blocked).unwrap();
        assert_eq!(json, r#"{"kind":"blocked","reason":"dirty"}"#);
        // Round-trips through the tagged form.
        match serde_json::from_str::<PullStatus>(&json).unwrap() {
            PullStatus::Blocked { reason } => assert_eq!(reason, PullBlockReason::Dirty),
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn operation_status_round_trips() {
        let op = OperationStatus {
            id: 7,
            kind: OperationKind::Cascade,
            outcome: OperationOutcome::Paused {
                detail: "2 merged".to_string(),
            },
            finished_at: Some(Utc::now()),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: OperationStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.kind, OperationKind::Cascade);
        matches!(back.outcome, OperationOutcome::Paused { .. });
    }

    #[test]
    fn snapshot_round_trips_with_maps() {
        let pid = ProjectId::new();
        let sid = SessionId::new();
        let mut project_pull = BTreeMap::new();
        project_pull.insert(pid, PullStatus::UpToDate);
        let snapshot = Snapshot {
            projects: vec![ProjectInfo {
                id: pid,
                name: "repo".to_string(),
                repo_path: PathBuf::from("/repo"),
                main_branch: "main".to_string(),
                session_ids: vec![sid],
                origin_url: Some("git@github.com:sizeak/claude-commander.git".to_string()),
                workspace: Some("Work".to_string()),
            }],
            sessions: vec![],
            cascade_paused: Some(sid),
            pending_comment_sessions: vec![sid],
            project_pull,
            operations: vec![],
            server: ServerStatus {
                gh_available: true,
                tmux_ok: true,
                version: "0.0.0".to_string(),
            },
            workspaces: vec![WorkspaceDef::named("Work")],
            main_workspace: Some(WorkspaceDef::named("Home")),
            startup_workspace: StartupWorkspace::Named("Work".to_string()),
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.projects[0].workspace.as_deref(), Some("Work"));
        assert_eq!(back.workspaces, snapshot.workspaces);
        assert_eq!(back.main_workspace, snapshot.main_workspace);
        assert_eq!(back.startup_workspace, snapshot.startup_workspace);
        assert_eq!(back.projects.len(), 1);
        assert_eq!(
            back.projects[0].origin_url.as_deref(),
            Some("git@github.com:sizeak/claude-commander.git")
        );
        assert_eq!(back.cascade_paused, Some(sid));
        assert!(back.project_pull.contains_key(&pid));
        assert!(back.server.gh_available);
    }

    /// A `Snapshot` with the optional collections omitted still
    /// deserializes (they default to empty).
    #[test]
    fn snapshot_defaults_optional_collections() {
        let json = r#"{
            "projects": [],
            "sessions": [],
            "server": {"gh_available": false, "tmux_ok": false, "version": "x"}
        }"#;
        let snap: Snapshot = serde_json::from_str(json).unwrap();
        assert!(snap.cascade_paused.is_none());
        assert!(snap.pending_comment_sessions.is_empty());
        assert!(snap.project_pull.is_empty());
        assert!(snap.operations.is_empty());
        // An older server has no workspaces: only Main, default label, `last`.
        assert!(snap.workspaces.is_empty());
        assert!(snap.main_workspace.is_none());
        assert_eq!(snap.startup_workspace, StartupWorkspace::Last);
    }

    /// The other direction of wire compatibility: an *older client* decoding a
    /// new server's payload. Its structs lack the workspace fields, and serde's
    /// default (no `deny_unknown_fields`) must let it ignore them — pinned with
    /// a stand-in for the pre-workspace shapes so adding `deny_unknown_fields`
    /// to either DTO would fail here.
    #[test]
    fn an_older_client_ignores_the_workspace_fields() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldProjectInfo {
            id: ProjectId,
            name: String,
            repo_path: PathBuf,
            main_branch: String,
            session_ids: Vec<SessionId>,
        }
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldSnapshot {
            projects: Vec<OldProjectInfo>,
            sessions: Vec<serde_json::Value>,
            server: ServerStatus,
        }
        let json = r##"{
            "projects": [{
                "id": "1b4e28ba-2fa1-11d2-883f-b9a761bde3fb",
                "name": "repo", "repo_path": "/repo", "main_branch": "main",
                "session_ids": [], "workspace": "Work"
            }],
            "sessions": [],
            "server": {"gh_available": false, "tmux_ok": false, "version": "x"},
            "workspaces": [{"name": "Work"}],
            "main_workspace": {"name": "Home"},
            "startup_workspace": "last"
        }"##;
        let old: OldSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(old.projects[0].name, "repo");
    }

    /// `POST /projects` from an older client (path only) lands in Main; the
    /// field is skipped on the way out when absent, so a new client talking to
    /// an old server sends exactly the old body.
    #[test]
    fn add_project_request_workspace_is_optional_both_ways() {
        let old: AddProjectRequest = serde_json::from_str(r#"{"path":"/repo"}"#).unwrap();
        assert_eq!(old.workspace, None);
        let body = serde_json::to_string(&AddProjectRequest {
            path: PathBuf::from("/repo"),
            workspace: None,
        })
        .unwrap();
        assert_eq!(body, r#"{"path":"/repo"}"#);
    }

    /// `origin_url` is additive: a payload from a server that predates it must
    /// still deserialize, with the field absent rather than the whole snapshot
    /// failing.
    #[test]
    fn project_info_defaults_origin_url_from_an_older_server() {
        let json = r#"{
            "id": "1b4e28ba-2fa1-11d2-883f-b9a761bde3fb",
            "name": "repo",
            "repo_path": "/repo",
            "main_branch": "main",
            "session_ids": []
        }"#;
        let info: ProjectInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.name, "repo");
        assert_eq!(info.origin_url, None);
        assert_eq!(info.workspace, None, "an older server's project is in Main");
    }

    #[test]
    fn agent_states_snapshot_round_trips() {
        let sid = SessionId::new();
        let mut states = BTreeMap::new();
        states.insert(sid, AgentState::Working);
        let snap = AgentStatesSnapshot {
            states,
            commander_running: true,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: AgentStatesSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.states.get(&sid), Some(&AgentState::Working));
        assert!(back.commander_running);
    }

    #[test]
    fn preview_data_round_trips() {
        let p = PreviewData {
            pane: Some("hello".to_string()),
            diff_text: "diff".to_string(),
            diff_stat: Some("1 file(s)".to_string()),
            stats: Some(DiffStat {
                files_changed: 1,
                lines_added: 2,
                lines_removed: 3,
            }),
            shell: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: PreviewData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pane.as_deref(), Some("hello"));
        assert_eq!(back.diff_text, "diff");
        let s = back.stats.unwrap();
        assert_eq!((s.files_changed, s.lines_added, s.lines_removed), (1, 2, 3));
        assert!(back.shell.is_none());
    }

    #[test]
    fn create_options_and_branch_info_round_trip() {
        let opts = CreateOptions {
            default_program: "claude".to_string(),
            programs: vec![ProgramInfo {
                label: "Claude".to_string(),
                command: "claude".to_string(),
            }],
            sections: vec!["Open PRs".to_string()],
        };
        let json = serde_json::to_string(&opts).unwrap();
        let back: CreateOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(back.default_program, "claude");
        assert_eq!(back.programs[0].label, "Claude");
        assert_eq!(back.sections, vec!["Open PRs".to_string()]);

        let b: BranchInfo = serde_json::from_str(r#"{"name":"main","is_remote":false}"#).unwrap();
        assert_eq!(b.name, "main");
        assert!(!b.is_remote);
    }

    #[test]
    fn set_programs_request_round_trip() {
        let req = SetProgramsRequest {
            programs: vec![
                ProgramInfo {
                    label: "Claude (Opus)".to_string(),
                    command: "claude --model opus".to_string(),
                },
                ProgramInfo {
                    label: "Shell".to_string(),
                    command: "bash".to_string(),
                },
            ],
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: SetProgramsRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.programs.len(), 2);
        assert_eq!(back.programs[0].command, "claude --model opus");
        assert_eq!(back.programs[1].label, "Shell");

        // An empty list is a valid request (clears the picker to the fallback).
        let empty: SetProgramsRequest = serde_json::from_str(r#"{"programs":[]}"#).unwrap();
        assert!(empty.programs.is_empty());
    }

    /// A snapshot from a server predating the `raw` field still deserializes,
    /// with `raw: None` telling the client to fall back to `diff`. Without the
    /// `#[serde(default)]` this is a hard decode failure and the review view
    /// goes blank against every older server.
    #[test]
    fn review_snapshot_tolerates_a_server_without_raw() {
        let wire = r#"{
            "base": "main",
            "diff": {"files": []},
            "comments": [],
            "reviewed": [],
            "content_hash": 7
        }"#;
        let snap: ReviewSnapshot = serde_json::from_str(wire).unwrap();
        assert_eq!(snap.base, "main");
        assert!(snap.raw.is_none());

        // And a current server's `raw` survives the round trip. An empty diff
        // is `Some("")` — distinguishable from "this server never sent one".
        let snap = ReviewSnapshot {
            raw: Some(String::new()),
            ..snap
        };
        let back: ReviewSnapshot =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(back.raw.as_deref(), Some(""));
    }

    #[test]
    fn set_session_base_body_roundtrips() {
        // An explicit null unstacks onto main, and an absent field means the
        // same — a client that omits it must not be read as naming a parent.
        let unstack: SetSessionBase =
            serde_json::from_str(r#"{"parent_session_id":null}"#).unwrap();
        assert!(unstack.parent_session_id.is_none());
        let absent: SetSessionBase = serde_json::from_str("{}").unwrap();
        assert!(absent.parent_session_id.is_none());

        let id = SessionId::new();
        let body = SetSessionBase {
            parent_session_id: Some(id),
        };
        let back: SetSessionBase =
            serde_json::from_str(&serde_json::to_string(&body).unwrap()).unwrap();
        assert_eq!(back.parent_session_id, Some(id));
    }

    #[test]
    fn set_session_base_outcome_roundtrips_each_pr_state() {
        for pr in [
            PrRetarget::NoPr,
            PrRetarget::Retargeted { pr_number: 7 },
            PrRetarget::Failed {
                pr_number: 7,
                message: "not found".to_string(),
            },
        ] {
            let outcome = SetSessionBaseOutcome {
                new_base_branch: "main".to_string(),
                old_base_branch: Some("feat".to_string()),
                pr: pr.clone(),
            };
            let back: SetSessionBaseOutcome =
                serde_json::from_str(&serde_json::to_string(&outcome).unwrap()).unwrap();
            assert_eq!(back.pr, pr);
            assert_eq!(back.new_base_branch, "main");
            assert_eq!(back.old_base_branch.as_deref(), Some("feat"));
        }
    }

    /// Clients read the envelope leniently: a body with a `message` but no
    /// `kind` (an older server, a proxy's own JSON error) still yields the
    /// message instead of falling back to a bare status line.
    #[test]
    fn error_body_without_a_kind_still_parses() {
        let body: ApiErrorBody = serde_json::from_str(r#"{"error":{"message":"busy"}}"#).unwrap();
        assert_eq!(body.error.message, "busy");
        assert_eq!(body.error.kind, "");
    }

    #[test]
    fn rename_and_set_section_bodies() {
        let r: RenameSession = serde_json::from_str(r#"{"title":"new"}"#).unwrap();
        assert_eq!(r.title, "new");
        // SetSection.section is optional: a `{}` body clears the override.
        let clear: SetSection = serde_json::from_str(r#"{}"#).unwrap();
        assert!(clear.section.is_none());
        let set: SetSection = serde_json::from_str(r#"{"section":"Open PRs"}"#).unwrap();
        assert_eq!(set.section.as_deref(), Some("Open PRs"));
    }
}
