//! CLI-only presentation and gating helpers.
//!
//! Everything here serves the `claude-commander` binary's subcommand output and
//! confirmation prompts: JSON/human formatting of sessions and status, and the
//! `delete` command's `--force`/TTY guard. Shared session resolution lives in
//! [`crate::session::lookup`], not here.

use chrono::{DateTime, Utc};
use serde::Serialize;

use claude_commander_viewmodel::workspace::{
    MergedWorkspace, WorkspaceSource, find_workspace, merge_workspace_sources, workspaces_visible,
};

use crate::api::ReviewPeek;
use crate::config::{AppState, Config};
use crate::error::{Error, SessionError};
use crate::git::DEV_NULL;
use crate::git::{FileDiff, FileStatus, PrState, ReviewDecision, effective_pr_state};
use crate::reviewed::{FileSelection, SetReviewedOutcome, resolve_selection};
use crate::session::{AgentState, WorktreeSession};

/// What the `delete` command should do before mutating, given the `--force`
/// flag and whether stdin is an interactive terminal.
#[derive(Debug, PartialEq, Eq)]
pub enum DeleteGuard {
    /// `--force` was given: delete without asking.
    Proceed,
    /// Interactive terminal: prompt the user for confirmation.
    Prompt,
    /// Non-interactive stdin without `--force`: refuse to delete.
    RequireForce,
}

/// Decide how the `delete` command should guard against accidental deletion.
pub fn delete_guard(force: bool, stdin_is_tty: bool) -> DeleteGuard {
    if force {
        DeleteGuard::Proceed
    } else if stdin_is_tty {
        DeleteGuard::Prompt
    } else {
        DeleteGuard::RequireForce
    }
}

/// Parse a yes/no confirmation answer. Only `y`/`yes` (case-insensitive,
/// surrounding whitespace ignored) count as yes; everything else is no.
pub fn parse_yes_no(input: &str) -> bool {
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

/// JSON-serializable session entry for CLI output.
#[derive(Debug, Serialize)]
pub struct SessionJsonEntry {
    pub id: String,
    pub title: String,
    pub branch: String,
    pub status: String,
    pub program: String,
    pub project_name: String,
    pub pr_number: Option<u32>,
    pub pr_url: Option<String>,
    /// Resolved PR state, accounting for legacy `pr_merged` field.
    /// Only meaningful when `pr_number` is `Some`.
    pub pr_state: PrState,
    pub pr_draft: bool,
    pub pr_labels: Vec<String>,
    pub created_at: DateTime<Utc>,
    /// The session's workspace (its project's tag); `null` is Main.
    pub workspace: Option<String>,
}

impl SessionJsonEntry {
    /// Stamp the entry with its project's workspace tag.
    pub fn with_workspace(mut self, workspace: Option<String>) -> Self {
        self.workspace = workspace;
        self
    }

    pub fn from_info(info: &crate::api::SessionInfo) -> Self {
        Self {
            id: info.id.clone(),
            title: info.title.clone(),
            branch: info.branch.clone(),
            status: info.status.to_string(),
            program: info.program.clone(),
            project_name: info.project_name.clone(),
            pr_number: info.pr_number,
            pr_url: info.pr_url.clone(),
            pr_state: info.pr_state,
            pr_draft: info.pr_draft,
            pr_labels: info.pr_labels.clone(),
            created_at: info.created_at,
            workspace: None,
        }
    }

    pub fn from_session(session: &WorktreeSession, project_name: &str) -> Self {
        Self {
            id: session.id.as_uuid().to_string(),
            title: session.title.clone(),
            branch: session.branch.clone(),
            status: session.status.to_string(),
            program: session.program.clone(),
            project_name: project_name.to_string(),
            pr_number: session.pr_number,
            pr_url: session.pr_url.clone(),
            pr_state: effective_pr_state(session.pr_state, session.pr_merged),
            pr_draft: session.pr_draft,
            pr_labels: session.pr_labels.clone(),
            created_at: session.created_at,
            workspace: None,
        }
    }
}

/// Workspace context for `list`: the merged workspace list for this host and
/// what `--workspace` resolved to.
#[derive(Debug, Clone)]
pub struct CliWorkspaces {
    pub workspaces: Vec<MergedWorkspace>,
    /// `None` = no `--workspace` flag (list everything); `Some(tag)` = only
    /// projects tagged `tag` (`Some(None)` = Main).
    pub filter: Option<Option<String>>,
}

impl CliWorkspaces {
    /// Build from this host's config and the tags its projects carry, and
    /// resolve `flag` (a name, case-insensitively, or Main by its label or the
    /// word `main`). An unknown name is an error listing what exists.
    pub fn resolve<'a>(
        config: &Config,
        project_tags: impl IntoIterator<Item = &'a str>,
        flag: Option<&str>,
    ) -> crate::Result<Self> {
        let source = WorkspaceSource {
            defs: &config.workspaces,
            main: config.main_workspace.as_ref(),
            project_tags: project_tags.into_iter().collect(),
        };
        let workspaces = merge_workspace_sources(&[source]);
        let filter = match flag {
            None => None,
            Some(typed) => match find_workspace(&workspaces, typed) {
                Some(w) => Some(w.name.clone()),
                None => {
                    let available = workspaces
                        .iter()
                        .map(|w| w.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(crate::error::ConfigError::InvalidValue {
                        key: "workspace".to_string(),
                        reason: format!("no workspace named '{typed}' (available: {available})"),
                    }
                    .into());
                }
            },
        };
        Ok(Self { workspaces, filter })
    }

    /// Whether the human listing shows a WORKSPACE column: only once there is
    /// more than one workspace, matching the TUI hiding its chip until then.
    pub fn show_column(&self) -> bool {
        workspaces_visible(&self.workspaces)
    }

    /// Whether a project tagged `tag` passes the `--workspace` filter.
    pub fn includes(&self, tag: Option<&str>) -> bool {
        self.filter.as_ref().is_none_or(|f| f.as_deref() == tag)
    }

    /// Display label for `tag` (Main's configured label for `None`).
    pub fn label(&self, tag: Option<&str>) -> String {
        self.workspaces
            .iter()
            .find(|w| w.name.as_deref() == tag)
            .map_or_else(|| tag.unwrap_or_default().to_string(), |w| w.label.clone())
    }
}

/// The tag `new --workspace <typed>` should give a project it registers: an
/// existing workspace (matched like `list --workspace`) keeps its spelling;
/// anything else is taken as a new name, which the host validates and defines.
pub fn resolve_new_session_workspace(
    snapshot: &crate::api::Snapshot,
    typed: &str,
) -> Option<String> {
    let workspaces = claude_commander_viewmodel::workspace::merge_workspaces([snapshot]);
    match find_workspace(&workspaces, typed) {
        Some(w) => w.name.clone(),
        None => Some(typed.trim().to_string()),
    }
}

/// Build a JSON-serializable list of sessions, optionally including stopped ones.
pub fn build_session_list(state: &AppState, include_stopped: bool) -> Vec<SessionJsonEntry> {
    let mut entries = Vec::new();
    for project in state.projects.values() {
        for session in project
            .worktrees
            .iter()
            .filter_map(|id| state.sessions.get(id))
            .filter(|s| include_stopped || s.status.is_active())
        {
            entries.push(SessionJsonEntry::from_session(session, &project.name));
        }
    }
    entries
}

/// JSON-serializable session detail for the `status` subcommand.
#[derive(Debug, Serialize)]
pub struct StatusJsonEntry {
    pub id: String,
    pub title: String,
    pub branch: String,
    pub status: String,
    pub program: String,
    pub project_name: String,
    pub agent_state: String,
    pub diff_stat: Option<String>,
    pub pr_number: Option<u32>,
    pub pr_url: Option<String>,
    pub pr_state: PrState,
    pub pr_draft: bool,
    pub pr_labels: Vec<String>,
    pub review_decision: Option<ReviewDecision>,
    pub pr_reviewers: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl StatusJsonEntry {
    pub fn from_detail(detail: &crate::api::SessionDetail) -> Self {
        let info = &detail.info;
        Self {
            id: info.id.clone(),
            title: info.title.clone(),
            branch: info.branch.clone(),
            status: info.status.to_string(),
            program: info.program.clone(),
            project_name: info.project_name.clone(),
            agent_state: detail.agent_state.to_string(),
            diff_stat: detail.diff_stat.clone(),
            pr_number: info.pr_number,
            pr_url: info.pr_url.clone(),
            pr_state: info.pr_state,
            pr_draft: info.pr_draft,
            pr_labels: info.pr_labels.clone(),
            review_decision: info.review_decision,
            pr_reviewers: info.pr_reviewers.clone(),
            created_at: info.created_at,
        }
    }

    pub fn from_session(
        session: &WorktreeSession,
        project_name: &str,
        agent_state: AgentState,
        diff_stat: Option<String>,
    ) -> Self {
        Self {
            id: session.id.as_uuid().to_string(),
            title: session.title.clone(),
            branch: session.branch.clone(),
            status: session.status.to_string(),
            program: session.program.clone(),
            project_name: project_name.to_string(),
            agent_state: agent_state.to_string(),
            diff_stat,
            pr_number: session.pr_number,
            pr_url: session.pr_url.clone(),
            pr_state: effective_pr_state(session.pr_state, session.pr_merged),
            pr_draft: session.pr_draft,
            pr_labels: session.pr_labels.clone(),
            review_decision: session.review_decision,
            pr_reviewers: session.pr_reviewers.clone(),
            created_at: session.created_at,
        }
    }
}

/// Format a human-readable status summary for a session.
pub fn format_status_human(entry: &StatusJsonEntry) -> String {
    let mut lines = Vec::new();
    lines.push(format!("Session: {} ({})", entry.title, entry.id));
    lines.push(format!("Branch:  {}", entry.branch));
    lines.push(format!(
        "Status:  {} | Agent: {}",
        entry.status, entry.agent_state
    ));
    lines.push(format!(
        "Program: {} | Project: {}",
        entry.program, entry.project_name
    ));

    if let Some(ref stat) = entry.diff_stat {
        lines.push(format!("Diff:    {}", stat.trim()));
    }

    if let Some(pr) = entry.pr_number {
        let url = entry.pr_url.as_deref().unwrap_or("(no url)");
        lines.push(format!(
            "PR:      #{} ({}, {}) {}",
            pr,
            entry.pr_state,
            if entry.pr_draft { "draft" } else { "ready" },
            url,
        ));
        if let Some(decision) = entry.review_decision {
            lines.push(format!("Review:  {}", decision));
        }
        if !entry.pr_reviewers.is_empty() {
            lines.push(format!("Reviewers: {}", entry.pr_reviewers.join(", ")));
        }
        if !entry.pr_labels.is_empty() {
            lines.push(format!("Labels:  {}", entry.pr_labels.join(", ")));
        }
    }

    lines.join("\n")
}

/// One file of a session's review diff, for `review files`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ReviewFileEntry {
    /// Display path — what `review diff`/`mark`/`unmark` take.
    pub path: String,
    /// The pre-rename path, for renames only.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
    pub reviewed: bool,
    /// Hex hash of the file's diff. Pass `path@hash` to `review mark` to pin
    /// the mark to this content: it fails if the file has changed since.
    pub hash: String,
}

/// What the `review` commands print when there is nothing to show.
const NOTHING_TO_REVIEW: &str = "No changes to review.";

/// Whether `file` is listed under `--unreviewed` (or listing everything).
fn listed(peek: &ReviewPeek, file: &FileDiff, unreviewed_only: bool) -> bool {
    !unreviewed_only || !peek.reviewed.contains(file.display_path())
}

/// The `review files` listing: the peeked diff's files, in diff order, with
/// their reviewed state and hash; only the unreviewed ones when
/// `unreviewed_only`.
pub fn review_file_entries(peek: &ReviewPeek, unreviewed_only: bool) -> Vec<ReviewFileEntry> {
    peek.diff
        .files
        .iter()
        .filter(|f| listed(peek, f, unreviewed_only))
        .map(|f| ReviewFileEntry {
            path: f.display_path().to_string(),
            old_path: (f.status == FileStatus::Renamed).then(|| f.old_path.clone()),
            status: f.status,
            added: f.added,
            removed: f.removed,
            binary: f.binary.is_some(),
            reviewed: peek.reviewed.contains(f.display_path()),
            hash: peek
                .hashes
                .get(f.display_path())
                .unwrap_or_default()
                .to_string(),
        })
        .collect()
}

/// Human `review files` output: one `[x]`/`[ ]` line per file plus a summary.
pub fn format_review_files_human(entries: &[ReviewFileEntry]) -> String {
    if entries.is_empty() {
        return NOTHING_TO_REVIEW.to_string();
    }
    let mut lines: Vec<String> = entries
        .iter()
        .map(|e| {
            let mark = if e.reviewed { "[x]" } else { "[ ]" };
            let status = e.status.marker();
            let path = match &e.old_path {
                Some(old) => format!("{old} -> {}", e.path),
                None => e.path.clone(),
            };
            let stat = if e.binary {
                "binary".to_string()
            } else {
                format!("+{} -{}", e.added, e.removed)
            };
            format!("{mark} {status} {path} ({stat})")
        })
        .collect();
    let done = entries.iter().filter(|e| e.reviewed).count();
    lines.push(format!("{done}/{} files reviewed", entries.len()));
    lines.join("\n")
}

/// Pick the files `review diff` prints: those named in `paths` (all when
/// empty), narrowed to unreviewed ones when `unreviewed_only`. Paths take the
/// same `PATH@HASH` grammar as `review mark` ([`resolve_selection`]), and
/// every bad one is reported rather than silently printing nothing for it.
pub fn select_review_files<'a>(
    peek: &'a ReviewPeek,
    paths: &[String],
    unreviewed_only: bool,
) -> std::result::Result<Vec<&'a FileDiff>, SessionError> {
    let selection = if paths.is_empty() {
        FileSelection::All
    } else {
        FileSelection::Paths(paths.to_vec())
    };
    Ok(resolve_selection(&peek.diff, &peek.hashes, &selection)?
        .into_iter()
        .filter(|f| listed(peek, f, unreviewed_only))
        .collect())
}

/// The stderr report for a `review` command's expected refusal — a named file
/// that isn't in the diff, or one changed since its hash was read — or `None`
/// for a genuine failure, which keeps its full error report. A refused
/// `mark`/`unmark` (`writes`) writes nothing, so its report says so.
pub fn review_refusal(err: &Error, session: &str, writes: bool) -> Option<String> {
    match err {
        Error::Session(
            e @ (SessionError::FileNotInDiff(_) | SessionError::FileSelectionRejected { .. }),
        ) => {
            let unchanged = if writes { " Nothing was changed." } else { "" };
            Some(format!(
                "{e}.{unchanged}\nUse 'claude-commander review files {session}' to list its files and their hashes."
            ))
        }
        _ => None,
    }
}

/// Render one parsed file back to unified-diff text for `review diff`.
///
/// Rendered from the parsed model rather than sliced from the raw composition
/// so the output is exactly the review view's content (the parser may skip an
/// entry it can't represent, which would misalign a slice). Binary files print
/// git's one-line notice. Not modelled, so omitted: no-newline-at-EOF markers,
/// mode changes and path quoting.
pub fn render_file_diff(file: &FileDiff) -> String {
    let side = |prefix: &str, path: &str| {
        if path == DEV_NULL {
            DEV_NULL.to_string()
        } else {
            format!("{prefix}{path}")
        }
    };
    let git_old = if file.old_path == DEV_NULL {
        &file.new_path
    } else {
        &file.old_path
    };
    let git_new = if file.new_path == DEV_NULL {
        &file.old_path
    } else {
        &file.new_path
    };
    let mut out = format!("diff --git a/{git_old} b/{git_new}\n");
    if file.status == FileStatus::Renamed {
        // Without these a pure rename (no hunks) would print as an empty diff.
        out.push_str(&format!(
            "rename from {}\nrename to {}\n",
            file.old_path, file.new_path
        ));
    }
    if file.binary.is_some() && file.hunks.is_empty() {
        out.push_str(&format!(
            "Binary files {} and {} differ\n",
            side("a/", &file.old_path),
            side("b/", &file.new_path)
        ));
        return out;
    }
    out.push_str(&format!("--- {}\n", side("a/", &file.old_path)));
    out.push_str(&format!("+++ {}\n", side("b/", &file.new_path)));
    for hunk in &file.hunks {
        let heading = if hunk.header.is_empty() {
            String::new()
        } else {
            format!(" {}", hunk.header)
        };
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@{heading}\n",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        ));
        for line in &hunk.lines {
            out.push(line.origin.prefix());
            out.push_str(&line.content);
            out.push('\n');
        }
    }
    out
}

/// Human output for `review mark`/`unmark`.
pub fn format_set_reviewed_human(outcome: &SetReviewedOutcome, reviewed: bool) -> String {
    let (verb, already) = if reviewed {
        ("Marked reviewed", "already reviewed")
    } else {
        ("Unmarked", "already unreviewed")
    };
    let mut lines: Vec<String> = outcome
        .changed
        .iter()
        .map(|p| format!("{verb}: {p}"))
        .collect();
    if !outcome.unchanged.is_empty() {
        lines.push(format!(
            "{} file(s) {already}: {}",
            outcome.unchanged.len(),
            outcome.unchanged.join(", ")
        ));
    }
    if lines.is_empty() {
        lines.push(NOTHING_TO_REVIEW.to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Project, ProjectId, SessionStatus, WorktreeSession};
    use std::path::PathBuf;

    fn make_project(name: &str) -> Project {
        Project::new(name, PathBuf::from("/tmp/repo"), "main")
    }

    fn make_state_with_project(project: &Project, sessions: Vec<WorktreeSession>) -> AppState {
        let mut state = AppState::new();
        let mut proj = project.clone();
        for s in &sessions {
            proj.add_worktree(s.id);
        }
        state.projects.insert(proj.id, proj);
        for s in sessions {
            state.sessions.insert(s.id, s);
        }
        state
    }

    fn make_session(title: &str) -> WorktreeSession {
        WorktreeSession::new(
            ProjectId::new(),
            title,
            format!("branch-{}", title),
            PathBuf::from("/tmp/wt"),
            "claude",
        )
    }

    fn make_session_for_project(title: &str, project_id: ProjectId) -> WorktreeSession {
        WorktreeSession::new(
            project_id,
            title,
            format!("branch-{}", title),
            PathBuf::from("/tmp/wt"),
            "claude",
        )
    }

    // -- Workspace tests --

    fn config_with(defs: &[&str], main: Option<&str>) -> Config {
        use claude_commander_protocol::workspace::WorkspaceDef;
        Config {
            workspaces: defs.iter().map(|d| WorkspaceDef::named(*d)).collect(),
            main_workspace: main.map(WorkspaceDef::named),
            ..Config::default()
        }
    }

    #[test]
    fn cli_workspaces_hide_the_column_until_a_second_workspace() {
        let one = CliWorkspaces::resolve(&config_with(&[], None), [], None).unwrap();
        assert!(!one.show_column());
        assert!(one.includes(Some("anything")), "no flag lists everything");
        let two = CliWorkspaces::resolve(&config_with(&["Work"], None), [], None).unwrap();
        assert!(two.show_column());
        // An orphaned tag is a workspace too.
        let orphan = CliWorkspaces::resolve(&config_with(&[], None), ["Lost"], None).unwrap();
        assert!(orphan.show_column());
    }

    #[test]
    fn cli_workspace_flag_resolves_names_and_main() {
        let c = config_with(&["Work"], Some("Home"));
        let work = CliWorkspaces::resolve(&c, [], Some("work")).unwrap();
        assert_eq!(work.filter, Some(Some("Work".to_string())));
        assert!(work.includes(Some("Work")));
        assert!(!work.includes(None));
        let home = CliWorkspaces::resolve(&c, [], Some("home")).unwrap();
        assert_eq!(home.filter, Some(None));
        assert!(home.includes(None));
        assert_eq!(home.label(None), "Home");
        assert_eq!(home.label(Some("Work")), "Work");
        let err = CliWorkspaces::resolve(&c, [], Some("play")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("play") && msg.contains("Home, Work"), "{msg}");
    }

    #[test]
    fn new_session_workspace_keeps_an_existing_spelling_or_takes_a_new_name() {
        let mut snap = crate::backend::empty_snapshot();
        snap.workspaces = vec![claude_commander_protocol::workspace::WorkspaceDef::named(
            "Work",
        )];
        assert_eq!(
            resolve_new_session_workspace(&snap, "work").as_deref(),
            Some("Work")
        );
        assert_eq!(resolve_new_session_workspace(&snap, "main"), None);
        assert_eq!(
            resolve_new_session_workspace(&snap, " Fresh ").as_deref(),
            Some("Fresh")
        );
    }

    #[test]
    fn json_entry_carries_the_workspace_tag() {
        let entry = SessionJsonEntry::from_session(&make_session("t"), "p")
            .with_workspace(Some("Work".into()));
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["workspace"], "Work");
        let main = SessionJsonEntry::from_session(&make_session("t"), "p");
        assert!(serde_json::to_value(&main).unwrap()["workspace"].is_null());
    }

    // -- SessionJsonEntry tests --

    #[test]
    fn json_entry_has_expected_fields() {
        let session = make_session("fix-bug");
        let entry = SessionJsonEntry::from_session(&session, "my-project");
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();

        assert_eq!(json["title"], "fix-bug");
        assert_eq!(json["branch"], "branch-fix-bug");
        assert_eq!(json["program"], "claude");
        assert_eq!(json["project_name"], "my-project");
        assert_eq!(json["pr_draft"], false);
        assert!(json["pr_number"].is_null());
        assert!(json["pr_url"].is_null());
        // No PR set + pr_merged=false → defaults to "open" via effective_pr_state
        assert_eq!(json["pr_state"], "open");
        assert!(json["pr_labels"].as_array().unwrap().is_empty());
        assert!(json["created_at"].is_string());
        assert!(json["id"].is_string());
        assert!(json["status"].is_string());
    }

    #[test]
    fn json_entry_includes_pr_fields_when_set() {
        let mut session = make_session("with-pr");
        session.pr_number = Some(42);
        session.pr_url = Some("https://github.com/org/repo/pull/42".to_string());
        session.pr_state = Some(crate::git::PrState::Open);
        session.pr_draft = true;
        session.pr_labels = vec!["bug".to_string(), "urgent".to_string()];

        let entry = SessionJsonEntry::from_session(&session, "proj");
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();

        assert_eq!(json["pr_number"], 42);
        assert_eq!(json["pr_url"], "https://github.com/org/repo/pull/42");
        assert_eq!(json["pr_state"], "open");
        assert_eq!(json["pr_draft"], true);
        assert_eq!(json["pr_labels"], serde_json::json!(["bug", "urgent"]));
    }

    #[test]
    fn json_entry_resolves_legacy_pr_merged() {
        let mut session = make_session("legacy");
        session.pr_number = Some(10);
        session.pr_state = None;
        session.pr_merged = true;

        let entry = SessionJsonEntry::from_session(&session, "proj");
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();

        assert_eq!(json["pr_state"], "merged");
    }

    #[test]
    fn json_entry_id_is_full_uuid() {
        let session = make_session("test");
        let entry = SessionJsonEntry::from_session(&session, "proj");
        // SessionId::Display truncates to 8 chars, but JSON should have the full UUID
        assert!(entry.id.len() > 8);
        assert!(uuid::Uuid::parse_str(&entry.id).is_ok());
    }

    // -- build_session_list tests --

    #[test]
    fn build_list_excludes_stopped_by_default() {
        let project = make_project("repo");
        let mut s1 = make_session_for_project("running", project.id);
        s1.set_status(SessionStatus::Running);
        let mut s2 = make_session_for_project("stopped", project.id);
        s2.set_status(SessionStatus::Stopped);

        let state = make_state_with_project(&project, vec![s1, s2]);
        let list = build_session_list(&state, false);

        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "running");
    }

    #[test]
    fn build_list_includes_stopped_when_requested() {
        let project = make_project("repo");
        let mut s1 = make_session_for_project("running", project.id);
        s1.set_status(SessionStatus::Running);
        let mut s2 = make_session_for_project("stopped", project.id);
        s2.set_status(SessionStatus::Stopped);

        let state = make_state_with_project(&project, vec![s1, s2]);
        let list = build_session_list(&state, true);

        assert_eq!(list.len(), 2);
    }

    #[test]
    fn build_list_populates_project_name() {
        let project = make_project("my-repo");
        let s = make_session_for_project("task", project.id);
        let state = make_state_with_project(&project, vec![s]);
        let list = build_session_list(&state, false);

        assert_eq!(list.len(), 1);
        assert_eq!(list[0].project_name, "my-repo");
    }

    #[test]
    fn build_list_empty_when_no_projects() {
        let state = AppState::new();
        let list = build_session_list(&state, true);
        assert!(list.is_empty());
    }

    // -- StatusJsonEntry tests --

    #[test]
    fn status_entry_has_expected_fields() {
        let session = make_session("fix-bug");
        let entry = StatusJsonEntry::from_session(
            &session,
            "proj",
            AgentState::Working,
            Some("3 files changed".to_string()),
        );
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();

        assert_eq!(json["title"], "fix-bug");
        assert_eq!(json["agent_state"], "working");
        assert_eq!(json["diff_stat"], "3 files changed");
        assert_eq!(json["project_name"], "proj");
        assert!(json["pr_reviewers"].as_array().unwrap().is_empty());
        assert!(json["review_decision"].is_null());
    }

    #[test]
    fn status_entry_with_pr_and_reviews() {
        let mut session = make_session("reviewed");
        session.pr_number = Some(5);
        session.pr_url = Some("https://github.com/org/repo/pull/5".to_string());
        session.pr_state = Some(crate::git::PrState::Open);
        session.review_decision = Some(ReviewDecision::Approved);
        session.pr_reviewers = vec!["alice".to_string(), "bob".to_string()];

        let entry = StatusJsonEntry::from_session(&session, "proj", AgentState::Idle, None);
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();

        assert_eq!(json["pr_number"], 5);
        assert_eq!(json["review_decision"], "approved");
        assert_eq!(json["pr_reviewers"], serde_json::json!(["alice", "bob"]));
        assert!(json["diff_stat"].is_null());
    }

    #[test]
    fn status_entry_agent_state_variants() {
        let session = make_session("test");
        for (state, expected) in [
            (AgentState::Working, "working"),
            (AgentState::Idle, "idle"),
            (AgentState::WaitingForInput, "waiting"),
            (AgentState::Unknown, "unknown"),
        ] {
            let entry = StatusJsonEntry::from_session(&session, "p", state, None);
            assert_eq!(entry.agent_state, expected);
        }
    }

    // -- format_status_human tests --

    #[test]
    fn human_format_includes_basic_info() {
        let session = make_session("my-task");
        let entry = StatusJsonEntry::from_session(
            &session,
            "my-project",
            AgentState::Working,
            Some("2 files changed, 10 insertions(+)".to_string()),
        );
        let output = format_status_human(&entry);

        assert!(output.contains("my-task"));
        assert!(output.contains("my-project"));
        assert!(output.contains("working"));
        assert!(output.contains("2 files changed"));
    }

    #[test]
    fn human_format_shows_pr_when_present() {
        let mut session = make_session("with-pr");
        session.pr_number = Some(42);
        session.pr_url = Some("https://example.com/pull/42".to_string());
        session.pr_state = Some(crate::git::PrState::Open);
        session.pr_labels = vec!["bug".to_string()];
        session.pr_reviewers = vec!["alice".to_string()];
        session.review_decision = Some(ReviewDecision::ChangesRequested);

        let entry = StatusJsonEntry::from_session(&session, "proj", AgentState::Idle, None);
        let output = format_status_human(&entry);

        assert!(output.contains("#42"));
        assert!(output.contains("https://example.com/pull/42"));
        assert!(output.contains("bug"));
        assert!(output.contains("alice"));
        assert!(output.contains("Changes requested"));
    }

    #[test]
    fn human_format_omits_pr_when_absent() {
        let session = make_session("no-pr");
        let entry = StatusJsonEntry::from_session(&session, "proj", AgentState::Idle, None);
        let output = format_status_human(&entry);

        assert!(!output.contains("PR:"));
        assert!(!output.contains("Review:"));
        assert!(!output.contains("Labels:"));
    }

    #[test]
    fn delete_guard_force_always_proceeds() {
        assert_eq!(delete_guard(true, true), DeleteGuard::Proceed);
        assert_eq!(delete_guard(true, false), DeleteGuard::Proceed);
    }

    #[test]
    fn delete_guard_interactive_prompts() {
        assert_eq!(delete_guard(false, true), DeleteGuard::Prompt);
    }

    #[test]
    fn delete_guard_non_tty_requires_force() {
        assert_eq!(delete_guard(false, false), DeleteGuard::RequireForce);
    }

    #[test]
    fn parse_yes_no_accepts_yes_variants() {
        for input in ["y", "Y", "yes", "YES", " yes\n", "  y  "] {
            assert!(parse_yes_no(input), "expected {input:?} to be yes");
        }
    }

    #[test]
    fn parse_yes_no_rejects_everything_else() {
        for input in ["", "n", "no", "x", "yep", "\n"] {
            assert!(!parse_yes_no(input), "expected {input:?} to be no");
        }
    }

    // --- review ---

    const REVIEW_RAW: &str = concat!(
        "diff --git a/src/a.rs b/src/a.rs\n",
        "index 1111111..2222222 100644\n",
        "--- a/src/a.rs\n",
        "+++ b/src/a.rs\n",
        "@@ -1,3 +1,3 @@ fn main() {\n",
        " use std::fmt;\n",
        "-use std::io;\n",
        "+use std::io::Write;\n",
        " fn x() {}\n",
        "diff --git a/new.txt b/new.txt\n",
        "new file mode 100644\n",
        "index 0000000..3333333\n",
        "--- /dev/null\n",
        "+++ b/new.txt\n",
        "@@ -0,0 +1,1 @@\n",
        "+hello\n",
        "diff --git a/gone.txt b/gone.txt\n",
        "deleted file mode 100644\n",
        "index 4444444..0000000\n",
        "--- a/gone.txt\n",
        "+++ /dev/null\n",
        "@@ -1,1 +0,0 @@\n",
        "-bye\n",
    );

    fn review_diff() -> crate::git::ParsedDiff {
        crate::git::parse_unified_diff(REVIEW_RAW)
    }

    /// `review_diff()` as `peek_review` would return it, with `reviewed`.
    fn peek(reviewed: &[&str]) -> ReviewPeek {
        let diff = review_diff();
        ReviewPeek {
            hashes: claude_commander_viewmodel::review::DiffHashes::of(&diff),
            diff,
            reviewed: reviewed.iter().map(|p| p.to_string()).collect(),
        }
    }

    /// `review diff` prints text that parses back to the same model the review
    /// view shows — so an agent judging a file sees exactly what is marked.
    #[test]
    fn render_file_diff_round_trips_through_the_parser() {
        let diff = review_diff();
        assert_eq!(diff.files.len(), 3, "precondition: three files parsed");
        let rendered: String = diff.files.iter().map(render_file_diff).collect();
        assert_eq!(crate::git::parse_unified_diff(&rendered), diff);
        assert!(rendered.contains("@@ -1,3 +1,3 @@ fn main() {\n"));
        assert!(rendered.contains("--- /dev/null\n+++ b/new.txt\n"));
        assert!(rendered.contains("--- a/gone.txt\n+++ /dev/null\n"));
    }

    /// A rename keeps its `rename from`/`to` lines, so a pure rename (no
    /// hunks) doesn't print as an empty — "nothing changed" — diff.
    #[test]
    fn render_file_diff_keeps_renames_visible() {
        let diff = crate::git::parse_unified_diff(concat!(
            "diff --git a/old.rs b/new.rs\n",
            "similarity index 100%\n",
            "rename from old.rs\n",
            "rename to new.rs\n",
        ));
        assert_eq!(diff.files.len(), 1, "precondition: rename parsed");
        let rendered = render_file_diff(&diff.files[0]);
        assert!(
            rendered.contains("rename from old.rs\nrename to new.rs\n"),
            "{rendered}"
        );
        assert_eq!(crate::git::parse_unified_diff(&rendered), diff);
    }

    #[test]
    fn review_file_entries_carry_status_counts_and_reviewed_state() {
        let peek = peek(&["new.txt"]);
        let entries = review_file_entries(&peek, false);
        let summary: Vec<_> = entries
            .iter()
            .map(|e| (e.path.as_str(), e.status, e.added, e.removed, e.reviewed))
            .collect();
        assert_eq!(
            summary,
            [
                ("src/a.rs", FileStatus::Modified, 1, 1, false),
                ("new.txt", FileStatus::Added, 1, 0, true),
                ("gone.txt", FileStatus::Deleted, 0, 1, false),
            ]
        );
        assert_eq!(
            format_review_files_human(&entries),
            "[ ] M src/a.rs (+1 -1)\n[x] A new.txt (+1 -0)\n[ ] D gone.txt (+0 -1)\n1/3 files reviewed"
        );
        assert_eq!(
            review_file_entries(&peek, true)
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["src/a.rs", "gone.txt"],
            "--unreviewed drops reviewed files"
        );
    }

    #[test]
    fn select_review_files_filters_and_reports_unknown_paths() {
        let reviewed = peek(&["new.txt"]);
        let paths = |files: Vec<&FileDiff>| -> Vec<String> {
            files.iter().map(|f| f.display_path().to_string()).collect()
        };

        assert_eq!(
            paths(select_review_files(&reviewed, &[], true).unwrap()),
            ["src/a.rs", "gone.txt"],
            "--unreviewed drops reviewed files"
        );
        assert_eq!(
            paths(select_review_files(&reviewed, &["gone.txt".into()], false).unwrap()),
            ["gone.txt"]
        );
        let err = select_review_files(&peek(&[]), &["gone.txt".into(), "typo.rs".into()], false)
            .unwrap_err();
        assert!(
            matches!(&err, SessionError::FileSelectionRejected { missing, .. } if missing == &["typo.rs"]),
            "{err}"
        );
    }

    /// `review diff` takes the `path@hash` values `review files --json`
    /// produces, exactly as `review mark` does.
    #[test]
    fn select_review_files_accepts_path_at_hash() {
        let peek = peek(&[]);
        let at = format!("gone.txt@{}", peek.hashes.get("gone.txt").unwrap());
        let picked = select_review_files(&peek, &[at], false).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].display_path(), "gone.txt");
    }

    #[test]
    fn review_refusal_reports_expected_refusals_only() {
        let rejected = Error::Session(SessionError::FileSelectionRejected {
            missing: vec!["typo.rs".into()],
            changed: vec!["b.rs".into()],
        });
        let report = review_refusal(&rejected, "s", true).unwrap();
        assert!(
            report.contains("typo.rs") && report.contains("b.rs"),
            "{report}"
        );
        assert!(report.contains("Nothing was changed"), "{report}");
        // `review diff` changes nothing even when it succeeds, so it mustn't
        // claim to have held back.
        let report = review_refusal(&rejected, "s", false).unwrap();
        assert!(!report.contains("Nothing was changed"), "{report}");

        let real = Error::Session(SessionError::NotFound(crate::session::SessionId::new()));
        assert!(review_refusal(&real, "s", true).is_none());
    }

    #[test]
    fn set_reviewed_human_lists_changes_and_no_ops() {
        let outcome = SetReviewedOutcome {
            changed: vec!["a.rs".into()],
            unchanged: vec!["b.rs".into(), "c.rs".into()],
        };
        assert_eq!(
            format_set_reviewed_human(&outcome, true),
            "Marked reviewed: a.rs\n2 file(s) already reviewed: b.rs, c.rs"
        );
    }
}
