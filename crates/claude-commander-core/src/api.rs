//! Commander API — unified service layer for CLI and TUI consumers.

use crate::git::git_command;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Utc};
use claude_commander_protocol::github::{
    CloneJob, CloneJobId, CloneRequest, CloneSource, GithubRepo, validate_clone_url,
    validate_dest_name, validate_repo_slug,
};
use claude_commander_protocol::workspace::{
    SetWorkspacesRequest, WorkspaceRejection, validate_set_workspaces, validate_workspace_label,
};
use futures::StreamExt;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::agent::AgentKind;
use crate::backend::{RunLocalError, run_local};
use crate::comment::{
    ApplyOutcome, Comment, CommentStatus, CommentStore, SendDecision, compose_markdown,
    decide_send, reanchor_comments,
};
use crate::config::{AppState, Config, ConfigStore, ProgramEntry, StateStore};
use crate::error::{GitError, Result, SessionError};
use crate::git::{
    CloneJobs, CloneOutcome, ComposedDiff, GitBackend, ParsedDiff, PrCheckResult,
    clone_source_rejected, compose_review_diff, compute_branch_diff, diff_stat_summary,
    effective_pr_state, enrich_binary_sizes, is_gh_available, list_repos, list_worktrees_at,
    parse_unified_diff, prefer_remote_branch, read_base_blob, read_worktree_file, ref_exists_cli,
    run_clone,
};
use crate::reviewed::{FileSelection, ReviewedStore, SetReviewedOutcome};
use crate::session::workspace;
use crate::session::{
    AgentState, CascadeOutcome, ProjectId, ScanResult, SessionId, SessionLookup, SessionManager,
    SessionStatus, WorktreeSession, apply_assignment, clear_override_and_reassign,
    decide_branch_reconcile, find_session, find_session_exact, program_with_agent_flags,
};
use crate::telemetry::{ConfigSnapshot, EnvFingerprint, FrontendInfo, Telemetry};
use crate::term_caps::ColorMode;
use crate::tmux::{AgentStateDetector, StatusBarInfo, TmuxExecutor};
use claude_commander_protocol::api::ReviewedFileMark;
use claude_commander_viewmodel::review::DiffHashes;

/// High-level service that wraps `SessionManager`, state stores, and agent
/// detection into a single entry point. Both the CLI and TUI route through
/// this rather than wiring the pieces together independently.
///
/// Cheap to clone: every field is an `Arc` or itself `Arc`-backed, so a clone
/// is a shared handle to the same state — used to hand the service to
/// background tasks (e.g. the review-diff refresh).
#[derive(Clone)]
pub struct CommanderService {
    manager: SessionManager,
    store: Arc<StateStore>,
    config_store: Arc<ConfigStore>,
    comments: Arc<CommentStore>,
    reviewed: Arc<ReviewedStore>,
    telemetry: Telemetry,
    /// Bounded in-memory ledger of recent cascade / push-stack operations,
    /// surfaced through [`Self::snapshot`]. Capped at
    /// [`OPERATION_LEDGER_CAP`]; oldest entries are evicted.
    operations: Arc<std::sync::Mutex<std::collections::VecDeque<OperationStatus>>>,
    /// Monotonic id source for ledger entries (stable for the process lifetime).
    next_op_id: Arc<std::sync::atomic::AtomicU64>,
    /// Cached `gh --version` availability. Computed once (fork/exec is not free)
    /// and reused for every `snapshot`.
    gh_available: Arc<tokio::sync::OnceCell<bool>>,
    /// Shared agent-state detector with a short TTL cache, reused across
    /// non-`fresh` [`Self::agent_states`] calls so repeated polls don't
    /// re-capture every pane. A `fresh` call bypasses it with a zero-TTL
    /// detector instead.
    agent_detector: Arc<tokio::sync::Mutex<AgentStateDetector>>,
    /// Latest bulk agent-state snapshot, maintained by the background poll loop
    /// ([`Self::spawn_background_tasks`]). Once [`Self::agent_states_primed`] is
    /// set, non-`fresh` [`Self::agent_states`] reads serve this cache rather than
    /// re-detecting on demand, so every frontend (and a remote client polling the
    /// route) sees the loop's liveness.
    agent_states_cache: Arc<tokio::sync::RwLock<AgentStatesSnapshot>>,
    /// Whether the poll loop has populated [`Self::agent_states_cache`] at least
    /// once. Until then (and when no loop runs — CLI, tests) `agent_states`
    /// falls back to on-demand detection.
    agent_states_primed: Arc<std::sync::atomic::AtomicBool>,
    /// Most recent per-project background-pull status, maintained by the pull
    /// loop and surfaced in [`Snapshot::project_pull`].
    pull_status: Arc<std::sync::Mutex<BTreeMap<ProjectId, PullStatus>>>,
    /// Last PR-status fan-out time, for debouncing manual refresh bursts.
    last_pr_check: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Signals the PR-status loop to run an immediate check (manual refresh).
    pr_refresh: Arc<tokio::sync::Notify>,
    /// Idempotency guard for [`Self::spawn_background_tasks`]: the loops spawn
    /// once per service, even if both a local TUI and an embedded caller ask.
    background_started: Arc<std::sync::atomic::AtomicBool>,
    /// Short-TTL cache of the last `tmux -V` probe result, so per-client
    /// [`Self::snapshot`] polling (~2s cadence) doesn't fork a
    /// subprocess on every poll. See [`Self::cached_tmux_ok`].
    tmux_ok_cache: Arc<std::sync::Mutex<Option<(std::time::Instant, bool)>>>,
    /// Running and recently-finished repository clones, started by
    /// [`Self::start_clone`] and polled through [`Self::clone_job`]. In-memory
    /// only — see [`CloneJobs`] for why a clone cannot outlive its process.
    clone_jobs: CloneJobs,
}

/// Max entries kept in the operation ledger before the oldest are evicted.
const OPERATION_LEDGER_CAP: usize = 32;

/// TTL for the shared agent-state detector cache used by non-`fresh`
/// `agent_states` polls.
const AGENT_STATE_CACHE_TTL: Duration = Duration::from_millis(1000);

/// TTL for the [`CommanderService::snapshot`] tmux-availability cache.
/// Long enough that a 2s client poll reuses the last probe rather than forking
/// `tmux -V` every time, short enough that tmux coming up/going down surfaces
/// within a few seconds.
const TMUX_OK_CACHE_TTL: Duration = Duration::from_secs(5);

impl CommanderService {
    /// Construct the service. `frontend` identifies the embedding application
    /// (binary/GUI name + version) for telemetry attribution and is required —
    /// [`FrontendInfo::new`] panics if it is not properly populated.
    pub fn new(
        config_store: Arc<ConfigStore>,
        store: Arc<StateStore>,
        frontend: FrontendInfo,
    ) -> Self {
        let manager = SessionManager::new(
            config_store.clone(),
            store.clone(),
            ColorMode::detect().tmux_status_style(),
        );
        // Comments and reviewed marks live beside state.json under the same
        // data dir the `StateStore` resolved — *not* a freshly recomputed
        // `Config::data_dir()`. Routing through the store's path means a test
        // (or any caller) that injects a `TempDir`-backed `StateStore` keeps
        // these sibling stores off the real `~/.local/share`, preserving the
        // project's strict test-isolation rule.
        let data_dir = store.data_dir();
        let comments = Arc::new(CommentStore::new(data_dir.join("comments")));
        let reviewed = Arc::new(ReviewedStore::new(data_dir.join("reviewed")));
        let telemetry = init_telemetry(&config_store, &store, &frontend);
        // NB: the idle-hibernation loop is NOT started here. `new` is shared by
        // one-shot CLI commands (via `for_cli`), and a tokio runtime is always
        // present under `#[tokio::main]`, so starting it here would let any CLI
        // command trigger a hibernation pass. Long-lived frontends call
        // `start_hibernation_loop` explicitly after construction instead.
        let agent_detector = Arc::new(tokio::sync::Mutex::new(AgentStateDetector::new(
            manager.tmux.clone(),
            AGENT_STATE_CACHE_TTL,
        )));
        Self {
            manager,
            store,
            config_store,
            comments,
            reviewed,
            telemetry,
            operations: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_op_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            gh_available: Arc::new(tokio::sync::OnceCell::new()),
            agent_detector,
            agent_states_cache: Arc::new(tokio::sync::RwLock::new(AgentStatesSnapshot {
                states: BTreeMap::new(),
                commander_running: false,
            })),
            agent_states_primed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pull_status: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            last_pr_check: Arc::new(std::sync::Mutex::new(None)),
            pr_refresh: Arc::new(tokio::sync::Notify::new()),
            background_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tmux_ok_cache: Arc::new(std::sync::Mutex::new(None)),
            clone_jobs: CloneJobs::new(),
        }
    }

    /// Start the background idle-hibernation policy loop. Long-lived frontends
    /// (the TUI) call this once after construction; one-shot CLI paths do not,
    /// so a CLI command can never trigger a hibernation pass as a side effect.
    /// No-op unless `hibernate_enabled` is set, the check interval is non-zero,
    /// and a tokio runtime is present.
    pub fn start_hibernation_loop(&self) {
        self.manager.spawn_hibernation_loop(self.telemetry.clone());
    }

    pub fn for_cli(
        config: crate::config::Config,
        frontend: FrontendInfo,
    ) -> std::result::Result<Self, crate::Error> {
        let config_store = Arc::new(ConfigStore::new(config)?);
        // A corrupt state file must propagate, not default to an empty
        // state: `list` would report no sessions and `create` would persist
        // a duplicate project alongside the unreadable original.
        let app_state = AppState::load()?;
        let store = Arc::new(StateStore::new(app_state)?);
        Ok(Self::new(config_store, store, frontend))
    }

    /// Shared telemetry handle. Frontends and library code call
    /// `service.telemetry().feature("…")` to record usage; a no-op when
    /// telemetry is disabled.
    pub fn telemetry(&self) -> &Telemetry {
        &self.telemetry
    }

    pub fn session_manager(&self) -> &SessionManager {
        &self.manager
    }

    pub fn store(&self) -> &Arc<StateStore> {
        &self.store
    }

    // -- Config --

    /// Snapshot the current in-memory config.
    pub fn read_config(&self) -> Config {
        self.config_store.read().clone()
    }

    /// Overwrite the persisted config (updates mtime so the hot-reload watcher
    /// won't re-read our own write).
    pub fn update_config(&self, config: Config) -> Result<()> {
        self.config_store.mutate(|c| *c = config)
    }

    /// Rename a configured section, migrating every session that refers to it
    /// (by `current_section` or `section_override`) in the same operation.
    ///
    /// Renaming via a plain config write is *not* equivalent: sessions store
    /// their section by name, so the config write alone strands them — see
    /// [`crate::session::rename_section`]. Exposed as its own method so no
    /// caller can perform half of the rename.
    ///
    /// Returns `false` (leaving everything untouched) when the rename is a
    /// no-op or `new` isn't an available name (see
    /// [`section_name_available`](crate::session::section_name_available)), or
    /// when `old` isn't a configured section.
    pub async fn rename_section(&self, old: &str, new: &str) -> Result<bool> {
        let new = new.trim();
        if new == old {
            return Ok(false);
        }
        let renamed = self.config_store.mutate(|c| {
            let Some(idx) = c.sections.iter().position(|s| s.name == old) else {
                return false;
            };
            if !crate::session::section_name_available(new, &c.sections) {
                return false;
            }
            c.sections[idx].name = new.to_string();
            true
        })?;
        if !renamed {
            return Ok(false);
        }
        self.telemetry.feature("section.rename");
        let (old, new) = (old.to_string(), new.to_string());
        self.store
            .mutate(move |state| {
                for session in state.sessions.values_mut() {
                    crate::session::rename_section(session, &old, &new);
                }
            })
            .await?;
        Ok(true)
    }

    /// Replace the configured program list (the new-session picker options) and
    /// persist it. Exposed as its own method — and, for remotes, its own HTTP
    /// endpoint — so the picker list can be edited without opening up the general
    /// config-patch surface. An empty list is allowed (the picker then falls back
    /// to a synthesized `claude` entry).
    pub fn set_programs(&self, programs: Vec<ProgramEntry>) -> Result<()> {
        self.telemetry.feature("programs.set");
        self.config_store.mutate(|c| c.programs = programs)
    }

    // -- Workspaces --

    /// Replace the workspace definitions (order = display order), and — when
    /// given — Main's label and the startup choice. Never re-tags a
    /// project: [`Self::rename_workspace`] and [`Self::delete_workspace`] do
    /// that, because a replace cannot tell a rename from a delete + create.
    /// Validated against the protocol's rules first, so nothing is written for
    /// a refused list.
    ///
    /// `main: None` leaves Main's label alone, so names are checked against
    /// the label this server already stores — otherwise a replace that omits
    /// `main` (every create/reorder edit, and any older client) could
    /// define a workspace named like Main, which a rename is refused for.
    pub fn set_workspace_defs(&self, req: SetWorkspacesRequest) -> Result<()> {
        let touches_main = req.main.is_some();
        let checked = SetWorkspacesRequest {
            // Only the stored label takes part (it is not written back), and
            // a hand-edited invalid one must not block every edit.
            main: req.main.or_else(|| {
                self.read_config()
                    .main_workspace
                    .filter(|m| validate_workspace_label(&m.name).is_ok())
            }),
            ..req
        };
        let mut req = validate_set_workspaces(&checked).map_err(|e| {
            let name = match &e {
                WorkspaceRejection::Reserved { name }
                | WorkspaceRejection::Duplicate { name }
                | WorkspaceRejection::UnknownStartup { name } => name.clone(),
                _ => "workspaces".to_string(),
            };
            workspace::workspace_rejected(&name, e)
        })?;
        if !touches_main {
            // Validated against the stored label only; nothing to write back.
            req.main = None;
        }
        self.telemetry.feature("workspace.update_defs");
        self.config_store.mutate(|c| {
            c.workspaces = req.workspaces;
            if let Some(main) = req.main {
                c.main_workspace = Some(main);
            }
            if let Some(startup) = req.startup_workspace {
                c.startup_workspace = startup;
            }
        })
    }

    /// Rename workspace `from` to `to`: the definition, a startup pin on it,
    /// this host's local theme for it (`[workspace_themes]`), and every project
    /// tagged with it, in that order. Returns `false` when this host neither
    /// defines `from` nor tags a project with it — so a rename sent eagerly to
    /// every server is a harmless no-op on the ones that never had the
    /// workspace. The theme moves even then: it is local config, and the TUI
    /// keeps themes for workspaces that live only on a remote server.
    pub async fn rename_workspace(&self, from: &str, to: &str) -> Result<bool> {
        let renamed_def = self
            .config_store
            .mutate(|c| {
                let renamed = workspace::rename_workspace_def(c, from, to)?;
                // `to` is valid and unclaimed once the def rename accepted it.
                workspace::rename_workspace_theme(c, from, to.trim());
                Ok(renamed)
            })?
            .map_err(|e| workspace::workspace_rejected(to, e))?;
        let to = claude_commander_protocol::workspace::validate_workspace_name(to)
            .map_err(|e| workspace::workspace_rejected(to, e))?;
        let (from_owned, to_tag) = (from.to_string(), to.clone());
        let moved = self
            .store
            .mutate(move |state| workspace::retag_projects(state, &from_owned, Some(&to_tag)))
            .await?;
        if !renamed_def && moved == 0 {
            return Ok(false);
        }
        if !renamed_def {
            // Tags existed without a definition (lost to an older binary's
            // write); give the new name one so it keeps its place.
            self.config_store
                .mutate(|c| workspace::ensure_workspace_defined(c, &to))?;
        }
        self.telemetry.feature("workspace.rename");
        Ok(true)
    }

    /// Delete workspace `name`: drop its definition (a startup pin on it
    /// becomes Main) and its local theme, and move its projects to Main.
    /// Returns `false` when there was no definition or tag to delete (a theme
    /// alone is still dropped). Main itself has no name and cannot be deleted.
    pub async fn delete_workspace(&self, name: &str) -> Result<bool> {
        let removed_def = self.config_store.mutate(|c| {
            workspace::delete_workspace_theme(c, name);
            workspace::delete_workspace_def(c, name)
        })?;
        let owned = name.to_string();
        let moved = self
            .store
            .mutate(move |state| workspace::retag_projects(state, &owned, None))
            .await?;
        if !removed_def && moved == 0 {
            return Ok(false);
        }
        self.telemetry.feature("workspace.delete");
        Ok(true)
    }

    /// Move project `id` into `workspace` (`None` = Main). A name this host
    /// has no definition for gets one appended — the self-heal that lets a
    /// client move a project into a workspace it created on another server.
    pub async fn set_project_workspace(
        &self,
        id: &ProjectId,
        workspace: Option<String>,
    ) -> Result<()> {
        let tag = workspace::validate_tag(workspace.as_deref())?;
        let id = *id;
        let exists = self.store.read().await.projects.contains_key(&id);
        if !exists {
            return Err(SessionError::ProjectNotFound(id.to_string()).into());
        }
        self.telemetry.feature("workspace.move_project");
        if let Some(name) = &tag {
            self.config_store
                .mutate(|c| workspace::ensure_workspace_defined(c, name))?;
        }
        self.store
            .mutate(move |state| match state.projects.get_mut(&id) {
                Some(project) => {
                    project.workspace = tag;
                    Ok(())
                }
                None => Err(SessionError::ProjectNotFound(id.to_string()).into()),
            })
            .await?
    }

    /// Validate `workspace` for a project about to be registered and make sure
    /// it is defined, returning the tag to store.
    fn prepare_new_project_workspace(&self, workspace: Option<String>) -> Result<Option<String>> {
        let tag = workspace::validate_tag(workspace.as_deref())?;
        if let Some(name) = &tag {
            self.config_store
                .mutate(|c| workspace::ensure_workspace_defined(c, name))?;
        }
        Ok(tag)
    }

    /// Reload config from disk if the file changed since the last read.
    pub fn reload_config(&self) -> Result<bool> {
        self.config_store.reload_if_changed()
    }

    /// Whether a pending config change requires an app restart to take effect.
    pub fn restart_required(&self) -> bool {
        self.config_store.restart_required()
    }

    // -- Projects --

    /// Register a git repository as a project, tagged with `workspace`
    /// (`None` = Main) — how a new project lands in the caller's active
    /// workspace. An undefined workspace name is defined on the way.
    pub async fn add_project(
        &self,
        repo_path: PathBuf,
        workspace: Option<String>,
    ) -> Result<ProjectId> {
        self.telemetry.feature("project.add");
        let tag = self.prepare_new_project_workspace(workspace)?;
        self.manager.add_project(repo_path, tag).await
    }

    /// Scan a directory for git repositories and register them as projects,
    /// each tagged with `workspace` (`None` = Main) like [`Self::add_project`].
    /// Repositories that are already projects keep their tag.
    pub async fn scan_directory(
        &self,
        dir: &Path,
        workspace: Option<String>,
    ) -> Result<ScanResult> {
        self.telemetry.feature("project.scan_directory");
        let tag = self.prepare_new_project_workspace(workspace)?;
        self.manager.scan_directory(dir, tag).await
    }

    // -- Repository clone --

    /// Every GitHub repo the authenticated user can clone, for the repo picker.
    ///
    /// Failures propagate rather than becoming an empty list — see
    /// [`crate::git::list_repos`], which also owns the `gh` availability probe.
    /// The [`Self::gh_available`] cache is deliberately *not* consulted here: it
    /// is computed once per process, so a user who installs `gh` and reopens the
    /// picker would keep being told it is missing. This call is user-initiated and
    /// infrequent, so it can afford a fresh probe.
    ///
    /// Bounded by `Config::repo_list_timeout_secs`: `gh api --paginate` is
    /// open-ended, and an unbounded run leaked a `gh` per abandoned request.
    /// Overrunning it is [`GitError::RepoListTimedOut`](crate::error::GitError::RepoListTimedOut),
    /// distinct from a missing `gh`.
    pub async fn list_github_repos(&self) -> Result<Vec<GithubRepo>> {
        self.telemetry.feature("github.list_repos");
        let timeout = Duration::from_secs(self.config_store.read().repo_list_timeout_secs);
        list_repos(timeout).await
    }

    /// Start cloning a repository into the projects directory, returning the
    /// created job — poll it with [`Self::clone_job`] until its status leaves
    /// [`CloneStatus::Running`](claude_commander_protocol::github::CloneStatus::Running).
    ///
    /// **The whole job rather than just its id** because that is what both callers
    /// need: the server answers `POST /projects/clone` with a 202 carrying this
    /// body, and the [`CommanderBackend`](crate::backend::CommanderBackend) seam
    /// mirrors that shape so a TUI gets the id, the destination and the redacted
    /// label in one call whichever transport it is on. Returning the id alone
    /// would leave every caller doing the same follow-up read.
    ///
    /// **The status in the returned job is not a terminal status.** Every outcome
    /// — including the occupied destination this method detects before it returns
    /// — is reported through the job registry, and the spawned future has not been
    /// scheduled yet, so this reads `Running` essentially always. Use it for the
    /// id and the destination; take the *outcome* from [`Self::clone_job`].
    ///
    /// A repeat call for a destination already being cloned returns the *in-flight*
    /// job: `CloneJobs::spawn` dedupes by destination, so a double-submit joins the
    /// running clone rather than starting a second one.
    ///
    /// Returns `Err` for what is wrong with the *request* — an unusable source or
    /// destination name, or a projects directory that cannot be created (plus one
    /// unreachable internal case noted at the read below). Everything that can only
    /// go wrong once the clone is running is reported through the job, because a
    /// clone takes minutes and no caller can wait for it.
    ///
    /// Three things happen before a subprocess does:
    ///
    /// 1. **The source is validated** (`validate_repo_slug` for a
    ///    [`CloneSource::Github`] slug, `validate_clone_url` otherwise) — it
    ///    becomes an argv element, and a rejection message is redacted at
    ///    construction by [`clone_source_rejected`].
    /// 2. **The destination name is resolved and checked.** A caller-supplied
    ///    `dest_name` clears the same bar as a derived one
    ///    ([`validate_dest_name`]); otherwise a `..` override would escape the
    ///    projects directory the derived-name rules exist to protect.
    /// 3. **The destination is pre-flighted.** An occupied path short-circuits to
    ///    a terminal [`CloneStatus::DestinationExists`] job rather than starting a
    ///    doomed clone, carrying the `is_git_repo` flag that lets a frontend offer
    ///    "register the existing checkout" instead of only "pick another name".
    ///
    /// [`CloneStatus::DestinationExists`]: claude_commander_protocol::github::CloneStatus::DestinationExists
    pub async fn start_clone(&self, req: CloneRequest) -> Result<CloneJob> {
        let dir_name = clone_dir_name(&req)?;
        // Checked up front like the destination: `clone_then_register` would
        // only refuse it after the whole clone, then delete the checkout.
        workspace::validate_tag(req.workspace.as_deref())?;
        let config = self.read_config();
        let projects_dir = config.projects_dir()?;
        // `projects_dir()` only *resolves* a path (as `worktrees_dir()` does), so
        // the first clone on a fresh machine has to create it.
        std::fs::create_dir_all(&projects_dir).map_err(|e| {
            GitError::OperationFailed(format!(
                "could not create projects directory {}: {e}",
                projects_dir.display()
            ))
        })?;
        let dest = projects_dir.join(dir_name);
        self.telemetry.feature("clone_project");

        // The label is raw user input; `CloneJobs::spawn` redacts it. Never
        // logged here — the destination is safe to log, the source is not.
        let source_label = clone_source_label(&req.source).to_string();

        let id = if let Some(is_git_repo) = occupied_destination(&dest) {
            debug!("clone destination {} already exists", dest.display());
            let occupied = dest.clone();
            self.clone_jobs
                .spawn(dest, source_label, async move {
                    CloneOutcome::destination_exists(occupied, is_git_repo)
                })
                .await
        } else {
            let service = self.clone();
            let timeout = Duration::from_secs(config.clone_timeout_secs);
            let job_dest = dest.clone();
            self.clone_jobs
                .spawn(dest, source_label, async move {
                    clone_then_register(service, req.source, req.workspace, job_dest, timeout).await
                })
                .await
        };

        // Unreachable in practice: the registry only misses an id that was never
        // issued or has been pruned, and pruning is TTL-based on *finished* jobs,
        // so a job spawned a moment ago is still there. An error rather than an
        // `expect` because an internal failure is the honest answer if that ever
        // stops holding — the server renders it as a 500, the backend seam as
        // `BackendError::Local`.
        self.clone_job(id).await.ok_or_else(|| {
            GitError::OperationFailed(format!("clone job {id} vanished before it could be read"))
                .into()
        })
    }

    /// The clone job with this id, as a frontend polls it, or `None` once it has
    /// been pruned (or never existed).
    pub async fn clone_job(&self, id: CloneJobId) -> Option<CloneJob> {
        self.clone_jobs.get(id).await
    }

    /// Clear the paused cascade state without merging.
    pub async fn cascade_abandon(&self) -> Result<()> {
        self.manager.cascade_abandon().await
    }

    /// Status-bar summary for a running session.
    pub fn status_bar_info(&self, session: &WorktreeSession, state: &AppState) -> StatusBarInfo {
        self.manager.status_bar_info(session, state)
    }

    // -- Queries --

    pub async fn list_sessions(&self, include_stopped: bool) -> Result<Vec<SessionInfo>> {
        let state = self.store.read().await;
        Ok(build_session_info_list(&state, include_stopped))
    }

    pub async fn find_session(&self, query: &str) -> Result<Option<SessionInfo>> {
        let state = self.store.read().await;
        Ok(find_session_info(&state, query))
    }

    /// Resolve a session by an *exact* title or full ID, surfacing ambiguity
    /// rather than picking arbitrarily. Used by destructive commands where a
    /// loose prefix match could act on the wrong session.
    pub async fn find_session_exact(&self, query: &str) -> Result<SessionLookup<SessionInfo>> {
        let state = self.store.read().await;
        Ok(match find_session_exact(&state, query) {
            SessionLookup::Found(session) => {
                let project_name = state
                    .projects
                    .get(&session.project_id)
                    .map(|p| p.name.as_str())
                    .unwrap_or("unknown");
                SessionLookup::Found(session_info_from_session(session, project_name))
            }
            SessionLookup::NotFound => SessionLookup::NotFound,
            SessionLookup::Ambiguous(n) => SessionLookup::Ambiguous(n),
        })
    }

    pub async fn get_session_detail(
        &self,
        query: &str,
        lines: Option<usize>,
    ) -> Result<Option<SessionDetail>> {
        let (found, project_name) = {
            let state = self.store.read().await;
            let Some(session) = find_session(&state, query) else {
                return Ok(None);
            };
            let pname = state
                .projects
                .get(&session.project_id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "unknown".to_string());
            (session.clone(), pname)
        };

        let agent_state = if found.status.is_active() {
            let mut detector = AgentStateDetector::new(self.manager.tmux.clone(), Duration::ZERO);
            detector
                .detect(
                    AgentKind::from_program(&found.program),
                    &found.tmux_session_name,
                )
                .await
        } else {
            AgentState::Unknown
        };

        let diff_stat = if found.worktree_path.exists() {
            // Resolve the base exactly as the review view does — prefer the PR
            // target branch, then the captured fork point, then HEAD — and pass
            // it through the merge-base so the stat counts only this branch's
            // changes (not changes the base accrued since the fork). Keeps the
            // CLI diffstat consistent with the review diff.
            let base = ReviewBase::of(&found).git_ref(&found.worktree_path).await;
            let target = crate::git::diff_target(&found.worktree_path, &base).await;
            diff_stat_summary(&found.worktree_path, &target).await
        } else {
            None
        };

        let pane_content = if found.status.is_active() && lines.is_some() {
            let n = lines.map(clamp_log_lines);
            capture_pane(&self.manager.tmux, &found.tmux_session_name, n).await?
        } else {
            None
        };

        Ok(Some(SessionDetail {
            info: session_info_from_session(&found, &project_name),
            agent_state,
            diff_stat,
            pane_content,
        }))
    }

    pub async fn get_pane_content(
        &self,
        query: &str,
        lines: Option<usize>,
    ) -> Result<Option<String>> {
        let state = self.store.read().await;
        let Some(session) = find_session(&state, query) else {
            return Ok(None);
        };
        let tmux_name = session.tmux_session_name.clone();
        drop(state);

        let n = lines.map(clamp_log_lines);
        capture_pane(&self.manager.tmux, &tmux_name, n).await
    }

    /// Resolve a session query (full UUID, ID prefix, or exact title) to its
    /// tmux session name, or `None` if nothing matches. Used by the server's
    /// WebSocket attach handler to spawn the tmux attach bridge for the right
    /// session, reusing the same `find_session` matching the CLI/HTTP API use.
    pub async fn resolve_tmux_session(&self, query: &str) -> Result<Option<String>> {
        let state = self.store.read().await;
        Ok(find_session(&state, query).map(|s| s.tmux_session_name.clone()))
    }

    /// Resolve a session query to its **shell** pane's tmux session name,
    /// creating the paired shell session on demand (the `Ctrl+\` partner). The
    /// shell counterpart of [`Self::resolve_tmux_session`], mirroring
    /// [`crate::backend::LocalBackend::attach`]'s `AttachKind::Shell` arm so the
    /// server's WebSocket attach and the local backend resolve the shell pane
    /// through the same [`ensure_shell_session`](crate::session::SessionManager::ensure_shell_session).
    /// `None` when the query matches no session.
    pub async fn resolve_shell_tmux_session(&self, query: &str) -> Result<Option<String>> {
        let session_id = {
            let state = self.store.read().await;
            find_session(&state, query).map(|s| s.id)
        };
        match session_id {
            Some(id) => Ok(Some(self.manager.ensure_shell_session(&id).await?)),
            None => Ok(None),
        }
    }

    /// Resolve a session query to the tmux session name for the requested pane,
    /// **reviving** a dead tmux session and **stamping** `last_attached_at` — the
    /// exact preparation [`crate::backend::LocalBackend::attach`] performs before
    /// spawning its bridge. The server's WebSocket attach handler routes through
    /// this so a remote attach reaches parity with a local one: a session whose
    /// tmux died is recreated (agent resumed, status bar reconfigured) rather than
    /// failing with a raw error, and every attach updates MRU ordering.
    /// `None` when the query matches no session.
    pub async fn resolve_attach_session(
        &self,
        query: &str,
        kind: crate::backend::AttachKind,
    ) -> Result<Option<String>> {
        let session_id = {
            let state = self.store.read().await;
            find_session(&state, query).map(|s| s.id)
        };
        let Some(id) = session_id else {
            return Ok(None);
        };
        let tmux_name = match kind {
            crate::backend::AttachKind::Agent => self.manager.ensure_attachable(&id).await?,
            crate::backend::AttachKind::Shell => self.manager.ensure_shell_session(&id).await?,
        };
        self.mark_attached(&id).await?;
        Ok(Some(tmux_name))
    }

    /// Resolve a session by **tmux session name** (primary or shell pair) and
    /// prepare its agent pane for attach, exactly as
    /// [`Self::resolve_attach_session`] does for a title/id query: a dead tmux
    /// session is revived (agent resumed, status bar reconfigured) and
    /// `last_attached_at` is stamped for MRU ordering. Returns the primary
    /// tmux name to attach or switch to. Used by the in-session Ctrl+Space
    /// switcher, which reaches a local session by `tmux switch-client` and so
    /// never goes through the backend's `attach` (where that work usually
    /// happens).
    pub async fn ensure_attachable_by_tmux_name(&self, tmux_name: &str) -> Result<String> {
        let session_id = {
            let state = self.store.read().await;
            state
                .sessions
                .values()
                .find(|s| s.matches_tmux_name(tmux_name))
                .map(|s| s.id)
        };
        let id =
            session_id.ok_or_else(|| SessionError::TmuxSessionNotFound(tmux_name.to_string()))?;
        let name = self.manager.ensure_attachable(&id).await?;
        self.mark_attached(&id).await?;
        Ok(name)
    }

    /// Store a pasted image for a remote session and inject its file path into
    /// the session's Claude pane.
    ///
    /// The desktop TUI captures the *local* clipboard image on Ctrl+V during a
    /// remote attach and uploads the bytes here (via the server's
    /// `POST /api/sessions/{id}/paste-image` route). We validate + write them to
    /// a pruned temp file under the OS temp dir, then `send-keys -l` the absolute
    /// path into the agent pane so the Claude CLI — which accepts a plain-text
    /// image path in the prompt — picks it up. No Enter is sent: the user adds
    /// prompt text and submits. Returns the written path.
    ///
    /// Errors: [`SessionError::InvalidImage`] (bad/oversized content → 400),
    /// [`SessionError::TmuxSessionNotFound`] (no such session → 404).
    pub async fn paste_image(&self, query: &str, bytes: &[u8]) -> Result<PathBuf> {
        // Validate the bytes up front so junk/oversized input is a clean 400
        // regardless of whether the session exists (and before any disk write).
        claude_commander_protocol::paste::validate(bytes)?;

        let tmux_name = self
            .resolve_tmux_session(query)
            .await?
            .ok_or_else(|| SessionError::TmuxSessionNotFound(query.to_string()))?;

        // Store under the agent temp dir (see `Self::agent_temp_dir`), the same
        // base comment-apply briefs use.
        let store = crate::paste_image::PasteImageStore::new(&self.agent_temp_dir());
        let path = store.store(bytes)?;

        // Inject the path with `send-keys -l` (no Enter). Unlike automated
        // comment-apply, this is user-initiated (the operator pressed Ctrl+V) and
        // user-visible (they're attached and watching the pane), so it is *not*
        // gated on agent state via `decide_send`: if the agent happens to be at a
        // prompt, the user sees the path land and can correct it. The composed
        // text wraps the path in spaces and escapes any interior space; the path
        // is server-generated (UUID name, space-free temp dir) so it carries no
        // client-controlled content.
        let injected = crate::paste_image::compose_injection(&path);
        self.manager
            .tmux
            .send_keys_literal(&tmux_name, &injected)
            .await?;

        self.telemetry.feature("paste_image");
        debug!("pasted image for session {} -> {}", query, path.display());
        Ok(path)
    }

    /// Base directory for the temp files handed to the agent by path (pasted
    /// images, comment-apply briefs).
    ///
    /// The OS temp dir, not the data dir: it is space-free on every platform
    /// (macOS's data dir under `~/Library/Application Support/…` contains
    /// spaces, which the CLI would mis-parse in an unquoted injected path), and
    /// it is proven readable by the agent without a permission prompt. Tests
    /// override it via [`Config::agent_temp_dir`] to keep writes (and the paste
    /// store's prune) off the real `/tmp`, per the repo's test-isolation rule.
    fn agent_temp_dir(&self) -> PathBuf {
        self.read_config()
            .agent_temp_dir
            .unwrap_or_else(std::env::temp_dir)
    }

    pub async fn check_tmux(&self) -> Result<()> {
        self.manager.check_tmux().await
    }

    /// tmux availability with a short-TTL cache ([`TMUX_OK_CACHE_TTL`]) so a
    /// per-client `snapshot` poll doesn't fork `tmux -V` every call.
    /// The lock is never held across the probe `.await`; two concurrent stale
    /// callers may both re-probe once (a benign, self-healing race).
    async fn cached_tmux_ok(&self) -> bool {
        if let Some((at, ok)) = *self.tmux_ok_cache.lock().expect("tmux_ok_cache poisoned")
            && at.elapsed() < TMUX_OK_CACHE_TTL
        {
            return ok;
        }
        let ok = self.check_tmux().await.is_ok();
        *self.tmux_ok_cache.lock().expect("tmux_ok_cache poisoned") =
            Some((std::time::Instant::now(), ok));
        ok
    }

    // -- Mutations --

    pub async fn create_session(&self, opts: CreateSessionOpts) -> Result<SessionId> {
        self.telemetry.feature("session.create");
        self.manager.check_tmux().await?;

        let base_program = opts
            .program
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| self.config_store.read().default_session_program());

        validate_program_flags(&opts, &base_program)?;

        let program = program_with_agent_flags(
            &base_program,
            opts.mode.as_deref(),
            opts.effort.as_deref(),
            opts.model.as_deref(),
        );

        let path = {
            let backend = GitBackend::discover(&opts.project_path)?;
            backend.path().to_path_buf()
        };

        // A session on an unregistered repo registers it in Main; a caller that
        // wants it elsewhere ensures the project with a workspace first (the
        // CLI's `new -d <path> --workspace`).
        let project_id = self.ensure_project(path, None).await?;

        let session_id = self
            .manager
            .prepare_session(&project_id, opts.title, Some(program), None)
            .await?;

        if let Some(section) = &opts.section {
            let section = section.clone();
            let sections =
                crate::session::effective_sections(&self.config_store.read().sections).into_owned();
            let now = chrono::Utc::now();
            self.store
                .mutate(move |state| {
                    if let Some(session) = state.sessions.get_mut(&session_id) {
                        crate::session::place_created_session(session, &section, &sections, now);
                    }
                })
                .await?;
        }

        // Local stack-parent hint (set by the "new stacked session" flow):
        // `finalize_session` reads it to fork the branch from the parent's
        // branch and inject the PR-base launch context.
        if let Some(parent) = opts.stack_parent {
            self.store
                .mutate(move |state| {
                    if let Some(session) = state.sessions.get_mut(&session_id) {
                        session.stack_parent_session_id = Some(parent);
                    }
                })
                .await?;
        }

        let result = async {
            self.manager
                .link_stack_parent_by_branch(&session_id, opts.base_branch.as_deref())
                .await?;
            self.manager
                .finalize_session(&session_id, opts.initial_prompt, opts.base_branch)
                .await?;
            Ok::<(), crate::Error>(())
        }
        .await;

        if let Err(e) = result {
            let _ = self.manager.remove_creating_session(&session_id).await;
            return Err(e);
        }

        // When smudging was skipped, the worktree holds LFS pointer files. The
        // TUI pulls the real content in the background, but a CLI invocation
        // exits right after this call, so pull synchronously here (best-effort)
        // to leave a usable worktree behind.
        if self.config_store.read().skip_lfs_smudge {
            let worktree_path = {
                let state = self.store.read().await;
                state
                    .get_session(&session_id)
                    .map(|s| s.worktree_path.clone())
            };
            if let Some(worktree_path) = worktree_path
                && let Err(e) = crate::git::lfs::pull(&worktree_path).await
            {
                tracing::warn!(error = %e, "git lfs pull after session create failed");
            }
        }

        Ok(session_id)
    }

    /// One-time startup reconciliation, run once when a frontend attaches (and
    /// on the server at boot). Bundles the checks the TUI used to run inline so
    /// every frontend — and the server — reconciles identically:
    ///
    /// 1. Drop sessions stuck in `Creating` from a previous run.
    /// 2. Reset transient `Merging`/`Pushing` states to `Running` (a died
    ///    mid-op process leaves stale spinners; `CascadePaused` is left alone).
    /// 3. Mark sessions whose tmux session/pane is gone as `Stopped`, and sync
    ///    each project's unmanaged worktrees.
    /// 4. Re-run section assignment over every session against current config.
    pub async fn startup_reconcile(&self) -> Result<()> {
        // 1. Stale Creating sessions.
        let creating_ids: Vec<SessionId> = {
            let state = self.store.read().await;
            state
                .sessions
                .values()
                .filter(|s| s.status == SessionStatus::Creating)
                .map(|s| s.id)
                .collect()
        };
        if !creating_ids.is_empty() {
            warn!(
                "Cleaning up {} stale Creating session(s) from previous run",
                creating_ids.len()
            );
            self.store
                .mutate(move |state| {
                    for sid in &creating_ids {
                        state.remove_session(sid);
                    }
                })
                .await?;
        }

        // 2. Stale Merging/Pushing sessions.
        let stale_ids: Vec<SessionId> = {
            let state = self.store.read().await;
            state
                .sessions
                .values()
                .filter(|s| matches!(s.status, SessionStatus::Merging | SessionStatus::Pushing))
                .map(|s| s.id)
                .collect()
        };
        if !stale_ids.is_empty() {
            warn!(
                "Resetting {} stale Merging/Pushing session(s) to Running",
                stale_ids.len()
            );
            self.store
                .mutate(move |state| {
                    for sid in &stale_ids {
                        if let Some(session) = state.get_session_mut(sid) {
                            session.set_status(SessionStatus::Running);
                        }
                    }
                })
                .await?;
        }

        // 3. Sync session status against live tmux, then sync worktrees.
        // Stopped sessions are probed too: the sync runs both ways, so a live
        // pane whose session was wrongly flagged stopped comes back (see
        // [`reconciled_status`]).
        let session_ids: Vec<(SessionId, String, SessionStatus)> = {
            let state = self.store.read().await;
            state
                .sessions
                .values()
                .filter(|s| s.status != SessionStatus::Creating)
                .map(|s| (s.id, s.tmux_session_name.clone(), s.status))
                .collect()
        };
        let probed: Vec<(SessionId, String, SessionStatus, PaneProbe)> =
            futures::stream::iter(session_ids.into_iter().map(
                |(id, tmux_name, status)| async move {
                    let probe = self.probe_pane(&tmux_name).await;
                    (id, tmux_name, status, probe)
                },
            ))
            .buffer_unordered(PANE_PROBE_CONCURRENCY)
            .collect()
            .await;
        for (session_id, tmux_name, status, probe) in probed {
            let Some(next) = reconciled_status(status, probe) else {
                continue;
            };
            if next == SessionStatus::Stopped {
                // A dead pane leaves its tmux session behind; drop it so the
                // next attach recreates the session cleanly.
                let _ = self.manager.tmux.kill_session(&tmux_name).await;
            } else {
                info!("Reviving session {session_id}: its tmux pane is live");
            }
            self.store
                .mutate(move |state| {
                    if let Some(session) = state.get_session_mut(&session_id) {
                        session.set_status(next);
                        if next == SessionStatus::Running {
                            // A live pane holds the agent's conversation, so
                            // there is nothing to resume into — upholds the
                            // "live pane ⇒ not hibernated" invariant.
                            session.hibernated = false;
                        }
                    }
                })
                .await?;
        }
        let project_ids: Vec<ProjectId> = {
            let state = self.store.read().await;
            state.projects.keys().copied().collect()
        };
        for project_id in project_ids {
            if let Err(e) = self.manager.sync_worktrees(&project_id).await {
                debug!("Failed to sync worktrees for project {}: {}", project_id, e);
            }
        }

        // 3b. Adopt any branch renames so PR polling, cascade-merge, and PR
        // retargeting key off the live branch name rather than a stale one.
        if let Err(e) = self.reconcile_session_branches().await {
            debug!("Failed to reconcile session branches: {e}");
        }

        // 4. Reconcile section assignments against current config.
        self.reconcile_all_section_assignments().await?;

        Ok(())
    }

    /// Re-run section assignment over every session against the current
    /// `[[sections]]` config. Used at startup and after a live config change.
    /// With no `[[sections]]` configured, the baked-in default board sections
    /// apply (see [`crate::session::effective_sections`]).
    pub async fn reconcile_all_section_assignments(&self) -> Result<()> {
        let sections =
            crate::session::effective_sections(&self.config_store.read().sections).into_owned();
        let now = chrono::Utc::now();
        self.store
            .mutate(move |state| {
                for session in state.sessions.values_mut() {
                    crate::session::apply_assignment(session, &sections, now);
                }
            })
            .await?;
        Ok(())
    }

    /// Re-run section assignment for a single session against current config.
    /// Used after creating a session, where the rest of the set is already
    /// reconciled. With no `[[sections]]` configured, the baked-in default
    /// board sections apply.
    pub async fn reconcile_one_section_assignment(&self, session_id: SessionId) -> Result<()> {
        let sections =
            crate::session::effective_sections(&self.config_store.read().sections).into_owned();
        let now = chrono::Utc::now();
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&session_id) {
                    crate::session::apply_assignment(session, &sections, now);
                }
            })
            .await?;
        Ok(())
    }

    /// Reconcile each session's stored `branch` with its worktree's live git
    /// HEAD, adopting the live name only when the stored branch was **renamed**
    /// away (see [`decide_branch_reconcile`] for the exact rule).
    ///
    /// A stale `session.branch` silently breaks PR polling (`gh pr list --head`
    /// queries a branch that no longer has a PR, so PR + stack topology vanish),
    /// cascade-merge (merges by branch name), and delete-time PR retargeting.
    /// Running this at the top of every PR-poll tick keeps the one stored name
    /// authoritative.
    ///
    /// Subprocess-only (no gix repository held across an `.await`) so the future
    /// stays `Send` for the spawned PR-status loop. Idempotent: a pass with no
    /// renames performs no writes.
    pub async fn reconcile_session_branches(&self) -> Result<()> {
        // Snapshot each project's repo path + main branch under the read lock,
        // plus its reconcile candidates (non-Creating sessions) and the branch
        // names of *all* its sessions (the collision set — a rename must not be
        // adopted onto a name any session, including a Creating one whose branch
        // is reserved, already holds).
        struct Cand {
            id: SessionId,
            worktree_path: PathBuf,
            stored: String,
        }
        struct ProjectSnapshot {
            repo_path: PathBuf,
            main_branch: String,
            cands: Vec<Cand>,
            all_branches: Vec<(SessionId, String)>,
        }
        let projects: Vec<ProjectSnapshot> = {
            let state = self.store.read().await;
            state
                .projects
                .values()
                .map(|project| {
                    let sessions = || {
                        project
                            .worktrees
                            .iter()
                            .filter_map(|sid| state.get_session(sid))
                    };
                    let cands = sessions()
                        .filter(|s| s.status != SessionStatus::Creating)
                        .map(|s| Cand {
                            id: s.id,
                            worktree_path: s.worktree_path.clone(),
                            stored: s.branch.clone(),
                        })
                        .collect();
                    let all_branches = sessions().map(|s| (s.id, s.branch.clone())).collect();
                    ProjectSnapshot {
                        repo_path: project.repo_path.clone(),
                        main_branch: project.main_branch.clone(),
                        cands,
                        all_branches,
                    }
                })
                .collect()
        };

        // Off-lock: per project, map each worktree's canonical path to its live
        // branch, then decide per session. Each update carries the expected old
        // branch so the write is a compare-and-set (below).
        let mut updates: Vec<(SessionId, String, String)> = Vec::new();
        for ProjectSnapshot {
            repo_path,
            main_branch,
            cands,
            all_branches,
        } in &projects
        {
            if cands.is_empty() {
                continue;
            }
            let worktrees = match list_worktrees_at(repo_path).await {
                Ok(w) => w,
                Err(e) => {
                    debug!("branch reconcile: list_worktrees_at({repo_path:?}) failed: {e}");
                    continue;
                }
            };
            // Canonicalize worktree paths so symlinked/relative session paths
            // match git's absolute output.
            let mut live_by_path: HashMap<PathBuf, String> = HashMap::new();
            for wt in &worktrees {
                let key = tokio::fs::canonicalize(&wt.path)
                    .await
                    .unwrap_or_else(|_| wt.path.clone());
                live_by_path.insert(key, wt.branch.clone());
            }

            for cand in cands {
                let canonical = tokio::fs::canonicalize(&cand.worktree_path)
                    .await
                    .unwrap_or_else(|_| cand.worktree_path.clone());
                let live = live_by_path.get(&canonical).map(String::as_str);
                // Only pay for ref-existence checks once there is a real
                // candidate (live branch present, differs, not detached).
                if !matches!(live, Some(b) if b != cand.stored && b != "HEAD") {
                    continue;
                }
                let local = ref_exists_cli(repo_path, &format!("refs/heads/{}", cand.stored)).await;
                let remote =
                    ref_exists_cli(repo_path, &format!("refs/remotes/origin/{}", cand.stored))
                        .await;
                // Collision set: every *other* session's branch in this project
                // (including Creating sessions, whose branch names are reserved).
                let siblings: Vec<&str> = all_branches
                    .iter()
                    .filter(|(id, _)| *id != cand.id)
                    .map(|(_, b)| b.as_str())
                    .collect();
                if let Some(new_branch) = decide_branch_reconcile(
                    &cand.stored,
                    live,
                    local,
                    remote,
                    main_branch,
                    &siblings,
                ) {
                    debug!(
                        "branch reconcile: session {} '{}' -> '{}'",
                        cand.id, cand.stored, new_branch
                    );
                    updates.push((cand.id, cand.stored.clone(), new_branch));
                }
            }
        }

        if updates.is_empty() {
            return Ok(());
        }
        let adopted_at = Utc::now();
        self.store
            .mutate(move |state| {
                for (id, expected_old, new_branch) in updates {
                    if let Some(session) = state.get_session_mut(&id) {
                        // Compare-and-set: only adopt if the branch still matches
                        // what we based the decision on, so a concurrent writer
                        // between snapshot and mutate is never clobbered.
                        if session.branch == expected_old {
                            session.branch = new_branch;
                            // Stamp the adoption so the PR poll doesn't credit
                            // this session with a PR that settled on the new
                            // name before it held it.
                            session.branch_adopted_at = Some(adopted_at);
                        }
                    }
                }
            })
            .await?;
        Ok(())
    }

    /// Persist a batch of PR-check results (from the background PR poll),
    /// re-run section assignment, then push refreshed status bars to running
    /// sessions' tmux panes. `Found` sets the cached PR fields, `NotFound`
    /// authoritatively clears them, `FetchFailed` preserves cached state so a
    /// transient error doesn't flatten a PR stack in the UI.
    pub async fn apply_pr_results(&self, results: Vec<(SessionId, PrCheckResult)>) -> Result<()> {
        let sections =
            crate::session::effective_sections(&self.config_store.read().sections).into_owned();
        let now = chrono::Utc::now();
        self.store
            .mutate(move |state| {
                for (session_id, result) in &results {
                    let Some(session) = state.get_session_mut(session_id) else {
                        continue;
                    };
                    match result {
                        PrCheckResult::Found(info) => {
                            session.pr_number = Some(info.number);
                            session.pr_url = Some(info.url.clone());
                            session.pr_state = Some(info.state);
                            session.pr_draft = info.is_draft;
                            session.pr_labels = info.labels.clone();
                            session.pr_merged = info.merged();
                            session.review_decision = info.review_decision;
                            session.pr_reviewers = info.reviewers.clone();
                            session.pr_base_branch = info.base_ref_name.clone();
                        }
                        PrCheckResult::NotFound => {
                            session.pr_number = None;
                            session.pr_url = None;
                            session.pr_state = None;
                            session.pr_draft = false;
                            session.pr_labels.clear();
                            session.pr_merged = false;
                            session.review_decision = None;
                            session.pr_reviewers.clear();
                            session.pr_base_branch = None;
                        }
                        PrCheckResult::FetchFailed => {}
                    }
                }
                for session in state.sessions.values_mut() {
                    crate::session::apply_assignment(session, &sections, now);
                }
            })
            .await?;

        // Push refreshed status bars to running sessions' tmux panes. Snapshot
        // under the lock, then release before the async tmux I/O.
        let status_bar_updates: Vec<_> = {
            let state = self.store.read().await;
            state
                .sessions
                .values()
                .filter(|s| s.status == SessionStatus::Running)
                .map(|s| (s.tmux_session_name.clone(), self.status_bar_info(s, &state)))
                .collect()
        };
        for (tmux_name, info) in &status_bar_updates {
            self.manager
                .tmux
                .configure_status_bar(tmux_name, info)
                .await;
        }
        Ok(())
    }

    /// Mark a batch of sessions unread (agent-finished transitions detected by
    /// the poll loop). Paired with [`Self::mark_read`].
    pub async fn mark_unread(&self, ids: Vec<SessionId>) -> Result<()> {
        self.store
            .mutate(move |state| {
                for id in &ids {
                    if let Some(session) = state.get_session_mut(id) {
                        session.unread = true;
                    }
                }
            })
            .await
    }

    /// Register `path` as a project, or answer with the id of the project already
    /// registered for it.
    ///
    /// The idempotent counterpart to [`Self::add_project`], and what both
    /// frontends' "register this existing checkout" offer calls: the destination
    /// they hand over is frequently one that *is* already a project, and
    /// `add_project` would register it a second time.
    ///
    /// The lookup is by
    /// [`repo_identity`](crate::session::repo_identity), not by the
    /// caller's string — see there for why comparing raw paths silently
    /// duplicates.
    ///
    /// `workspace` tags the project only when this call registers it; an
    /// existing project keeps its workspace.
    pub async fn ensure_project(
        &self,
        path: PathBuf,
        workspace: Option<String>,
    ) -> Result<ProjectId> {
        let identity = crate::session::repo_identity(&path).await;
        let existing = {
            let state = self.store.read().await;
            state
                .projects
                .values()
                .find(|p| p.repo_path == identity)
                .map(|p| p.id)
        };
        match existing {
            Some(id) => Ok(id),
            None => {
                let tag = self.prepare_new_project_workspace(workspace)?;
                self.manager.add_project(path, tag).await
            }
        }
    }

    pub async fn kill_session(&self, id: &SessionId) -> Result<()> {
        self.manager.kill_session(id, false).await
    }

    pub async fn restart_session(&self, id: &SessionId) -> Result<()> {
        self.telemetry.feature("session.restart");
        self.manager.restart_session(id).await
    }

    /// Restart a session's tmux pane without `--resume` (a fresh agent
    /// conversation). Used by the attach loop when the agent process exits, so
    /// the user seamlessly gets a new session rather than being dropped to the
    /// tree.
    pub async fn restart_session_fresh(&self, id: &SessionId) -> Result<()> {
        self.telemetry.feature("session.restart_fresh");
        self.manager.restart_session_fresh(id).await
    }

    pub async fn delete_session(&self, id: &SessionId) -> Result<()> {
        self.telemetry.feature("session.delete");
        self.manager.delete_session(id).await?;
        // Best-effort: the session is already gone, so a leftover marks file
        // is litter, not a failure to report.
        if let Err(e) = self.reviewed.remove(*id).await {
            tracing::warn!("Failed to remove reviewed marks for {id}: {e}");
        }
        Ok(())
    }

    /// Set a session's keep-alive flag (opt-out of auto-hibernation).
    pub async fn set_keep_alive(&self, id: &SessionId, keep_alive: bool) -> Result<bool> {
        self.telemetry.feature("session.set_keep_alive");
        self.manager.set_keep_alive(id, keep_alive).await
    }

    /// Toggle a session's keep-alive flag, returning the new value.
    pub async fn toggle_keep_alive(&self, id: &SessionId) -> Result<bool> {
        self.telemetry.feature("session.toggle_keep_alive");
        self.manager.toggle_keep_alive(id).await
    }

    // -- Review / comments --

    /// Open the review diff for a session: compose the base→working-tree diff,
    /// parse it, and re-anchor the session's stored comments against it
    /// (persisting any status changes). Returns the parsed diff plus the
    /// re-anchored comments.
    pub async fn open_review(&self, session_id: &SessionId) -> Result<ReviewSnapshot> {
        self.telemetry.feature("review.open");
        let (worktree_path, review_base) = self.review_target(session_id).await?;
        let base = review_base.git_ref(&worktree_path).await;
        let composed_at = Utc::now();
        let composed = compose_review_diff(&worktree_path, &base).await?;
        self.snapshot_from_raw(session_id, &worktree_path, base, composed, composed_at)
            .await
    }

    /// Re-compose the review diff and, when its content differs from
    /// `prev_hash`, return a fresh snapshot (re-anchoring comments and pruning
    /// reviewed marks against the new diff). Returns `None` when the working
    /// tree is unchanged, so the caller can skip the expensive parse/precompute
    /// and leave the open view untouched. Drives the review view's in-place
    /// refresh after the agent finishes a turn (or on manual request).
    pub async fn refresh_review_if_changed(
        &self,
        session_id: &SessionId,
        prev_hash: u64,
    ) -> Result<Option<ReviewSnapshot>> {
        let (worktree_path, review_base) = self.review_target(session_id).await?;
        let base = review_base.git_ref(&worktree_path).await;
        let composed_at = Utc::now();
        let composed = compose_review_diff(&worktree_path, &base).await?;
        if xxhash_rust::xxh3::xxh3_64(composed.raw.as_bytes()) == prev_hash {
            return Ok(None);
        }
        Ok(Some(
            self.snapshot_from_raw(session_id, &worktree_path, base, composed, composed_at)
                .await?,
        ))
    }

    /// Resolve the worktree path and review base for a session under a brief
    /// read lock. Shared by `open_review`, `refresh_review_if_changed` and
    /// `fetch_diff_blob`.
    async fn review_target(&self, session_id: &SessionId) -> Result<(PathBuf, ReviewBase)> {
        let state = self.store.read().await;
        let session = state
            .sessions
            .get(session_id)
            .ok_or(SessionError::NotFound(*session_id))?;
        Ok((session.worktree_path.clone(), ReviewBase::of(session)))
    }

    /// Build a [`ReviewSnapshot`] from an already-composed unified diff: hash it
    /// for staleness detection, parse it, then drop orphaned comments, re-anchor
    /// the rest, and prune stale reviewed marks against the parsed diff
    /// (persisting any changes). `composed_at` is when the composition began:
    /// marks set after it can't be judged against this diff.
    async fn snapshot_from_raw(
        &self,
        session_id: &SessionId,
        worktree_path: &Path,
        base: String,
        composed: ComposedDiff,
        composed_at: DateTime<Utc>,
    ) -> Result<ReviewSnapshot> {
        let content_hash = xxhash_rust::xxh3::xxh3_64(composed.raw.as_bytes());
        let mut diff = parse_unified_diff(&composed.raw);
        // Binary files carry metadata only; fill in the blob sizes the parser
        // can't know. Bytes are lazy-loaded via `fetch_diff_blob`.
        enrich_binary_sizes(&mut diff, worktree_path).await;

        // Deleting per-file state is only sound when the diff can actually prove
        // the file is gone; a degraded or incomplete composition omits files that
        // are still under review (see `absence_is_authoritative`).
        let can_prune = composed.absence_is_authoritative();

        let mut comments = self.comments.load(*session_id).await?;
        // A comment whose file left the diff has nothing left to anchor to and
        // no row to render on, so it is dropped rather than left blocking Apply
        // from somewhere the user can't reach.
        let dropped_comments = if can_prune {
            crate::comment::prune_orphaned(&mut comments, &diff)
        } else {
            Vec::new()
        };
        reanchor_comments(&mut comments, &diff);
        self.comments.save(*session_id, &comments).await?;

        // Reviewed marks pinned to a file's diff content: drop any whose file
        // changed or left the diff since they were set. Gated on the same proof
        // as the comments above — a mark's file "vanishing" from a diff that
        // simply omits it would silently throw away review progress (a stacked
        // base that only exists remotely degrades on *every* open, so this is a
        // persistent state, not a transient one).
        //
        // Read-modify-write under the store's lock, so a mark the CLI sets
        // meanwhile is neither overwritten nor (being newer than `composed_at`)
        // pruned against a diff that predates it.
        // The common open/refresh has nothing to prune, so it reads without
        // the lock and takes it only to prune (re-checking under it).
        let hashes = DiffHashes::of(&diff);
        let mut marks = self.reviewed.load(*session_id).await?;
        if can_prune && crate::reviewed::has_invalidated(&marks, &hashes, composed_at) {
            let hashes = hashes.clone();
            marks = self
                .reviewed
                .update(*session_id, move |marks| {
                    let pruned = crate::reviewed::prune_invalidated(marks, &hashes, composed_at);
                    (marks.clone(), pruned)
                })
                .await?;
        }
        // Only a mark pinned to the content shown reads as reviewed — on a
        // degraded diff too, where nothing is pruned but a mark for edited
        // content still mustn't show (so this view agrees with `review files`).
        // In store order, so the snapshot is deterministic.
        let reviewed = marks
            .into_iter()
            .filter(|m| hashes.holds(&m.file, &m.hash))
            .map(|m| m.file)
            .collect();

        Ok(ReviewSnapshot {
            base,
            diff,
            comments,
            reviewed,
            content_hash,
            dropped_comments,
            // Moved, not cloned: nothing below needs the composition again, and
            // a remote client re-parses this instead of the lossier `diff`.
            raw: Some(composed.raw),
        })
    }

    /// Fetch the raw bytes of one side of a binary file in a session's review
    /// diff. This is the lazy-load half of the binary-diff seam: bytes are kept
    /// OUT of `ReviewSnapshot` and fetched on demand only when a consumer needs
    /// to render (or compare) the image.
    ///
    /// - `DiffSide::New` reads the working-tree file.
    /// - `DiffSide::Old` reads the blob at the review base (errors for an added
    ///   file, which has no base side).
    pub async fn fetch_diff_blob(
        &self,
        session_id: &SessionId,
        side: DiffSide,
        path: &str,
    ) -> Result<Vec<u8>> {
        let (worktree_path, review_base) = self.review_target(session_id).await?;
        match side {
            DiffSide::New => read_worktree_file(&worktree_path, path).await,
            DiffSide::Old => {
                let base = review_base.git_ref(&worktree_path).await;
                read_base_blob(&worktree_path, &base, path).await
            }
        }
    }

    /// Toggle a file's reviewed mark by display path — only for a request
    /// that doesn't say which state it wants (a client predating
    /// [`ToggleReviewed::reviewed`]); every in-tree frontend uses
    /// [`Self::set_file_reviewed_by_path`]. Resolves the file in the
    /// **current** review diff and toggles against that. Keeps the wire API down
    /// to a path (no `FileDiff` echo for remote clients to cache) and makes it
    /// impossible to record a mark against a stale copy of the file — the hash
    /// always reflects the diff as it exists now. `FileNotInDiff` when the path
    /// isn't in the current diff.
    pub async fn toggle_file_reviewed_by_path(
        &self,
        session_id: &SessionId,
        display_path: &str,
    ) -> Result<bool> {
        self.telemetry.feature("review.toggle_reviewed");
        let target = self.current_mark_for(session_id, display_path).await?;
        self.reviewed
            .update(*session_id, move |marks| {
                (crate::reviewed::toggle(marks, &target), true)
            })
            .await
    }

    /// Set (`reviewed`) or clear one file's mark by display path, against the
    /// current diff — the direction-explicit counterpart of
    /// [`Self::toggle_file_reviewed_by_path`] for a view that knows which way
    /// the user meant (see [`ToggleReviewed::reviewed`]). Returns the new state.
    pub async fn set_file_reviewed_by_path(
        &self,
        session_id: &SessionId,
        display_path: &str,
        reviewed: bool,
    ) -> Result<bool> {
        // Same metric as the toggle: this is the TUI's `m`.
        self.telemetry.feature("review.toggle_reviewed");
        let target = self.current_mark_for(session_id, display_path).await?;
        self.reviewed
            .update(*session_id, move |marks| {
                (reviewed, crate::reviewed::set(marks, &target, reviewed))
            })
            .await
    }

    /// The mark pinning `display_path` to its content in the **current**
    /// review diff; `FileNotInDiff` when it isn't there.
    async fn current_mark_for(
        &self,
        session_id: &SessionId,
        display_path: &str,
    ) -> Result<ReviewedFileMark> {
        // Only the one file is hashed, not the whole diff.
        let diff = self.current_review_diff(session_id).await?;
        let file = diff
            .files
            .iter()
            .find(|f| f.display_path() == display_path)
            .ok_or_else(|| SessionError::FileNotInDiff(display_path.to_string()))?;
        Ok(ReviewedFileMark {
            file: display_path.to_string(),
            hash: claude_commander_viewmodel::review::hash_hex(file),
        })
    }

    /// Parse the session's review diff as it exists now, for operations that
    /// resolve files by path (so a mark is always pinned to current content).
    async fn current_review_diff(&self, session_id: &SessionId) -> Result<ParsedDiff> {
        let (worktree_path, review_base) = self.review_target(session_id).await?;
        let base = review_base.git_ref(&worktree_path).await;
        Ok(parse_unified_diff(
            &compose_review_diff(&worktree_path, &base).await?.raw,
        ))
    }

    /// Set (or clear) the reviewed mark on several files of a session's review
    /// diff at once, resolved against the current diff (see
    /// [`crate::reviewed::resolve_selection`]). Idempotent — the bulk
    /// counterpart of [`Self::toggle_file_reviewed_by_path`] for callers (the
    /// CLI, an agent) that must be safe to re-run. All-or-nothing: a path not
    /// in the diff, or one whose `@hash` no longer matches, fails the call
    /// (`FileSelectionRejected`, naming every such file) and nothing is
    /// written.
    pub async fn set_files_reviewed(
        &self,
        session_id: &SessionId,
        selection: &FileSelection,
        reviewed: bool,
    ) -> Result<SetReviewedOutcome> {
        self.telemetry.feature("review.set_reviewed");
        let diff = self.current_review_diff(session_id).await?;
        let hashes = DiffHashes::of(&diff);
        let targets: Vec<ReviewedFileMark> =
            crate::reviewed::resolve_selection(&diff, &hashes, selection)?
                .into_iter()
                .filter_map(|f| hashes.mark_for(f.display_path()))
                .collect();
        self.reviewed
            .update(*session_id, move |marks| {
                let mut outcome = SetReviewedOutcome::default();
                for target in targets {
                    if crate::reviewed::set(marks, &target, reviewed) {
                        outcome.changed.push(target.file);
                    } else {
                        outcome.unchanged.push(target.file);
                    }
                }
                let changed = !outcome.changed.is_empty();
                (outcome, changed)
            })
            .await
    }

    /// Read a session's review diff and which of its files are reviewed,
    /// **without** the side effects of [`Self::open_review`] (no comment
    /// re-anchoring or pruning, no store writes, no telemetry): the `review`
    /// CLI reads concurrently with an open frontend, and a stale whole-file
    /// save from a read would clobber that frontend's edits. Reviewed state is
    /// hash-checked against this diff (`live_paths`), so a file edited since it
    /// was marked reads as unreviewed even when pruning would be unsound.
    pub async fn peek_review(&self, session_id: &SessionId) -> Result<ReviewPeek> {
        // `current_review_diff` already checked the session exists.
        let diff = self.current_review_diff(session_id).await?;
        let hashes = DiffHashes::of(&diff);
        let marks = self.reviewed.load(*session_id).await?;
        let reviewed = claude_commander_viewmodel::review::live_paths(
            marks.iter().map(|m| (m.file.as_str(), m.hash.as_str())),
            &hashes,
        );
        Ok(ReviewPeek {
            diff,
            hashes,
            reviewed,
        })
    }

    /// A session's stored reviewed marks, unpruned. Cheap (one small file
    /// read, no compose), so an open review view can poll it to pick up marks
    /// set elsewhere; see `claude_commander_viewmodel::review::fold_marks` for the filtering a
    /// caller must apply. `NotFound` for an unknown session, so a poller can
    /// tell a deleted session from one with no marks.
    pub async fn reviewed_marks(&self, session_id: &SessionId) -> Result<Vec<ReviewedFileMark>> {
        self.review_target(session_id).await?;
        Ok(self
            .reviewed
            .load(*session_id)
            .await?
            .iter()
            .map(ReviewedFileMark::from)
            .collect())
    }

    /// List a session's stored comments (without re-anchoring).
    pub async fn list_comments(&self, session_id: &SessionId) -> Result<Vec<Comment>> {
        self.comments.load(*session_id).await
    }

    /// Session ids that have at least one not-yet-applied comment, for the
    /// session-list pending-comment indicator.
    pub async fn sessions_with_pending_comments(
        &self,
    ) -> Result<std::collections::HashSet<SessionId>> {
        self.comments.sessions_with_pending().await
    }

    /// Stage a new comment; returns its id.
    pub async fn create_comment(&self, session_id: &SessionId, draft: NewComment) -> Result<Uuid> {
        self.telemetry.feature("review.comment.create");
        let ann = Comment::new(
            draft.file,
            draft.side,
            draft.line_range,
            draft.snippet,
            draft.comment,
        );
        let id = ann.id;
        self.comments.add(*session_id, ann).await?;
        Ok(id)
    }

    /// Delete a staged comment by id (no-op if absent).
    pub async fn delete_comment(&self, session_id: &SessionId, id: Uuid) -> Result<()> {
        self.telemetry.feature("review.comment.delete");
        self.comments.delete(*session_id, id).await
    }

    /// Apply a session's staged comments: re-anchor them, and if none are
    /// drifted, compose a markdown brief to a temp file and inject a pointer
    /// prompt into the session's tmux pane for the agent to act on.
    ///
    /// Delivery is gated on agent state: sent immediately when idle/working
    /// (Claude queues natively), held until a permission prompt clears, and
    /// deferred if the agent is stopped or never becomes ready. Applied
    /// comments are marked [`CommentStatus::Applied`].
    pub async fn apply_comments(&self, session_id: &SessionId) -> Result<ApplyOutcome> {
        self.telemetry.feature("review.apply_comments");
        let (worktree_path, review_base, title, tmux_name, is_active, kind) = {
            let state = self.store.read().await;
            let s = state
                .sessions
                .get(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            (
                s.worktree_path.clone(),
                ReviewBase::of(s),
                s.title.clone(),
                s.tmux_session_name.clone(),
                s.status.is_active(),
                AgentKind::from_program(&s.program),
            )
        };

        // Re-anchor against a fresh diff so drift status is current.
        let base = review_base.git_ref(&worktree_path).await;
        let composed = compose_review_diff(&worktree_path, &base).await?;
        let parsed = parse_unified_diff(&composed.raw);
        let mut comments = self.comments.load(*session_id).await?;
        reanchor_comments(&mut comments, &parsed);
        self.comments.save(*session_id, &comments).await?;

        // A comment whose file left the diff since the view was composed takes no
        // part in Apply: there is no code left to point the agent at, and it must
        // not block the comments that *are* still live (the bug this replaced).
        //
        // Deliberately NOT deleted here. Deletion belongs to `snapshot_from_raw`,
        // which is the only path that reports what it dropped — orphaning implies
        // the diff moved, so the next compose is guaranteed to produce a snapshot
        // (its hash differs) and announce the drop. Deleting here instead would
        // leave the store already clean, so that notice would never fire and the
        // user's comment would vanish silently mid-Apply.
        let skip_orphans = composed.absence_is_authoritative();

        // Only not-yet-applied comments participate.
        let staged: Vec<Comment> = comments
            .iter()
            .filter(|a| a.status != CommentStatus::Applied)
            .filter(|a| !(skip_orphans && crate::comment::is_orphaned(a, &parsed)))
            .cloned()
            .collect();
        if staged.is_empty() {
            return Ok(ApplyOutcome::Nothing);
        }
        let drifted: Vec<Uuid> = staged
            .iter()
            .filter(|a| a.status == CommentStatus::Drifted)
            .map(|a| a.id)
            .collect();
        if !drifted.is_empty() {
            return Ok(ApplyOutcome::Blocked { drifted });
        }

        // Compose the brief to an absolute temp path outside the worktree.
        let path = write_apply_brief(
            &self.agent_temp_dir(),
            *session_id,
            &compose_markdown(&title, &staged),
        )
        .await?;
        let count = staged.len();

        if !is_active {
            return Ok(ApplyOutcome::Deferred { path, count });
        }

        // Gate delivery on agent state.
        let mut detector = AgentStateDetector::new(self.manager.tmux.clone(), Duration::ZERO);
        let ready = match decide_send(detector.detect(kind, &tmux_name).await) {
            SendDecision::Now => true,
            SendDecision::HoldUntilClear => wait_until_ready(&mut detector, kind, &tmux_name).await,
        };
        if !ready {
            return Ok(ApplyOutcome::Deferred { path, count });
        }

        // Inject the pointer prompt (literal text, then Enter to submit).
        let prompt = format!(
            "Review the comments in {} and address them.",
            path.display()
        );
        self.manager.tmux.send_keys(&tmux_name, &prompt).await?;
        // Some harnesses (Codex) fold an Enter that arrives in the same terminal
        // read as the preceding text into the paste instead of submitting it,
        // leaving the prompt unsent in the composer; wait for the harness to
        // drain the text so the Enter lands as its own keystroke. See
        // AgentKind::submit_key_delay.
        if let Some(delay) = kind.submit_key_delay() {
            tokio::time::sleep(delay).await;
        }
        self.manager.tmux.send_keys(&tmux_name, "Enter").await?;

        // Delivering a prompt flips an idle agent back to working without
        // attaching or changing status, so bump last_active_at: a concurrent
        // hibernation pass then sees a fresh stamp and won't kill the session we
        // just handed work to (its still_hibernatable re-check compares stamps).
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&sid) {
                    session.touch();
                }
            })
            .await?;

        // Mark the delivered comments applied.
        for ann in comments
            .iter_mut()
            .filter(|a| a.status != CommentStatus::Applied)
        {
            ann.status = CommentStatus::Applied;
        }
        self.comments.save(*session_id, &comments).await?;

        Ok(ApplyOutcome::Applied { path, count })
    }

    // -- Workspace / tree (additive: everything the session tree needs) --

    /// One snapshot of the whole workspace: projects, sessions (including
    /// stopped, so the full tree renders), cascade state, pending-comment
    /// indicators, the recent-operations ledger, and server health. This is the
    /// single query a remote client polls to build the session tree.
    ///
    /// `project_pull` reflects the background pull loop's latest per-project
    /// status ([`Self::spawn_background_tasks`]); it is empty until the loop has
    /// run (or when project auto-pull is disabled).
    pub async fn snapshot(&self) -> Result<Snapshot> {
        let gh_available = self.gh_available().await;
        let tmux_ok = self.cached_tmux_ok().await;
        let pending = self.sessions_with_pending_comments().await?;

        let (workspaces, main_workspace, startup_workspace) = {
            let config = self.config_store.read();
            (
                config.workspaces.clone(),
                config.main_workspace.clone(),
                config.startup_workspace.clone(),
            )
        };
        let (projects, sessions, cascade_paused) = {
            let state = self.store.read().await;
            (
                build_project_info_list(&state),
                build_session_info_list(&state, true),
                state.cascade_paused_at,
            )
        };

        let mut pending_comment_sessions: Vec<SessionId> = pending.into_iter().collect();
        pending_comment_sessions.sort();

        Ok(Snapshot {
            projects,
            sessions,
            cascade_paused,
            pending_comment_sessions,
            project_pull: self
                .pull_status
                .lock()
                .expect("pull status poisoned")
                .clone(),
            operations: self.operations_snapshot(),
            server: ServerStatus {
                gh_available,
                tmux_ok,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            workspaces,
            main_workspace,
            startup_workspace,
        })
    }

    /// List projects (sorted by name), for clients that only need the project
    /// set rather than a full [`Self::snapshot`].
    pub async fn list_projects(&self) -> Vec<ProjectInfo> {
        let state = self.store.read().await;
        build_project_info_list(&state)
    }

    /// Remove a project and all its sessions (kills their tmux sessions first).
    /// Wraps [`SessionManager::remove_project`].
    pub async fn remove_project(&self, id: &ProjectId) -> Result<()> {
        self.telemetry.feature("project.remove");
        self.manager.remove_project(id).await
    }

    /// List a project's git branches. When `fetch` is set, a best-effort
    /// `git fetch origin` runs first so newly-pushed remote branches appear.
    /// Mirrors the TUI checkout picker's source of truth
    /// ([`GitBackend::list_branches`]).
    pub async fn list_branches(
        &self,
        project_id: &ProjectId,
        fetch: bool,
    ) -> Result<Vec<BranchInfo>> {
        let repo_path = {
            let state = self.store.read().await;
            state
                .get_project(project_id)
                .ok_or_else(|| SessionError::ProjectNotFound(project_id.to_string()))?
                .repo_path
                .clone()
        };
        if fetch {
            // Best-effort — a failed fetch (offline, no remote) just means we
            // list whatever refs already exist.
            let _ = git_command()
                .current_dir(&repo_path)
                .args(["fetch", "origin"])
                .output()
                .await;
        }
        let backend = GitBackend::open(&repo_path)?;
        Ok(backend
            .list_branches()?
            .into_iter()
            .map(|(name, is_remote)| BranchInfo { name, is_remote })
            .collect())
    }

    /// Compute preview data (agent pane, diff + stat, shell pane) for a session
    /// or a project. Lifts the TUI's `fetch_preview_data`; the TUI keeps its own
    /// copy until Phase C deletes it.
    pub async fn preview(&self, target: PreviewTarget) -> Result<PreviewData> {
        match target {
            PreviewTarget::Session { id, lines } => self.session_preview(&id, lines).await,
            PreviewTarget::Project(id) => self.project_preview(&id).await,
        }
    }

    async fn session_preview(&self, sid: &SessionId, lines: Option<usize>) -> Result<PreviewData> {
        let (is_creating, tmux_name) = {
            let state = self.store.read().await;
            match state.get_session(sid) {
                Some(s) => (
                    s.status == crate::session::SessionStatus::Creating,
                    s.tmux_session_name.clone(),
                ),
                None => return Err(SessionError::NotFound(*sid).into()),
            }
        };
        // No tmux session exists yet while creating; short-circuit like the TUI.
        if is_creating {
            return Ok(PreviewData {
                pane: Some("Creating session...".to_string()),
                diff_text: String::new(),
                diff_stat: None,
                stats: None,
                shell: None,
            });
        }

        // An explicit line count captures the pane directly; otherwise use the
        // manager's cached content (matches the TUI preview).
        let pane = match lines {
            Some(n) => capture_pane(&self.manager.tmux, &tmux_name, Some(n)).await?,
            None => self.manager.get_content(sid).await.ok().map(|c| c.content),
        };
        let diff = self.manager.get_diff(sid).await.ok();
        let shell = self
            .manager
            .get_shell_content(sid)
            .await
            .ok()
            .flatten()
            .map(|c| c.content);
        Ok(PreviewData {
            pane,
            diff_text: diff.as_ref().map(|d| d.diff.clone()).unwrap_or_default(),
            diff_stat: diff.as_ref().map(|d| d.summary()),
            stats: diff.as_ref().map(|d| diff_stat_from_info(d)),
            shell,
        })
    }

    async fn project_preview(&self, pid: &ProjectId) -> Result<PreviewData> {
        let diff = self.manager.get_project_diff(pid).await.ok();
        let shell = self
            .manager
            .get_project_shell_content(pid)
            .await
            .ok()
            .flatten()
            .map(|c| c.content);
        Ok(PreviewData {
            pane: None,
            diff_text: diff.as_ref().map(|d| d.diff.clone()).unwrap_or_default(),
            diff_stat: diff.as_ref().map(|d| d.summary()),
            stats: diff.as_ref().map(|d| diff_stat_from_info(d)),
            shell,
        })
    }

    /// The full branch diff (committed vs `origin/<main>` plus uncommitted
    /// working changes) used for the AI summary. Wraps
    /// [`crate::git::compute_branch_diff`].
    pub async fn branch_diff(&self, session_id: &SessionId) -> Result<String> {
        let (worktree_path, main_branch) = {
            let state = self.store.read().await;
            let s = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            let project = state
                .get_project(&s.project_id)
                .ok_or_else(|| SessionError::ProjectNotFound(s.project_id.to_string()))?;
            (s.worktree_path.clone(), project.main_branch.clone())
        };
        Ok(compute_branch_diff(&worktree_path, &main_branch).await)
    }

    /// Rename a session's title. Rejects an empty (whitespace-only) title.
    pub async fn rename_session(&self, id: &SessionId, title: impl Into<String>) -> Result<()> {
        let title = title.into().trim().to_string();
        if title.is_empty() {
            return Err(SessionError::InvalidName {
                name: title,
                reason: "session name cannot be empty".to_string(),
            }
            .into());
        }
        self.ensure_session_exists(id).await?;
        self.telemetry.feature("session.rename");
        let id = *id;
        self.store
            .mutate(move |state| {
                if let Some(s) = state.get_session_mut(&id) {
                    s.title = title;
                }
            })
            .await
    }

    /// Change a session's launch program (which agent harness runs) and relaunch
    /// its pane so the new program takes effect. The stored `program` is the
    /// single source of truth for the harness ([`AgentKind`] is derived from it)
    /// and only applies at launch, so the pane is restarted *fresh* — a different
    /// harness cannot resume the previous one's conversation. Runs on the
    /// session's owning host (called directly by the local backend and by the
    /// server's PATCH handler for remote sessions).
    pub async fn change_program(&self, id: &SessionId, program: impl Into<String>) -> Result<()> {
        let program = program.into().trim().to_string();
        if program.is_empty() {
            return Err(SessionError::InvalidProgram("program cannot be empty".to_string()).into());
        }
        self.ensure_session_exists(id).await?;
        self.telemetry.feature("session.change_program");
        let sid = *id;
        let prog = program.clone();
        self.store
            .mutate(move |state| {
                if let Some(s) = state.get_session_mut(&sid) {
                    s.program = prog;
                }
            })
            .await?;
        // Relaunch so the newly-chosen agent actually runs. Fresh (no `--resume`):
        // the previous harness's conversation belongs to a different CLI and
        // can't be resumed by the new one.
        self.manager.restart_session_fresh(id).await
    }

    /// Move a session to a section (`Some(name)`) or clear its manual override
    /// and re-run predicate assignment (`None`). Mirrors the TUI's
    /// `apply_section_move`.
    pub async fn set_section(&self, id: &SessionId, section: Option<String>) -> Result<()> {
        self.ensure_session_exists(id).await?;
        self.telemetry.feature("session.set_section");
        let sections =
            crate::session::effective_sections(&self.config_store.read().sections).into_owned();
        let now = chrono::Utc::now();
        let id = *id;
        self.store
            .mutate(move |state| {
                if let Some(s) = state.get_session_mut(&id) {
                    match section {
                        Some(name) => {
                            s.section_override = Some(name);
                            apply_assignment(s, &sections, now);
                        }
                        None => {
                            clear_override_and_reassign(s, &sections, now);
                        }
                    }
                }
            })
            .await
    }

    /// Retarget a session's stack base: stack it onto `parent` (another session
    /// in the same project), or unstack it onto the project's main branch when
    /// `parent` is `None`.
    ///
    /// Metadata and PR base only — git history is deliberately untouched, so the
    /// branch still carries its old base's commits and the review diff/PR will
    /// show them until the user rebases or merges by hand.
    ///
    /// Validation and mutation happen inside one `try_mutate` closure
    /// ([`AppState::set_session_base`]) because the store re-reads state from
    /// disk under its lock: planning against an earlier snapshot could be stale
    /// by the time it applied, letting a concurrent change close a cycle the
    /// check had just rejected. A refused plan writes nothing.
    pub async fn set_session_base(
        &self,
        id: &SessionId,
        parent: Option<SessionId>,
    ) -> Result<SetSessionBaseOutcome> {
        self.telemetry.feature("session.set_base");
        let session_id = *id;
        let (plan, pr_retarget) = self
            .store
            .try_mutate(move |state| {
                state
                    .set_session_base(&session_id, parent)
                    // An unknown session id is a 404 everywhere else in the API;
                    // don't let it fall through the rejection's 400/409 mapping.
                    .map_err(|e| match e {
                        crate::session::SetBaseRejection::SessionNotFound => {
                            SessionError::NotFound(session_id).into()
                        }
                        other => crate::Error::from(other),
                    })
            })
            .await?;

        // Durable half, best-effort. The local mirror already re-stacked the
        // tree, but only this makes it survive the next PR sync — so a failure
        // is reported rather than logged.
        let pr = match pr_retarget {
            None => PrRetarget::NoPr,
            Some(r) => match crate::git::try_retarget_pr_base(
                &r.repo_path,
                r.pr_number,
                &r.new_base_branch,
            )
            .await
            {
                Ok(()) => PrRetarget::Retargeted {
                    pr_number: r.pr_number,
                },
                Err(message) => PrRetarget::Failed {
                    pr_number: r.pr_number,
                    message,
                },
            },
        };

        Ok(SetSessionBaseOutcome {
            new_base_branch: plan.new_base_branch,
            old_base_branch: plan.old_base_branch,
            pr,
        })
    }

    /// Stamp a session's `last_attached_at` to now, for MRU ordering in the
    /// in-session switcher. Called by the backend's `attach` so every frontend
    /// records the attach the same way (the TUI used to do this inline). No-op
    /// if the session doesn't exist — an attach to a vanished session is a
    /// harmless race, not an error worth surfacing here.
    pub async fn mark_attached(&self, id: &SessionId) -> Result<()> {
        let id = *id;
        self.store
            .mutate(move |state| {
                if let Some(s) = state.get_session_mut(&id) {
                    s.mark_attached();
                }
            })
            .await
    }

    /// Clear a session's unread flag (as the TUI does when attaching).
    pub async fn mark_read(&self, id: &SessionId) -> Result<()> {
        self.ensure_session_exists(id).await?;
        let id = *id;
        self.store
            .mutate(move |state| {
                if let Some(s) = state.get_session_mut(&id) {
                    s.unread = false;
                }
            })
            .await
    }

    /// New-session dialog options: the default program, the configured program
    /// list, and the configured section names.
    pub fn create_options(&self) -> CreateOptions {
        let config = self.config_store.read();
        CreateOptions {
            default_program: config.default_session_program(),
            programs: config.programs.iter().map(ProgramInfo::from).collect(),
            sections: crate::session::effective_sections(&config.sections)
                .iter()
                .map(|s| s.name.clone())
                .collect(),
        }
    }

    // -- Cascade / push-stack (record outcomes in the operation ledger) --

    /// Cascade-merge the stack containing `start_from`. Detects agent states
    /// itself (the server has no cached map like the TUI), runs the merge, and
    /// records the outcome in the ledger. Returns the recorded status.
    pub async fn cascade_merge(&self, start_from: &SessionId) -> Result<OperationStatus> {
        self.telemetry.feature("cascade.merge");
        let states = self.detect_active_states().await;
        let outcome = self.manager.cascade_merge_stack(start_from, &states).await;
        Ok(self.record_cascade_outcome(outcome))
    }

    /// Resume a paused cascade. Records the outcome in the ledger.
    pub async fn cascade_resume(&self) -> Result<OperationStatus> {
        self.telemetry.feature("cascade.resume");
        let states = self.detect_active_states().await;
        let outcome = self.manager.cascade_resume(&states).await;
        Ok(self.record_cascade_outcome(outcome))
    }

    /// Push every branch in the stack containing `start_from`. Records the
    /// outcome in the ledger.
    pub async fn push_stack(&self, start_from: &SessionId) -> Result<OperationStatus> {
        self.telemetry.feature("stack.push");
        let states = self.detect_active_states().await;
        let outcome = match self.manager.push_stack(start_from, &states).await {
            Ok(o) => OperationOutcome::Succeeded {
                detail: format!("{} pushed", o.sessions_pushed),
            },
            Err(e) => OperationOutcome::Failed {
                error: e.to_string(),
            },
        };
        Ok(self.record_operation(OperationKind::PushStack, outcome))
    }

    /// Bulk agent-state snapshot over active sessions.
    ///
    /// When the background poll loop ([`Self::spawn_background_tasks`]) is
    /// running, a non-`fresh` call serves its maintained cache (so the reported
    /// `commander_running` and states reflect the loop's liveness, and a remote
    /// client polling the route sees the same data the TUI does). Before the
    /// loop's first tick — or when no loop runs (CLI, tests) — it falls back to
    /// on-demand detection via the shared TTL-cached detector.
    ///
    /// `fresh` always bypasses caches with a zero-TTL detector, forcing a
    /// re-capture, and folds the result back into the shared cache so the loop's
    /// unread baseline advances too — this is what stops a session the operator
    /// just watched go idle during an attach from being re-flagged unread on the
    /// next poll tick.
    pub async fn agent_states(&self, fresh: bool) -> AgentStatesSnapshot {
        if fresh {
            // Recompute commander liveness the same way the poll loop does, so a
            // fresh read (and any non-fresh read served from the primed cache
            // afterwards, including a remote client's) reflects the commander's
            // real state rather than a stale flag left in the cache.
            let (commander_enabled, commander_program) = {
                let c = self.config_store.read();
                (c.commander_enabled, c.commander_program())
            };
            let commander_running =
                commander_enabled && crate::commander::is_running(&self.manager.tmux).await;
            // Include the commander's sentinel target when it's live so the
            // rebuilt cache carries its state too — otherwise the commander chip
            // blinks empty for one tick after an attach primes the cache.
            let active = with_commander_target(
                self.active_session_targets().await,
                commander_running,
                &commander_program,
            );
            let mut detector = AgentStateDetector::new(self.manager.tmux.clone(), Duration::ZERO);
            let states = detector.detect_all(&active).await;
            let mut cache = self.agent_states_cache.write().await;
            cache.states = states;
            cache.commander_running = commander_running;
            self.agent_states_primed
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return cache.clone();
        }
        if self
            .agent_states_primed
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return self.agent_states_cache.read().await.clone();
        }
        let active = self.active_session_targets().await;
        let states = {
            let mut detector = self.agent_detector.lock().await;
            detector.detect_all(&active).await
        };
        // Mirror the fresh/loop arms: never fabricate `commander_running`.
        // `is_running` short-circuits when the commander is disabled, so no
        // tmux probe happens in the common (disabled) case.
        let commander_enabled = self.config_store.read().commander_enabled;
        let commander_running =
            commander_enabled && crate::commander::is_running(&self.manager.tmux).await;
        AgentStatesSnapshot {
            states,
            commander_running,
        }
    }

    /// Drop any agent states left in the cache once the running-session set has
    /// gone empty (the last running session stopped), waking the change-feed so
    /// clients re-read an empty snapshot rather than being served ghost states.
    /// Idempotent: a call on an already-empty cache is a no-op (returns whether
    /// it cleared anything). Called from the agent-state loop's quiet path.
    async fn clear_stale_agent_states(&self) -> bool {
        let had_states = !self.agent_states_cache.read().await.states.is_empty();
        if had_states {
            self.agent_states_cache.write().await.states.clear();
            self.store.notify_change();
        }
        had_states
    }

    /// Request an immediate PR-metadata refresh. Wakes the background PR-status
    /// loop ([`Self::spawn_background_tasks`]); if no loop is running (PR checks
    /// disabled, or before startup) the notification is simply dropped.
    pub fn request_pr_refresh(&self) -> Result<()> {
        self.pr_refresh.notify_one();
        Ok(())
    }

    /// Spawn the service-owned background loops: agent-state polling, PR-status
    /// checks, project auto-pull, and cross-instance state-sync. Idempotent — a
    /// second call (e.g. a local TUI and an embedded caller sharing one service)
    /// is a no-op and returns empty handles.
    ///
    /// Every loop drives the same observable surface the frontends read: the
    /// agent loop maintains [`Self::agent_states`]' cache and persists unread
    /// transitions; the PR loop persists results via [`Self::apply_pr_results`];
    /// the pull loop feeds [`Snapshot::project_pull`]; the sync loop
    /// reloads the state file. Each wakes the [`StateStore`] change-feed on a
    /// real change (either via a persisted mutation or [`StateStore::notify_change`]),
    /// so a subscriber (the TUI's per-backend change-feed task) re-reads the
    /// relevant snapshot — no frontend-specific event plumbing crosses the
    /// backend seam.
    ///
    /// Intervals are read from config once here (matching the old TUI loops).
    /// The returned [`BackgroundHandles`] let tests abort the loops; production
    /// callers ignore them and let the tasks run for the process lifetime.
    pub fn spawn_background_tasks(&self, opts: BackgroundOpts) -> BackgroundHandles {
        // Idempotency: only the first caller spawns.
        if self
            .background_started
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return BackgroundHandles::default();
        }

        let config = self.config_store.read().clone();
        let handles = vec![
            self.spawn_agent_state_loop(
                config.agent_state_poll_interval_ms,
                opts.commander_enabled,
                config.commander_program(),
            ),
            self.spawn_pr_status_loop(config.pr_check_interval_secs),
            self.spawn_project_pull_loop(
                config.project_pull_enabled,
                config.project_pull_interval_secs,
            ),
            self.spawn_state_sync_loop(config.state_sync_interval_ms),
        ];
        BackgroundHandles { handles }
    }

    /// Poll every running session's (and the commander's) agent state on a fixed
    /// cadence, maintain [`Self::agent_states_cache`], persist Working→Idle
    /// transitions as unread, and wake the change-feed on any change. No-op loop
    /// when `interval_ms` is 0.
    fn spawn_agent_state_loop(
        &self,
        interval_ms: u64,
        commander_enabled: bool,
        commander_program: String,
    ) -> tokio::task::JoinHandle<()> {
        let service = self.clone();
        let tmux = self.manager.tmux.clone();
        let cache = self.agent_states_cache.clone();
        let primed = self.agent_states_primed.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            if interval_ms == 0 {
                return;
            }
            let cache_ttl = Duration::from_millis(interval_ms.saturating_sub(500).max(500));
            let mut detector = AgentStateDetector::new(tmux.clone(), cache_ttl);
            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
            let mut last_commander_running = false;
            let sentinel = claude_commander_protocol::session::COMMANDER_SENTINEL_ID;
            loop {
                interval.tick().await;
                let sessions: Vec<(SessionId, String, String)> = {
                    let state = store.read().await;
                    state
                        .sessions
                        .values()
                        .filter(|s| s.status == SessionStatus::Running)
                        .map(|s| (s.id, s.tmux_session_name.clone(), s.program.clone()))
                        .collect()
                };
                let commander_running =
                    commander_enabled && crate::commander::is_running(&tmux).await;
                let sessions =
                    with_commander_target(sessions, commander_running, &commander_program);
                // Quiet path: nothing to detect and the commander's state is
                // unchanged — skip the tick (no cache write, no wake). But if the
                // cache still holds states from a previous tick (the last running
                // session just stopped), clear it once and wake the feed so
                // clients — including remote pollers — aren't served ghost states.
                if poll_tick_can_skip(
                    sessions.is_empty(),
                    commander_running,
                    last_commander_running,
                ) {
                    service.clear_stale_agent_states().await;
                    continue;
                }
                let states: BTreeMap<SessionId, AgentState> = if sessions.is_empty() {
                    BTreeMap::new()
                } else {
                    detector.detect_all(&sessions).await
                };
                if !poll_tick_should_send(
                    states.is_empty(),
                    commander_running,
                    last_commander_running,
                ) {
                    continue;
                }
                let commander_flipped = commander_running != last_commander_running;
                last_commander_running = commander_running;

                // Diff against the previous cache to flag agents that just
                // finished a turn (Working→Idle), skipping the commander
                // sentinel (it has no `WorktreeSession` to mark).
                let prev = { cache.read().await.states.clone() };
                let unread_ids: Vec<SessionId> = detect_unread_transitions(&prev, &states)
                    .into_iter()
                    .filter(|id| *id != sentinel)
                    .collect();
                let states_changed = states != prev;

                // Only write the cache when something changed: a rebuilt-but-
                // identical map would serialize identically anyway (BTreeMap,
                // deterministic key order), but skipping the write keeps the
                // remote pollers' content-hash diffing honest by construction
                // and avoids needless lock traffic on all-idle ticks.
                {
                    let mut c = cache.write().await;
                    if states_changed {
                        c.states = states;
                    }
                    c.commander_running = commander_running;
                }
                primed.store(true, std::sync::atomic::Ordering::Relaxed);

                // Wake the change-feed only when observable state actually
                // changed (fresh states, or the commander flipped), so a steady
                // row of idle agents doesn't trigger a snapshot re-fetch every
                // tick. Persisting unread already bumps the feed; otherwise wake
                // it explicitly. The cache is updated first so the snapshot the
                // wake triggers reads the fresh states.
                if !unread_ids.is_empty() {
                    let _ = service.mark_unread(unread_ids).await;
                } else if states_changed || commander_flipped {
                    store.notify_change();
                }
            }
        })
    }

    /// The `(session id, branch, repo path, branch-owned-since)` tuples the PR
    /// poll fans out over: every non-`Creating` session paired with its project's
    /// repo path. Reads a snapshot under the lock so the poll's per-branch `gh`
    /// calls run off-lock. The timestamp is
    /// [`WorktreeSession::branch_owned_since`], which lets the check discard PRs
    /// that had already settled on this branch name before the session held it
    /// (see [`crate::git::check_pr_for_branch`]).
    ///
    /// Extracted from the poll loop so the reconcile→poll ordering is testable:
    /// after [`Self::reconcile_session_branches`] adopts a rename, the targets
    /// returned here carry the corrected `--head` branch on the same tick.
    async fn pr_poll_targets(&self) -> Vec<(SessionId, String, PathBuf, DateTime<Utc>)> {
        let state = self.store.read().await;
        state
            .sessions
            .values()
            .filter(|s| s.status != SessionStatus::Creating)
            .filter_map(|s| {
                let project = state.projects.get(&s.project_id)?;
                Some((
                    s.id,
                    s.branch.clone(),
                    project.repo_path.clone(),
                    s.branch_owned_since(),
                ))
            })
            .collect()
    }

    /// Fan out `gh pr list` across all sessions on a fixed cadence (and on
    /// [`Self::request_pr_refresh`]), then persist results via
    /// [`Self::apply_pr_results`]. When `interval_secs` is 0 the periodic tick is
    /// disabled but a manual refresh still runs.
    fn spawn_pr_status_loop(&self, interval_secs: u64) -> tokio::task::JoinHandle<()> {
        let service = self.clone();
        let notify = self.pr_refresh.clone();
        let last_check = self.last_pr_check.clone();
        tokio::spawn(async move {
            let mut ticker = (interval_secs > 0)
                .then(|| tokio::time::interval(Duration::from_secs(interval_secs)));
            loop {
                match ticker.as_mut() {
                    Some(t) => {
                        tokio::select! {
                            _ = t.tick() => {}
                            _ = notify.notified() => {}
                        }
                    }
                    None => notify.notified().await,
                }
                // Debounce rapid re-triggers (e.g. a double manual refresh) so we
                // don't launch several concurrent sweeps.
                {
                    let mut lc = last_check.lock().expect("pr check time poisoned");
                    let now = std::time::Instant::now();
                    if !pr_check_debounce_passed(*lc, now, PR_CHECK_DEBOUNCE) {
                        continue;
                    }
                    *lc = Some(now);
                }
                // Adopt any branch renames before polling, so this same tick
                // queries GitHub with the corrected `--head` name.
                if let Err(e) = service.reconcile_session_branches().await {
                    debug!("reconcile_session_branches failed: {e}");
                }
                if !service.gh_available().await {
                    continue;
                }
                let sessions_to_check = service.pr_poll_targets().await;
                if sessions_to_check.is_empty() {
                    continue;
                }
                let results: Vec<(SessionId, PrCheckResult)> =
                    futures::stream::iter(sessions_to_check.into_iter().map(
                        |(id, branch, repo_path, created_at)| async move {
                            (
                                id,
                                crate::git::check_pr_for_branch(&repo_path, &branch, created_at)
                                    .await,
                            )
                        },
                    ))
                    .buffer_unordered(PR_FANOUT_CONCURRENCY)
                    .collect()
                    .await;
                if let Err(e) = service.apply_pr_results(results).await {
                    debug!("apply_pr_results failed: {e}");
                }
            }
        })
    }

    /// Fast-forward each project's main branch on a fixed cadence, recording the
    /// per-project outcome in [`Self::pull_status`] (surfaced through
    /// [`Snapshot::project_pull`]) and waking the change-feed when any
    /// project's status changes. No-op loop when disabled or `interval_secs` 0.
    fn spawn_project_pull_loop(
        &self,
        enabled: bool,
        interval_secs: u64,
    ) -> tokio::task::JoinHandle<()> {
        let store = self.store.clone();
        let pull_status = self.pull_status.clone();
        tokio::spawn(async move {
            if !enabled || interval_secs == 0 {
                return;
            }
            // Startup grace so launch doesn't immediately hammer every project.
            tokio::time::sleep(PROJECT_PULL_STARTUP_GRACE).await;
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                let projects: Vec<(ProjectId, PathBuf, String)> = {
                    let state = store.read().await;
                    state
                        .projects
                        .values()
                        .map(|p| (p.id, p.repo_path.clone(), p.main_branch.clone()))
                        .collect()
                };
                if projects.is_empty() {
                    continue;
                }
                let outcomes: Vec<(ProjectId, PullStatus)> =
                    futures::stream::iter(projects.into_iter().map(
                        |(id, repo_path, main_branch)| async move {
                            let status = crate::git::run_project_pull(&repo_path, &main_branch)
                                .await
                                .to_status();
                            (id, status)
                        },
                    ))
                    .buffer_unordered(PROJECT_PULL_FANOUT_CONCURRENCY)
                    .collect()
                    .await;

                let mut changed = false;
                {
                    let mut status = pull_status.lock().expect("pull status poisoned");
                    for (id, outcome) in outcomes {
                        if status.get(&id) != Some(&outcome) {
                            status.insert(id, outcome);
                            changed = true;
                        }
                    }
                }
                if changed {
                    store.notify_change();
                }
            }
        })
    }

    /// Reload the state file on a fixed cadence, waking the change-feed when
    /// another instance mutated it. No-op loop when `interval_ms` is 0.
    fn spawn_state_sync_loop(&self, interval_ms: u64) -> tokio::task::JoinHandle<()> {
        let store = self.store.clone();
        tokio::spawn(async move {
            if interval_ms == 0 {
                return;
            }
            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
            loop {
                interval.tick().await;
                match store.reload_if_changed().await {
                    Ok(_) => {}
                    Err(e) => debug!("State sync check failed: {e}"),
                }
            }
        })
    }

    // -- Internal helpers for the workspace surface --

    /// Cached `gh --version` availability (computed once per process).
    async fn gh_available(&self) -> bool {
        *self
            .gh_available
            .get_or_init(|| async { is_gh_available().await })
            .await
    }

    /// Error with `NotFound` when a session id doesn't exist, so mutations can
    /// surface a 404 rather than silently no-op'ing.
    async fn ensure_session_exists(&self, id: &SessionId) -> Result<()> {
        let state = self.store.read().await;
        if state.get_session(id).is_none() {
            return Err(SessionError::NotFound(*id).into());
        }
        Ok(())
    }

    /// Ask tmux what state a session's pane is in, keeping a tmux *failure*
    /// distinct from a genuine absence so [`reconciled_status`] can refuse to
    /// act on it. `session_exists` already draws that line for `has-session`;
    /// an `is_pane_dead` error gets the same treatment here.
    async fn probe_pane(&self, tmux_name: &str) -> PaneProbe {
        match self.manager.tmux.session_exists(tmux_name).await {
            Ok(false) => PaneProbe::Absent,
            Ok(true) => match self.manager.tmux.is_pane_dead(tmux_name).await {
                Ok(true) => PaneProbe::Dead,
                Ok(false) => PaneProbe::Live,
                Err(e) => {
                    debug!("Pane-liveness probe failed for {tmux_name}: {e}");
                    PaneProbe::Unknown
                }
            },
            Err(e) => {
                debug!("Session-existence probe failed for {tmux_name}: {e}");
                PaneProbe::Unknown
            }
        }
    }

    /// `(id, tmux_session_name, program)` tuples for active sessions, the input
    /// shape [`AgentStateDetector::detect_all`] expects.
    async fn active_session_targets(&self) -> Vec<(SessionId, String, String)> {
        let state = self.store.read().await;
        state
            .get_active_sessions()
            .into_iter()
            .map(|s| (s.id, s.tmux_session_name.clone(), s.program.clone()))
            .collect()
    }

    /// Detect agent states for active sessions via the shared TTL-cached
    /// detector — used to gate cascade/push delivery.
    async fn detect_active_states(&self) -> BTreeMap<SessionId, AgentState> {
        let active = self.active_session_targets().await;
        let mut detector = self.agent_detector.lock().await;
        detector.detect_all(&active).await
    }

    /// Push an operation onto the bounded ledger and return the recorded status.
    fn record_operation(&self, kind: OperationKind, outcome: OperationOutcome) -> OperationStatus {
        let status = OperationStatus {
            id: self.next_op_id.fetch_add(1, Ordering::Relaxed),
            kind,
            outcome,
            finished_at: Some(chrono::Utc::now()),
        };
        let mut ledger = self.operations.lock().expect("operation ledger poisoned");
        if ledger.len() >= OPERATION_LEDGER_CAP {
            ledger.pop_front();
        }
        ledger.push_back(status.clone());
        status
    }

    /// Map a cascade result onto a ledger entry (`Complete`/`PausedOnConflict`/
    /// error) and record it.
    fn record_cascade_outcome(&self, outcome: Result<CascadeOutcome>) -> OperationStatus {
        let outcome = match outcome {
            Ok(CascadeOutcome::Complete { sessions_merged }) => OperationOutcome::Succeeded {
                detail: format!("{sessions_merged} merged"),
            },
            Ok(CascadeOutcome::PausedOnConflict {
                at,
                sessions_merged,
            }) => OperationOutcome::Paused {
                detail: format!("paused at {at} after {sessions_merged} merged"),
            },
            Err(e) => OperationOutcome::Failed {
                error: e.to_string(),
            },
        };
        self.record_operation(OperationKind::Cascade, outcome)
    }

    /// Snapshot the operation ledger (oldest first).
    fn operations_snapshot(&self) -> Vec<OperationStatus> {
        self.operations
            .lock()
            .expect("operation ledger poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

/// [`CommanderService::peek_review`]'s result: the current review diff, its
/// per-file hashes, and the display paths of its files whose reviewed mark
/// holds against it.
#[derive(Debug, Clone)]
pub struct ReviewPeek {
    pub diff: ParsedDiff,
    pub hashes: DiffHashes,
    pub reviewed: std::collections::HashSet<String>,
}

/// What a [`CommanderService::preview`] call targets: a session (with an
/// optional explicit pane line count) or a project.
#[derive(Debug, Clone, Copy)]
pub enum PreviewTarget {
    Session { id: SessionId, lines: Option<usize> },
    Project(ProjectId),
}

/// Project a computed [`DiffInfo`](crate::git::DiffInfo)'s counts onto the
/// protocol [`DiffStat`] DTO carried in [`PreviewData`].
fn diff_stat_from_info(d: &crate::git::DiffInfo) -> crate::api::DiffStat {
    crate::api::DiffStat {
        files_changed: d.files_changed,
        lines_added: d.lines_added,
        lines_removed: d.lines_removed,
    }
}

/// Options for [`CommanderService::spawn_background_tasks`].
#[derive(Debug, Clone, Default)]
pub struct BackgroundOpts {
    /// Whether the persistent commander session should be polled for its agent
    /// state. Restart-required in the TUI (mirrors `commander_enabled_at_init`),
    /// so it is passed in rather than re-read live.
    pub commander_enabled: bool,
}

/// Abortable handles to the service's background loops (see
/// [`CommanderService::spawn_background_tasks`]). Production callers ignore the
/// value and let the loops run for the process lifetime; tests hold it and call
/// [`Self::abort`] to stop them deterministically.
#[derive(Default)]
pub struct BackgroundHandles {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl BackgroundHandles {
    /// Abort every background loop. Idempotent.
    pub fn abort(&self) {
        for h in &self.handles {
            h.abort();
        }
    }

    /// Whether this handle owns no loops — true for the no-op returned by a
    /// second [`CommanderService::spawn_background_tasks`] call.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

/// Cap concurrent subprocess fan-outs (e.g. `gh pr list` across all sessions).
/// Each call holds 3+ pipe FDs, so unbounded fan-out can EMFILE under the macOS
/// launchd 256-FD default.
const PR_FANOUT_CONCURRENCY: usize = 8;

/// Cap concurrent project-branch pulls so a user with many projects doesn't
/// spawn one `git fetch` per project at the same instant.
const PROJECT_PULL_FANOUT_CONCURRENCY: usize = 4;

/// Cap concurrent pane probes in the startup reconcile. The reconcile probes
/// *every* session (it syncs both ways), so on a long-lived workspace this is
/// tens of `tmux has-session` calls on the pre-first-paint path — serially that
/// is a visible startup stall. `TmuxExecutor`'s own semaphore is the real
/// throttle; this just keeps the in-flight future count sane.
const PANE_PROBE_CONCURRENCY: usize = 8;

/// Minimum gap between PR-status fan-outs, debouncing rapid manual triggers
/// (e.g. a double manual refresh). It sits far below `pr_check_interval_secs`,
/// so a manual refresh is still effectively immediate.
const PR_CHECK_DEBOUNCE: Duration = Duration::from_secs(2);

/// Grace period after startup before the first project-pull sweep, so launch
/// doesn't immediately hammer every project.
const PROJECT_PULL_STARTUP_GRACE: Duration = Duration::from_secs(5);

/// Whether enough time has elapsed since the last PR-status check to spawn
/// another (see [`PR_CHECK_DEBOUNCE`]). `None` (never checked) always passes.
fn pr_check_debounce_passed(
    last_check: Option<std::time::Instant>,
    now: std::time::Instant,
    debounce: Duration,
) -> bool {
    last_check.is_none_or(|t| now.saturating_duration_since(t) >= debounce)
}

/// Session ids whose agent just finished a turn: present as [`AgentState::Idle`]
/// now but [`AgentState::Working`] in the previous poll. Drives the unread
/// marker. An empty `prev` (never polled, or cleared after an attach) yields no
/// transitions, so a freshly-populated baseline can't produce false unread.
pub fn detect_unread_transitions(
    prev: &BTreeMap<SessionId, AgentState>,
    new: &BTreeMap<SessionId, AgentState>,
) -> Vec<SessionId> {
    new.iter()
        .filter(|(id, state)| {
            **state == AgentState::Idle && prev.get(id) == Some(&AgentState::Working)
        })
        .map(|(id, _)| *id)
        .collect()
}

/// Append the commander's sentinel detection target to `active` when the
/// commander is running, so its agent state is detected alongside real
/// sessions. The sentinel is a reserved id with no `WorktreeSession`; callers
/// that flag unread transitions must filter it out. Shared by the poll loop and
/// the `fresh` [`CommanderService::agent_states`] rebuild so both carry the
/// commander's state (keeping the footer chip from blinking empty for a tick).
fn with_commander_target(
    mut active: Vec<(SessionId, String, String)>,
    commander_running: bool,
    commander_program: &str,
) -> Vec<(SessionId, String, String)> {
    if commander_running {
        active.push((
            claude_commander_protocol::session::COMMANDER_SENTINEL_ID,
            crate::commander::COMMANDER_TMUX_NAME.to_string(),
            commander_program.to_string(),
        ));
    }
    active
}

/// What tmux reports about one session's pane, as the startup reconciler sees
/// it. Distinguishing [`Self::Unknown`] from [`Self::Absent`] is the whole
/// point: a tmux *failure* tells us nothing about the session and must never
/// move its status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneProbe {
    /// The tmux session exists and its pane is running.
    Live,
    /// The tmux session exists but its pane has exited (held by
    /// `remain-on-exit`).
    Dead,
    /// The tmux session genuinely does not exist.
    Absent,
    /// tmux itself failed (crashed server, timeout, spawn error).
    Unknown,
}

/// The status a session should be moved to given what tmux reports, or `None`
/// to leave it as it is.
///
/// Reconciliation is deliberately **bidirectional**. A live pane and a
/// `Stopped` status contradict each other — every path that produces `Stopped`
/// (kill, hibernate, this reconciler) kills the pane first — and the
/// contradiction is reachable: a pass that read tmux as absent (a run pointed at
/// a different `TMUX_TMPDIR`, or `no server running` from a server that has
/// since restarted) marks sessions `Stopped`, and its cleanup kill no-ops on
/// that same wrong socket, leaving live agents flagged stopped. Without the
/// revive there is no way back: `Stopped` suppresses agent-state detection
/// (gated on [`SessionStatus::is_active`]), so those rows render `○` forever.
///
/// Pure, so the policy can be tested without tmux.
pub(crate) fn reconciled_status(stored: SessionStatus, probe: PaneProbe) -> Option<SessionStatus> {
    match probe {
        // A detection failure is not evidence of anything.
        PaneProbe::Unknown => None,
        // A live pane means Running, whatever we had recorded. Transient
        // cascade states (`Merging`/`Pushing`/`CascadePaused`) are left alone —
        // they imply a live pane already and carry extra meaning this reconciler
        // must not erase.
        PaneProbe::Live => (stored == SessionStatus::Stopped).then_some(SessionStatus::Running),
        // No pane, or a pane whose program exited: Stopped, unless the session
        // is mid-creation (a pane that does not exist *yet* is normal, and
        // stale `Creating` records are dropped earlier in the reconcile).
        PaneProbe::Absent | PaneProbe::Dead => (stored != SessionStatus::Creating
            && stored != SessionStatus::Stopped)
            .then_some(SessionStatus::Stopped),
    }
}

/// Whether the agent-state poll tick can skip entirely: nothing to detect (no
/// running sessions and the commander isn't running) AND the commander's
/// running state has not changed since the last emitted update. Skipping keeps
/// the no-sessions path quiet — no cache write, no change-feed wake.
fn poll_tick_can_skip(
    sessions_empty: bool,
    commander_running: bool,
    last_commander_running: bool,
) -> bool {
    sessions_empty && !commander_running && !last_commander_running
}

/// Whether a poll tick (that wasn't skipped) should emit an update: when there
/// are fresh agent states, or the commander's running state flipped — the
/// latter is what lets the footer chip turn *off* on the trailing edge.
fn poll_tick_should_send(
    states_empty: bool,
    commander_running: bool,
    last_commander_running: bool,
) -> bool {
    !states_empty || commander_running != last_commander_running
}

/// Write the apply brief to a fresh absolute path under `base` (the OS temp
/// dir in normal use, so it's outside the worktree and never committed).
///
/// The name carries the session id for legibility **and a per-apply UUID for
/// uniqueness**: the path used to be session-stable and overwritten on
/// re-apply, which lost the earlier batch whenever a second Apply landed before
/// the agent opened the first file — and those comments were already marked
/// `Applied`, so nothing brought them back. A brief must therefore outlive the
/// next Apply.
///
/// The cost is one small file per Apply, left behind for whatever sweeps the
/// OS temp dir (distro-dependent on Linux, so assume nothing does). It is
/// deliberately not pruned here: nothing can distinguish a read brief from an
/// unread one, so the only file a prune could target is exactly the unread
/// brief this uniqueness exists to keep — and a brief is a few KB written at
/// human rates, unlike the remote-triggerable images `PasteImageStore` prunes.
async fn write_apply_brief(base: &Path, session_id: SessionId, markdown: &str) -> Result<PathBuf> {
    let path = base.join(format!(
        "cc-comments-{}-{}.md",
        session_id.as_uuid(),
        Uuid::new_v4()
    ));
    let save_failed = |e: std::io::Error| crate::error::ConfigError::SaveFailed(e.to_string());
    tokio::fs::create_dir_all(base).await.map_err(save_failed)?;
    tokio::fs::write(&path, markdown)
        .await
        .map_err(save_failed)?;
    Ok(path)
}

/// Poll the agent state, returning `true` once it leaves `WaitingForInput`, or
/// `false` if it stays at a prompt past the bounded timeout.
async fn wait_until_ready(
    detector: &mut AgentStateDetector,
    kind: AgentKind,
    tmux_name: &str,
) -> bool {
    const ATTEMPTS: u32 = 20;
    const INTERVAL: Duration = Duration::from_millis(250);
    for _ in 0..ATTEMPTS {
        if detector.detect(kind, tmux_name).await != AgentState::WaitingForInput {
            return true;
        }
        tokio::time::sleep(INTERVAL).await;
    }
    false
}

/// The logical base a session's review diff is computed against.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReviewBase {
    /// A target *branch*, with the frozen fork-point SHA as a fallback. The
    /// branch resolves to its `origin/<branch>` remote-tracking ref when present
    /// (so the diff reflects the pushed upstream rather than a possibly-stale
    /// local branch), giving `merge-base(branch, HEAD)..working-tree` recomputed
    /// against the branch's *live* tip — the GitHub PR model. If the branch can
    /// no longer be resolved at all, the `fallback` SHA is used instead.
    Branch {
        name: String,
        fallback: Option<String>,
    },
    /// The fork-point commit captured at session creation (a fixed SHA). Only
    /// reached when no target branch was ever recorded.
    Commit(String),
    /// No base known; diff the working tree against `HEAD`.
    Head,
}

impl ReviewBase {
    /// Classify a session's base, preferring a *live branch* over a frozen SHA:
    ///
    /// 1. the PR's target branch (GitHub is authoritative once a PR exists);
    /// 2. else the branch captured at creation ([`WorktreeSession::base_branch`]
    ///    — a stack parent's branch, an explicit `--base-branch`, or main),
    ///    which keeps the diff correct as that branch advances;
    /// 3. else the frozen fork-point commit ([`WorktreeSession::base_commit`]),
    ///    used only when no branch was recorded;
    /// 4. else `HEAD`.
    ///
    /// In the branch cases the frozen `base_commit` rides along as a fallback,
    /// used by [`Self::git_ref`] only if the branch itself fails to resolve.
    fn of(session: &WorktreeSession) -> Self {
        let fallback = session.base_commit.clone();
        if let Some(branch) = session.pr_base_branch.clone() {
            ReviewBase::Branch {
                name: branch,
                fallback,
            }
        } else if let Some(branch) = session.base_branch.clone() {
            ReviewBase::Branch {
                name: branch,
                fallback,
            }
        } else if let Some(commit) = session.base_commit.clone() {
            ReviewBase::Commit(commit)
        } else {
            ReviewBase::Head
        }
    }

    /// The git commit-ish to diff against. A branch base prefers its
    /// remote-tracking ref; if neither the remote nor local branch resolves it
    /// drops to the frozen `fallback` SHA, then to the branch name verbatim. A
    /// commit SHA and `HEAD` are used verbatim.
    async fn git_ref(self, worktree: &Path) -> String {
        match self {
            ReviewBase::Branch { name, fallback } => {
                let candidate = prefer_remote_branch(worktree, &name).await;
                if crate::git::ref_resolves(worktree, &candidate).await {
                    candidate
                } else if let Some(sha) = fallback {
                    sha
                } else {
                    candidate
                }
            }
            ReviewBase::Commit(commit) => commit,
            ReviewBase::Head => "HEAD".to_string(),
        }
    }
}

/// Validate that Claude-only create flags aren't set for a program that can't
/// use them. [`CreateSessionOpts`] itself is a plain wire type in
/// `claude-commander-protocol`; this check lives here because it needs core's
/// [`AgentKind`] harness abstraction.
pub fn validate_program_flags(opts: &CreateSessionOpts, resolved_program: &str) -> Result<()> {
    let kind = AgentKind::from_program(resolved_program);
    // `--effort` / `--mode` map to Claude-specific flags.
    if !kind.is_claude() && (opts.effort.is_some() || opts.mode.is_some()) {
        return Err(SessionError::InvalidProgram(format!(
            "--effort and --mode are only supported when the program is \
             claude (got {:?})",
            resolved_program
        ))
        .into());
    }
    // An initial prompt is passed as a positional argument, which only
    // harnesses that accept one (claude, codex, omp) understand.
    if opts.initial_prompt.is_some() && !kind.accepts_positional_prompt() {
        return Err(SessionError::InvalidProgram(format!(
            "--initial-prompt is only supported for programs that accept a \
             positional prompt, e.g. claude, codex, or omp (got {:?})",
            resolved_program
        ))
        .into());
    }
    // `--model` is understood by Claude, Codex, OpenCode, and omp.
    if opts.model.is_some() && !kind.supports_model_flag() {
        return Err(SessionError::InvalidProgram(format!(
            "--model is only supported for programs that accept it, e.g. \
             claude, codex, opencode, or omp (got {:?})",
            resolved_program
        ))
        .into());
    }
    Ok(())
}

/// Build the telemetry handle for a freshly-constructed service: resolve the
/// install id, construct the handle from config, and emit the once-per-launch
/// `session_start` event. A no-op handle is returned when telemetry is disabled.
fn init_telemetry(
    config_store: &Arc<ConfigStore>,
    store: &Arc<StateStore>,
    frontend: &FrontendInfo,
) -> Telemetry {
    let config = config_store.read().clone();
    // Skip install-id generation (and its background persist spawn) entirely
    // when telemetry is off — keeps disabled/sync contexts (e.g. unit tests)
    // from needing a Tokio runtime.
    if !crate::telemetry::would_be_enabled(&config.telemetry) {
        return Telemetry::disabled();
    }
    let install_id = ensure_install_id(store);
    let telemetry = Telemetry::init(&config.telemetry, frontend, &install_id);
    if telemetry.is_active() {
        let env = EnvFingerprint::collect(Some(ColorMode::detect().name()));
        let snapshot = ConfigSnapshot::from_config(&config);
        telemetry.session_start(&env, &snapshot);
    }
    telemetry
}

/// Return the anonymous install id, generating one if none is stored yet.
///
/// When genuinely absent, the fresh id is persisted **synchronously** (a
/// one-time, flocked, uncontended write via [`StateStore::persist_install_id_if_absent`])
/// so it is durable before construction returns and reused on every future
/// launch. A previous version spawned the persist as a fire-and-forget
/// `tokio::spawn`, which short-lived callers (one-shot CLI, or a GUI that builds
/// and drops a runtime per operation) tore down before it flushed — so every
/// launch minted a new id and inflated unique-install telemetry. The persist
/// never clobbers an existing id: if another writer won the race, its id is
/// returned instead of ours.
fn ensure_install_id(store: &Arc<StateStore>) -> String {
    if let Some(id) = store.try_install_id() {
        return id;
    }
    let candidate = Uuid::new_v4().to_string();
    store
        .persist_install_id_if_absent(&candidate)
        .unwrap_or_else(|e| {
            warn!("failed to persist install id ({e}); using an ephemeral id this session");
            candidate
        })
}

// -- Response types --
//
// The HTTP request/response DTOs live in `claude-commander-protocol`
// (`Serialize + Deserialize`, mobile-safe) and are re-exported here so
// `crate::api::{SessionInfo, ReviewSnapshot, ...}` paths keep working.
// Construction that needs core's domain model is done by
// `session_info_from_session` below.
pub use claude_commander_protocol::api::{
    AgentStatesSnapshot, BranchInfo, ChangeProgram, CreateOptions, CreateSessionOpts, DiffSide,
    DiffStat, NewComment, OperationKind, OperationOutcome, OperationStatus, PrRetarget,
    PreviewData, ProgramInfo, ProjectInfo, PullBlockReason, PullStatus, RenameSession,
    ReviewSnapshot, ServerStatus, SessionDetail, SessionInfo, SetProgramsRequest, SetSection,
    SetSessionBase, SetSessionBaseOutcome, Snapshot, ToggleReviewed,
};

/// Build a [`SessionInfo`] wire DTO from core's `WorktreeSession` domain model.
/// (Was `SessionInfo::from_session`; relocated here because `SessionInfo` is now
/// a foreign type and this conversion needs core-only types.)
pub(crate) fn session_info_from_session(
    session: &WorktreeSession,
    project_name: &str,
) -> SessionInfo {
    SessionInfo {
        id: session.id.as_uuid().to_string(),
        session_id: session.id,
        title: session.title.clone(),
        branch: session.branch.clone(),
        status: session.status,
        program: session.program.clone(),
        project_id: session.project_id,
        project_name: project_name.to_string(),
        pr_number: session.pr_number,
        pr_url: session.pr_url.clone(),
        pr_state: effective_pr_state(session.pr_state, session.pr_merged),
        pr_draft: session.pr_draft,
        pr_labels: session.pr_labels.clone(),
        review_decision: session.review_decision,
        pr_reviewers: session.pr_reviewers.clone(),
        created_at: session.created_at,
        unread: session.unread,
        stack_parent_session_id: session.stack_parent_session_id,
        pr_base_branch: session.pr_base_branch.clone(),
        pr_merged: session.pr_merged,
        current_section: session.current_section.clone(),
        section_override: session.section_override.clone(),
        entered_section_at: Some(session.entered_section_at),
        last_attached_at: session.last_attached_at,
        keep_alive: session.keep_alive,
        worktree_path: session.worktree_path.to_string_lossy().into_owned(),
        tmux_session_name: session.tmux_session_name.clone(),
    }
}

// -- Internal helpers --

/// Build the [`ProjectInfo`] list from state, sorted by project name (stable
/// ordering for the tree). Sessions are carried by id only; the full
/// [`SessionInfo`] list rides alongside in [`Snapshot`].
fn build_project_info_list(state: &AppState) -> Vec<ProjectInfo> {
    let mut projects: Vec<_> = state.projects.values().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    projects
        .into_iter()
        .map(|p| ProjectInfo {
            id: p.id,
            name: p.name.clone(),
            repo_path: p.repo_path.clone(),
            main_branch: p.main_branch.clone(),
            session_ids: p.worktrees.clone(),
            origin_url: p.origin_url.clone(),
            workspace: p.workspace.clone(),
        })
        .collect()
}

fn build_session_info_list(state: &AppState, include_stopped: bool) -> Vec<SessionInfo> {
    let mut entries: Vec<(String, SessionInfo)> = Vec::new();
    for project in state.projects.values() {
        for session in project
            .worktrees
            .iter()
            .filter_map(|id| state.sessions.get(id))
            .filter(|s| include_stopped || s.status.is_active())
        {
            entries.push((
                project.name.clone(),
                session_info_from_session(session, &project.name),
            ));
        }
    }
    // Canonicalize the order so it never depends on `state.projects`' HashMap
    // iteration order (which would leak into the serialized snapshot and make
    // the wire output nondeterministic across processes/polls). Key: project
    // name, then the session's `created_at`, then its id as a final stable
    // tiebreaker for sessions sharing a timestamp.
    entries.sort_by(|(a_name, a), (b_name, b)| {
        a_name
            .cmp(b_name)
            .then(a.created_at.cmp(&b.created_at))
            .then(a.session_id.cmp(&b.session_id))
    });
    entries.into_iter().map(|(_, info)| info).collect()
}

/// Build a [`Snapshot`] purely from persisted state, with no live
/// gh/tmux probing — the I/O-bearing fields ([`ServerStatus`], pending
/// comments, operations) get cheap placeholders. The full
/// [`CommanderService::snapshot`] is the production source of truth
/// (the change-feed cache reads it); this synchronous, allocation-only
/// projection is retained for the tree-builder tests, which feed a
/// hand-constructed `AppState` through the same DTO builders.
#[cfg(any(test, feature = "test-support"))]
pub fn snapshot_from_state(state: &AppState) -> Snapshot {
    Snapshot {
        projects: build_project_info_list(state),
        sessions: build_session_info_list(state, true),
        cascade_paused: state.cascade_paused_at,
        pending_comment_sessions: Vec::new(),
        project_pull: BTreeMap::new(),
        operations: Vec::new(),
        server: ServerStatus {
            gh_available: false,
            tmux_ok: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        workspaces: Vec::new(),
        main_workspace: None,
        startup_workspace: Default::default(),
    }
}

fn find_session_info(state: &AppState, query: &str) -> Option<SessionInfo> {
    let session = find_session(state, query)?;
    let project_name = state
        .projects
        .get(&session.project_id)
        .map(|p| p.name.as_str())
        .unwrap_or("unknown");
    Some(session_info_from_session(session, project_name))
}

async fn capture_pane(
    executor: &TmuxExecutor,
    tmux_name: &str,
    lines: Option<usize>,
) -> Result<Option<String>> {
    if !executor.session_exists(tmux_name).await? {
        return Ok(None);
    }
    let mut args = vec!["capture-pane", "-t", tmux_name, "-p"];
    let lines_arg;
    if let Some(n) = lines {
        lines_arg = format!("-{}", n);
        args.extend_from_slice(&["-S", &lines_arg]);
    }
    Ok(Some(executor.execute(&args).await?))
}

/// Maximum lines allowed for a pane-capture line count (the CLI `log` command's
/// `--lines` flag, and the HTTP API's equivalent query parameter).
pub(crate) const LOG_MAX_LINES: usize = 10_000;

/// Clamp a requested line count to the allowed range [1, LOG_MAX_LINES].
pub(crate) fn clamp_log_lines(requested: usize) -> usize {
    requested.clamp(1, LOG_MAX_LINES)
}

// -- Repository clone helpers --

/// The directory name a clone lands in: the caller's `dest_name` when given,
/// otherwise the name derived from the source.
///
/// The **source is validated either way**, before the override is even looked at:
/// it becomes an argv element regardless of where the directory name comes from,
/// and that is the hazard that is invisible when it goes wrong.
fn clone_dir_name(req: &CloneRequest) -> Result<String> {
    let derived = match &req.source {
        CloneSource::Github { full_name } => {
            validate_repo_slug(full_name).map_err(clone_source_rejected)?;
            // `validate_repo_slug` has just guaranteed exactly one `/`; the
            // fallback is unreachable rather than a policy.
            full_name
                .split_once('/')
                .map_or(full_name.as_str(), |(_, name)| name)
                .to_string()
        }
        CloneSource::Url { url } => {
            validate_clone_url(url)
                .map_err(clone_source_rejected)?
                .default_dir_name
        }
    };
    match req.dest_name.as_deref() {
        Some(name) => Ok(validate_dest_name(name)
            .map_err(clone_source_rejected)?
            .to_string()),
        None => Ok(derived),
    }
}

/// What to show the user as a clone's source: the slug or the URL, as typed.
///
/// Raw user input — every consumer redacts (`CloneJobs::spawn` for the job label,
/// [`clone_source_rejected`] for an error message), so this must not be logged or
/// formatted into a message on its own.
fn clone_source_label(source: &CloneSource) -> &str {
    match source {
        CloneSource::Github { full_name } => full_name,
        CloneSource::Url { url } => url,
    }
}

/// `Some(is_git_repo)` when `dest` is already occupied, `None` when it is free.
///
/// An `Option` rather than a pair of booleans because `is_git_repo` only means
/// anything once something is there — which is exactly the shape
/// `CloneStatus::DestinationExists` needs.
///
/// `symlink_metadata`, not `exists()`: a dangling symlink is an occupied path (git
/// refuses to clone onto one) while `exists()` follows the link and would call it
/// free. A `.git` entry decides the flag and is tested with `exists()` because it
/// is a directory in an ordinary checkout and a *file* in a linked worktree or
/// submodule. A **bare** repo at the destination reads as "occupied, not a repo" —
/// it has no `.git`, and it is not something a user would want registered as a
/// project anyway.
fn occupied_destination(dest: &Path) -> Option<bool> {
    dest.symlink_metadata().ok()?;
    Some(dest.join(".git").exists())
}

/// Clone into `dest`, register the checkout as a project, and report which of the
/// three terminal outcomes happened.
///
/// The fallible steps sit in **one `?`-scoped block with a single cleanup**, so a
/// step added later (an LFS fetch, a hook install) inherits the
/// remove-the-partial-clone path instead of repeating it. CLAUDE.md calls this out
/// as a review-time rule precisely because clippy cannot see it.
///
/// `ensure_project` builds a non-`Send` `gix::Repository` and holds it across an
/// await, so it cannot be driven directly by this `tokio::spawn`ed task;
/// [`run_local`] hands it to a current-thread runtime on its own thread. Same
/// reason the server's project handlers do it (`handlers/projects.rs`).
async fn clone_then_register(
    service: CommanderService,
    source: CloneSource,
    workspace: Option<String>,
    dest: PathBuf,
    timeout: Duration,
) -> CloneOutcome {
    let cloned = async {
        run_clone(&source, &dest, timeout).await?;
        let path = dest.clone();
        run_local(move || async move { service.ensure_project(path, workspace).await })
            .await
            .map_err(|e| match e {
                RunLocalError::Inner(err) => err,
                RunLocalError::WorkerLost => GitError::OperationFailed(
                    "the worker registering the cloned repository was lost".to_string(),
                )
                .into(),
            })
    }
    .await;

    match cloned {
        Ok(project_id) => CloneOutcome::succeeded(project_id),
        Err(e) => {
            remove_partial_clone(&dest);
            // `CloneOutcome::failed` redacts, so a message built from git's stderr
            // cannot carry a credential into the registry.
            CloneOutcome::failed(e.to_string())
        }
    }
}

/// Remove whatever a failed clone left at `dest`, so a retry into the same
/// directory is not blocked by a half-written checkout.
///
/// Only ever called for a destination [`CommanderService::start_clone`] found free
/// moments earlier, so this can only delete what this clone wrote — the same rule
/// `run_clone`'s own cleanup follows. It overlaps with that cleanup for a failed
/// clone (harmless: a missing directory is not an error) and is the *only* cleanup
/// for a step that fails after the checkout exists.
fn remove_partial_clone(dest: &Path) {
    match std::fs::remove_dir_all(dest) {
        Ok(()) => debug!("removed partial clone at {}", dest.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("failed to remove partial clone at {}: {e}", dest.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_commander_protocol::github::CloneStatus;
    use claude_commander_protocol::workspace::{StartupWorkspace, WorkspaceDef};

    use crate::comment::CommentSide;
    use crate::git::PrState;
    use crate::git::fixture::{fixture_git, fixture_git_std};
    use crate::session::{Project, ProjectId, SessionId, SessionStatus, WorktreeSession};
    use std::path::PathBuf;

    fn make_project(name: &str) -> Project {
        Project::new(name, PathBuf::from("/tmp/repo"), "main")
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

    // -- clamp_log_lines tests --

    #[test]
    fn clamp_log_lines_default_passthrough() {
        assert_eq!(clamp_log_lines(100), 100);
    }

    #[test]
    fn clamp_log_lines_zero_becomes_one() {
        assert_eq!(clamp_log_lines(0), 1);
    }

    #[test]
    fn clamp_log_lines_max_boundary() {
        assert_eq!(clamp_log_lines(LOG_MAX_LINES), LOG_MAX_LINES);
    }

    #[test]
    fn clamp_log_lines_over_max() {
        assert_eq!(clamp_log_lines(LOG_MAX_LINES + 1), LOG_MAX_LINES);
        assert_eq!(clamp_log_lines(usize::MAX), LOG_MAX_LINES);
    }

    #[test]
    fn build_project_info_list_projects_origin_url() {
        // The repo picker badges a row by comparing a project's origin against
        // the API's clone url, so the persisted value has to reach the wire DTO.
        // It is projected, never derived here: this is a pure sync projection of
        // `AppState` on the workspace-poll path, with no repo to open.
        let mut with_origin = make_project("with-origin");
        with_origin.origin_url = Some("git@github.com:sizeak/claude-commander.git".to_string());
        let without_origin = make_project("no-origin");

        let mut state = AppState::new();
        state.projects.insert(with_origin.id, with_origin.clone());
        state
            .projects
            .insert(without_origin.id, without_origin.clone());

        let infos = build_project_info_list(&state);
        let find = |id: ProjectId| infos.iter().find(|i| i.id == id).unwrap();
        assert_eq!(
            find(with_origin.id).origin_url.as_deref(),
            Some("git@github.com:sizeak/claude-commander.git")
        );
        assert_eq!(find(without_origin.id).origin_url, None);
    }

    #[test]
    fn build_session_info_list_order_is_canonical_not_insertion_order() {
        // Six single-session projects (names p0..p5) built up front, then
        // inserted into two states in OPPOSITE orders. The serialized session
        // order must equal the project-name-sorted order in BOTH — never the
        // HashMap iteration order of `state.projects`. Against an unsorted
        // builder the reverse (and almost always the forward) insertion diverges
        // from the canonical order for six projects: red.
        let mut fixtures: Vec<(Project, WorktreeSession)> = (0..6)
            .map(|i| {
                let p = make_project(&format!("p{i}"));
                let s = make_session_for_project("s", p.id);
                (p, s)
            })
            .collect();
        // Canonical order: by project name, i.e. the p0..p5 build order.
        let expected: Vec<SessionId> = fixtures.iter().map(|(_, s)| s.id).collect();

        let build = |order: &[(Project, WorktreeSession)]| -> Vec<SessionId> {
            let mut state = AppState::new();
            for (p, s) in order {
                let mut proj = p.clone();
                proj.add_worktree(s.id);
                state.projects.insert(proj.id, proj);
                state.sessions.insert(s.id, s.clone());
            }
            build_session_info_list(&state, true)
                .into_iter()
                .map(|si| si.session_id)
                .collect()
        };

        let forward = build(&fixtures);
        fixtures.reverse();
        let reverse = build(&fixtures);

        assert_eq!(
            forward, expected,
            "forward-insertion session order must be canonical (name-sorted)"
        );
        assert_eq!(
            reverse, expected,
            "reverse-insertion session order must be canonical (name-sorted)"
        );
    }

    #[test]
    fn session_info_from_session_populates_fields() {
        let session = make_session_for_project("fix-bug", ProjectId::new());
        let info = session_info_from_session(&session, "my-project");

        assert_eq!(info.title, "fix-bug");
        assert_eq!(info.branch, "branch-fix-bug");
        assert_eq!(info.program, "claude");
        assert_eq!(info.project_name, "my-project");
        assert_eq!(info.session_id, session.id);
        assert!(uuid::Uuid::parse_str(&info.id).is_ok());
    }

    #[test]
    fn session_info_resolves_legacy_pr_merged() {
        let mut session = make_session_for_project("legacy", ProjectId::new());
        session.pr_number = Some(10);
        session.pr_state = None;
        session.pr_merged = true;

        let info = session_info_from_session(&session, "proj");
        assert_eq!(info.pr_state, PrState::Merged);
    }

    #[test]
    fn build_list_excludes_stopped_by_default() {
        let project = make_project("repo");
        let mut s1 = make_session_for_project("running", project.id);
        s1.set_status(SessionStatus::Running);
        let mut s2 = make_session_for_project("stopped", project.id);
        s2.set_status(SessionStatus::Stopped);

        let state = make_state_with_project(&project, vec![s1, s2]);
        let list = build_session_info_list(&state, false);

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
        let list = build_session_info_list(&state, true);

        assert_eq!(list.len(), 2);
    }

    #[test]
    fn build_list_populates_project_name() {
        let project = make_project("my-repo");
        let s = make_session_for_project("task", project.id);
        let state = make_state_with_project(&project, vec![s]);
        let list = build_session_info_list(&state, false);

        assert_eq!(list.len(), 1);
        assert_eq!(list[0].project_name, "my-repo");
    }

    #[test]
    fn find_session_info_by_title() {
        let project = make_project("repo");
        let s = make_session_for_project("fix-auth", project.id);
        let expected_id = s.id;
        let state = make_state_with_project(&project, vec![s]);

        let info = find_session_info(&state, "fix-auth").unwrap();
        assert_eq!(info.session_id, expected_id);
        assert_eq!(info.project_name, "repo");
    }

    #[test]
    fn find_session_info_returns_none_for_missing() {
        let state = AppState::new();
        assert!(find_session_info(&state, "nope").is_none());
    }

    #[test]
    fn session_detail_flattens_info_in_json() {
        let session = make_session_for_project("test", ProjectId::new());
        let detail = SessionDetail {
            info: session_info_from_session(&session, "proj"),
            agent_state: AgentState::Working,
            diff_stat: Some("3 files changed".to_string()),
            pane_content: None,
        };
        let json: serde_json::Value = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["title"], "test");
        assert_eq!(json["agent_state"], "working");
        assert_eq!(json["diff_stat"], "3 files changed");
        assert!(json["pane_content"].is_null());
    }

    #[test]
    fn validate_rejects_non_claude_program_with_effort() {
        let opts = CreateSessionOpts {
            project_path: PathBuf::from("/tmp/repo"),
            title: "test".to_string(),
            program: Some("bash".to_string()),
            initial_prompt: None,
            effort: Some("high".to_string()),
            mode: None,
            model: None,
            base_branch: None,
            section: None,
            stack_parent: None,
        };
        let err = validate_program_flags(&opts, "bash").unwrap_err();
        assert!(err.to_string().contains("--effort"));
    }

    #[test]
    fn validate_rejects_non_claude_program_with_mode() {
        let opts = CreateSessionOpts {
            project_path: PathBuf::from("/tmp/repo"),
            title: "test".to_string(),
            program: Some("vim".to_string()),
            initial_prompt: None,
            effort: None,
            mode: Some("auto".to_string()),
            model: None,
            base_branch: None,
            section: None,
            stack_parent: None,
        };
        let err = validate_program_flags(&opts, "vim").unwrap_err();
        assert!(err.to_string().contains("--mode"));
    }

    #[test]
    fn validate_allows_claude_with_flags() {
        let opts = CreateSessionOpts {
            project_path: PathBuf::from("/tmp/repo"),
            title: "test".to_string(),
            program: Some("claude".to_string()),
            initial_prompt: Some("hello".to_string()),
            effort: Some("high".to_string()),
            mode: Some("auto".to_string()),
            model: Some("opus".to_string()),
            base_branch: None,
            section: None,
            stack_parent: None,
        };
        validate_program_flags(&opts, "claude").unwrap();
    }

    #[test]
    fn validate_rejects_unknown_program_with_model() {
        let opts = CreateSessionOpts {
            project_path: PathBuf::from("/tmp/repo"),
            title: "test".to_string(),
            program: Some("bash".to_string()),
            initial_prompt: None,
            effort: None,
            mode: None,
            model: Some("opus".to_string()),
            base_branch: None,
            section: None,
            stack_parent: None,
        };
        let err = validate_program_flags(&opts, "bash").unwrap_err();
        assert!(err.to_string().contains("--model"));
    }

    #[test]
    fn validate_allows_codex_with_model() {
        let opts = CreateSessionOpts {
            project_path: PathBuf::from("/tmp/repo"),
            title: "test".to_string(),
            program: Some("codex".to_string()),
            initial_prompt: None,
            effort: None,
            mode: None,
            model: Some("gpt-5".to_string()),
            base_branch: None,
            section: None,
            stack_parent: None,
        };
        validate_program_flags(&opts, "codex").unwrap();
    }

    #[test]
    fn validate_allows_opencode_with_model() {
        let opts = CreateSessionOpts {
            project_path: PathBuf::from("/tmp/repo"),
            title: "test".to_string(),
            program: Some("opencode".to_string()),
            initial_prompt: None,
            effort: None,
            mode: None,
            model: Some("anthropic/claude-sonnet-4-5".to_string()),
            base_branch: None,
            section: None,
            stack_parent: None,
        };
        validate_program_flags(&opts, "opencode").unwrap();
    }

    #[test]
    fn review_base_prefers_live_branch_then_falls_back_to_commit_then_head() {
        let mut s = make_session_for_project("t", ProjectId::new());
        // Nothing recorded yet → HEAD.
        assert_eq!(ReviewBase::of(&s), ReviewBase::Head);
        // Only a frozen fork-point commit → use it as a last resort.
        s.base_commit = Some("abc123".to_string());
        assert_eq!(ReviewBase::of(&s), ReviewBase::Commit("abc123".to_string()));
        // The branch captured at creation wins over the frozen SHA, which rides
        // along as a fallback for when the branch can't be resolved.
        s.base_branch = Some("parent-feature".to_string());
        assert_eq!(
            ReviewBase::of(&s),
            ReviewBase::Branch {
                name: "parent-feature".to_string(),
                fallback: Some("abc123".to_string()),
            }
        );
        // The PR's target branch is authoritative once known, over both.
        s.pr_base_branch = Some("main".to_string());
        assert_eq!(
            ReviewBase::of(&s),
            ReviewBase::Branch {
                name: "main".to_string(),
                fallback: Some("abc123".to_string()),
            }
        );
    }

    #[test]
    fn validate_allows_non_claude_without_flags() {
        let opts = CreateSessionOpts {
            project_path: PathBuf::from("/tmp/repo"),
            title: "test".to_string(),
            program: Some("bash".to_string()),
            initial_prompt: None,
            effort: None,
            mode: None,
            model: None,
            base_branch: None,
            section: None,
            stack_parent: None,
        };
        validate_program_flags(&opts, "bash").unwrap();
    }

    /// A `CommanderService` built over `TempDir`-backed stores must root its
    /// comment/reviewed stores under that temp data dir — never the real
    /// `Config::data_dir()`. Writing a comment proves the on-disk path is the
    /// injected one (test-isolation regression).
    #[tokio::test]
    async fn comment_writes_stay_under_injected_data_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        // Built through the shared helper rather than by hand, so this test
        // inherits every isolation knob it sets (telemetry off, tmux and
        // projects dir pinned) instead of having to repeat them.
        let service = service_with_config(&dir, Config::default());

        // Write a comment through the public API.
        let session_id = SessionId::new();
        service
            .create_comment(
                &session_id,
                NewComment {
                    file: "a.rs".to_string(),
                    side: CommentSide::New,
                    line_range: (1, 1),
                    snippet: "let x = 1;".to_string(),
                    comment: "nit".to_string(),
                },
            )
            .await
            .unwrap();

        // The comment must have landed under the injected temp dir, and the
        // real data dir must be untouched by this write.
        let comments_dir = dir.path().join("comments");
        assert!(
            comments_dir.exists(),
            "comments should be written under the injected data dir"
        );
        if let Ok(real) = Config::data_dir() {
            assert_ne!(
                real,
                dir.path(),
                "temp data dir must differ from the real data dir"
            );
        }
    }

    // -- Workspace-surface service methods (Phase A) --

    use crate::config::storage::AppState as CoreState;
    use crate::config::{ConfigStore, StateStore};

    /// Build a hermetic service over TempDir-backed stores with the given
    /// config. Telemetry is disabled, tmux is isolated onto a throwaway socket
    /// dir, and the projects dir is pinned under `dir`, per the project's
    /// test-isolation rules. The knobs are applied *after* the caller's config
    /// arrives, so an explicitly-passed `Config` can't silently lose them.
    /// Guarded by `service_with_config_pins_projects_dir`.
    fn service_with_config(dir: &tempfile::TempDir, mut config: Config) -> CommanderService {
        config.telemetry.enabled = false;
        let tmux_tmpdir = dir.path().join("tmux");
        std::fs::create_dir_all(&tmux_tmpdir).unwrap();
        config.tmux_tmpdir = Some(tmux_tmpdir);
        // `projects_dir` defaults to the user's REAL `~/Projects`, which the
        // repo-clone paths write into. Pin it so no service built here can
        // clone outside the temp tree.
        config.projects_dir = Some(dir.path().join("projects"));
        // Same for the agent temp files (apply briefs, pasted images), which
        // otherwise land in the real `/tmp`.
        config.agent_temp_dir = Some(dir.path().join("agent-temp"));
        let config_store = Arc::new(ConfigStore::with_path(
            config,
            dir.path().join("config.toml"),
        ));
        let store = Arc::new(StateStore::with_path(
            CoreState::default(),
            dir.path().join("state.json"),
        ));
        CommanderService::new(config_store, store, FrontendInfo::new("test", "0.0.0"))
    }

    fn service(dir: &tempfile::TempDir) -> CommanderService {
        service_with_config(dir, Config::default())
    }

    /// Guard: the core service helper must pin the projects directory into the
    /// temp dir. `projects_dir` defaults to the user's REAL `~/Projects` and the
    /// repo-clone paths write there, so an unpinned helper would check
    /// repositories out into the developer's own projects directory from
    /// `cargo test` / CI. The caller-supplied config must not be able to defeat
    /// the pin either, so this asserts it for an explicitly-passed `Config` too.
    #[test]
    fn service_with_config_pins_projects_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        for config in [
            Config::default(),
            Config {
                projects_dir: Some(PathBuf::from("/definitely/not/a/temp/dir")),
                ..Config::default()
            },
        ] {
            let svc = service_with_config(&dir, config);
            let projects_dir = svc.read_config().projects_dir().unwrap();
            assert!(
                projects_dir.starts_with(dir.path()),
                "core test services must not clone into the real ~/Projects (got {projects_dir:?})"
            );
        }
    }

    /// Seed one project with one session and return their ids.
    async fn seed_project_session(svc: &CommanderService) -> (ProjectId, SessionId) {
        let project = Project::new("repo", PathBuf::from("/tmp/repo"), "main");
        let pid = project.id;
        let session = WorktreeSession::new(
            pid,
            "task",
            "branch-task",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        let sid = session.id;
        svc.store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(session);
            })
            .await
            .unwrap();
        (pid, sid)
    }

    /// Seed a project + session over a real one-commit git repo in `dir`, with
    /// `changed.txt` modified in the working tree and `reverted.txt` left exactly
    /// as committed. The session's review base is the initial commit, so the
    /// composed diff contains `changed.txt` and nothing else.
    async fn seed_review_repo(
        svc: &CommanderService,
        dir: &tempfile::TempDir,
    ) -> (SessionId, PathBuf) {
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = async |args: &[&str]| {
            fixture_git()
                .current_dir(&repo)
                .args(args)
                .output()
                .await
                .unwrap()
        };
        git(&["init", "-q", "-b", "main"]).await;
        git(&["config", "user.email", "t@example.com"]).await;
        git(&["config", "user.name", "T"]).await;
        std::fs::write(repo.join("changed.txt"), "one\n").unwrap();
        std::fs::write(repo.join("reverted.txt"), "stable\n").unwrap();
        git(&["add", "."]).await;
        git(&["commit", "-q", "-m", "base"]).await;
        let head = git(&["rev-parse", "HEAD"]).await;
        let base_commit = String::from_utf8_lossy(&head.stdout).trim().to_string();

        // Only `changed.txt` differs from the base.
        std::fs::write(repo.join("changed.txt"), "two\n").unwrap();

        let project = Project::new("repo", repo.clone(), "main");
        let pid = project.id;
        let mut session = WorktreeSession::new(pid, "task", "branch-task", repo.clone(), "claude");
        session.base_commit = Some(base_commit);
        // Inactive, so `apply_comments` takes the Deferred path and never talks
        // to tmux (these tests are about comment pruning, not delivery).
        session.status = SessionStatus::Stopped;
        let sid = session.id;
        svc.store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(session);
            })
            .await
            .unwrap();
        (sid, repo)
    }

    /// Regression: a comment whose file has left the diff entirely (the change it
    /// was written against was reverted) used to survive as `Drifted` forever —
    /// invisible in the review tree (file rows come from the diff, so it had no
    /// row and no ⚠), unreachable by `d`, and blocking Apply with "review or
    /// delete them" that the user had no way to obey. It must be dropped from the
    /// store, reported, and must not block Apply.
    #[tokio::test]
    async fn open_review_drops_comment_whose_file_left_the_diff() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, _repo) = seed_review_repo(&svc, &dir).await;

        let orphan = svc
            .create_comment(
                &sid,
                NewComment {
                    file: "reverted.txt".to_string(),
                    side: CommentSide::New,
                    line_range: (1, 1),
                    snippet: "gone from the diff".to_string(),
                    comment: "this is unpleasant".to_string(),
                },
            )
            .await
            .unwrap();
        let live = svc
            .create_comment(
                &sid,
                NewComment {
                    file: "changed.txt".to_string(),
                    side: CommentSide::New,
                    line_range: (1, 1),
                    snippet: "two".to_string(),
                    comment: "why two".to_string(),
                },
            )
            .await
            .unwrap();

        let snapshot = svc.open_review(&sid).await.unwrap();

        // The orphan is gone from the snapshot and reported as dropped...
        let ids: Vec<Uuid> = snapshot.comments.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![live], "only the in-diff comment should survive");
        assert_eq!(
            snapshot
                .dropped_comments
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![orphan],
            "the dropped comment must be reported, not silently deleted"
        );
        // ...and the drop is persisted, so it can't come back to block Apply.
        let stored = svc.list_comments(&sid).await.unwrap();
        assert_eq!(stored.iter().map(|c| c.id).collect::<Vec<_>>(), vec![live]);

        // Apply is no longer wedged by the unreachable comment. The session isn't
        // active, so a clean run defers (writes the brief) rather than blocking.
        match svc.apply_comments(&sid).await.unwrap() {
            ApplyOutcome::Deferred { count, .. } => assert_eq!(count, 1),
            other => panic!("expected Deferred, got {other:?}"),
        }
    }

    /// A snapshot carries the composition it was parsed from, so a remote
    /// client can lay the diff out itself (word diff, side-by-side, context
    /// expansion) instead of re-deriving it from the lossier wire model.
    #[tokio::test]
    async fn open_review_carries_the_raw_composition() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, _repo) = seed_review_repo(&svc, &dir).await;

        let snapshot = svc.open_review(&sid).await.unwrap();
        let raw = snapshot.raw.expect("the raw diff must reach the client");
        assert!(
            raw.contains("+++ b/changed.txt"),
            "expected the composed unified diff, got: {raw}"
        );
        // It is the *same* text `content_hash` was taken over, so a client that
        // re-parses it cannot disagree with the server's staleness check.
        assert_eq!(
            xxhash_rust::xxh3::xxh3_64(raw.as_bytes()),
            snapshot.content_hash
        );
    }

    /// The safety precondition for the drop: a diff that fell back to
    /// working-tree-vs-HEAD is a *narrower* view than the session's real base
    /// (committed work is missing from it), so a file's absence there proves
    /// nothing. Pruning against it would delete live comments.
    #[tokio::test]
    async fn degraded_diff_does_not_drop_comments() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, repo) = seed_review_repo(&svc, &dir).await;

        // Commit the change so a HEAD-relative diff is empty, then point the
        // session at a base branch that cannot resolve (and drop the frozen SHA
        // fallback, which `git_ref` would otherwise use). Composition degrades to
        // working-tree-vs-HEAD, in which `changed.txt` no longer appears at all —
        // exactly the false-orphan trap the guard exists for.
        let git = async |args: &[&str]| {
            fixture_git()
                .current_dir(&repo)
                .args(args)
                .output()
                .await
                .unwrap()
        };
        git(&["commit", "-qam", "work"]).await;
        svc.store()
            .mutate(move |state| {
                if let Some(s) = state.sessions.get_mut(&sid) {
                    s.base_branch = Some("no-such-branch".to_string());
                    s.base_commit = None;
                }
            })
            .await
            .unwrap();

        let id = svc
            .create_comment(
                &sid,
                NewComment {
                    file: "changed.txt".to_string(),
                    side: CommentSide::New,
                    line_range: (1, 1),
                    snippet: "two".to_string(),
                    comment: "still live against the real base".to_string(),
                },
            )
            .await
            .unwrap();

        let snapshot = svc.open_review(&sid).await.unwrap();
        assert!(
            snapshot.dropped_comments.is_empty(),
            "a degraded diff must never drop comments"
        );
        assert_eq!(
            svc.list_comments(&sid)
                .await
                .unwrap()
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![id],
            "the comment must survive on disk"
        );
    }

    /// Reviewed marks are keyed on a file's diff content, so the same proof gates
    /// them: on a degraded diff every committed file looks absent, and pruning
    /// would throw away review progress. A stacked base that only exists remotely
    /// degrades on *every* open, so this would recur until the marks were gone.
    #[tokio::test]
    async fn degraded_diff_does_not_prune_reviewed_marks() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, repo) = seed_review_repo(&svc, &dir).await;

        // Mark the file reviewed while the diff is healthy.
        let marked = svc
            .toggle_file_reviewed_by_path(&sid, "changed.txt")
            .await
            .unwrap();
        assert!(marked, "expected the file to become marked");
        assert_eq!(
            svc.open_review(&sid).await.unwrap().reviewed,
            ["changed.txt"]
        );

        // Now degrade: commit the work and point at an unresolvable base, so the
        // composed diff no longer contains `changed.txt` at all.
        fixture_git()
            .current_dir(&repo)
            .args(["commit", "-qam", "work"])
            .output()
            .await
            .unwrap();
        svc.store()
            .mutate(move |state| {
                if let Some(s) = state.sessions.get_mut(&sid) {
                    s.base_branch = Some("no-such-branch".to_string());
                    s.base_commit = None;
                }
            })
            .await
            .unwrap();

        let snapshot = svc.open_review(&sid).await.unwrap();
        assert!(
            snapshot.diff.files.is_empty(),
            "precondition: the degraded diff should be empty here"
        );
        assert!(
            snapshot.reviewed.is_empty(),
            "a file the diff doesn't show can't read as reviewed"
        );
        assert_eq!(
            svc.reviewed_marks(&sid).await.unwrap().len(),
            1,
            "a degraded diff must not prune reviewed marks"
        );
    }

    /// Opening a review with nothing to prune is a plain read: it takes no
    /// lock (so it never waits on a concurrent `review mark`) and leaves no
    /// lock file behind. `ReviewedStore::remove` (which `delete_session` calls)
    /// clears the marks file.
    #[tokio::test]
    async fn open_review_reads_marks_without_locking_and_delete_cleans_up() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, _repo) = seed_review_repo(&svc, &dir).await;
        let reviewed_dir = svc.store().data_dir().join("reviewed");

        svc.open_review(&sid).await.unwrap();
        assert!(
            !reviewed_dir
                .join(format!("{}.lock", sid.as_uuid()))
                .exists(),
            "an open with nothing to prune must not lock"
        );

        svc.set_files_reviewed(&sid, &FileSelection::All, true)
            .await
            .unwrap();
        let marks_file = reviewed_dir.join(format!("{}.json", sid.as_uuid()));
        assert!(marks_file.exists(), "precondition: marks were written");
        svc.reviewed.remove(sid).await.unwrap();
        assert!(!marks_file.exists());
    }

    /// The bulk setter is idempotent (re-running never flips a mark back, which
    /// a toggle would) and reports what it changed.
    #[tokio::test]
    async fn set_files_reviewed_is_idempotent_and_reports_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, _repo) = seed_review_repo(&svc, &dir).await;
        let paths = FileSelection::Paths(vec!["changed.txt".to_string()]);

        let first = svc.set_files_reviewed(&sid, &paths, true).await.unwrap();
        assert_eq!(first.changed, ["changed.txt"]);
        let again = svc.set_files_reviewed(&sid, &paths, true).await.unwrap();
        assert!(again.changed.is_empty());
        assert_eq!(again.unchanged, ["changed.txt"]);
        assert_eq!(
            svc.open_review(&sid).await.unwrap().reviewed,
            ["changed.txt"]
        );
        assert_eq!(
            svc.reviewed_marks(&sid)
                .await
                .unwrap()
                .iter()
                .map(|m| m.file.as_str())
                .collect::<Vec<_>>(),
            ["changed.txt"]
        );

        let cleared = svc
            .set_files_reviewed(&sid, &FileSelection::All, false)
            .await
            .unwrap();
        assert_eq!(cleared.changed, ["changed.txt"]);
        assert!(svc.open_review(&sid).await.unwrap().reviewed.is_empty());
    }

    /// `review files` hands out a hash per file; marking with a hash the file
    /// no longer has (it was edited after being read) is refused, writes
    /// nothing, and `peek_review` reports the file as not reviewed.
    #[tokio::test]
    async fn set_files_reviewed_refuses_a_stale_hash_and_peek_is_hash_checked() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, repo) = seed_review_repo(&svc, &dir).await;
        let read_hash = claude_commander_viewmodel::review::hash_hex(
            &svc.peek_review(&sid).await.unwrap().diff.files[0],
        );

        std::fs::write(repo.join("changed.txt"), "three\n").unwrap();
        let err = svc
            .set_files_reviewed(
                &sid,
                &FileSelection::Paths(vec![format!("changed.txt@{read_hash}")]),
                true,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Changed since"), "{err}");
        assert!(svc.reviewed_marks(&sid).await.unwrap().is_empty());

        // Marked, then edited again: the store still holds the mark, but a
        // read must not call the file reviewed.
        svc.set_files_reviewed(&sid, &FileSelection::All, true)
            .await
            .unwrap();
        std::fs::write(repo.join("changed.txt"), "four\n").unwrap();
        assert!(svc.peek_review(&sid).await.unwrap().reviewed.is_empty());
    }

    /// A view showing an older copy of a file than the one the CLI just
    /// marked shows it unreviewed; the user pressing "mark" there must keep
    /// the mark, where a toggle would have cleared it.
    #[tokio::test]
    async fn set_file_reviewed_by_path_keeps_a_mark_set_elsewhere() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, _repo) = seed_review_repo(&svc, &dir).await;
        svc.set_files_reviewed(&sid, &FileSelection::All, true)
            .await
            .unwrap();

        assert!(
            svc.set_file_reviewed_by_path(&sid, "changed.txt", true)
                .await
                .unwrap()
        );
        assert_eq!(svc.reviewed_marks(&sid).await.unwrap().len(), 1);
        assert!(
            !svc.set_file_reviewed_by_path(&sid, "changed.txt", false)
                .await
                .unwrap()
        );
        assert!(svc.reviewed_marks(&sid).await.unwrap().is_empty());
    }

    /// The marks route a review view polls must tell a deleted session from
    /// one with no marks; an empty list would read as "everything unmarked".
    #[tokio::test]
    async fn reviewed_marks_for_an_unknown_session_is_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let err = svc.reviewed_marks(&SessionId::new()).await.unwrap_err();
        assert!(
            matches!(err, crate::Error::Session(SessionError::NotFound(_))),
            "{err}"
        );
    }

    /// A bulk set naming a path outside the diff writes nothing — a typo must
    /// not leave a half-applied batch behind — and the error names the path.
    #[tokio::test]
    async fn set_files_reviewed_with_an_unknown_path_writes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, _repo) = seed_review_repo(&svc, &dir).await;

        let err = svc
            .set_files_reviewed(
                &sid,
                &FileSelection::Paths(vec!["changed.txt".into(), "nope.txt".into()]),
                true,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope.txt"), "{err}");
        assert!(svc.reviewed_marks(&sid).await.unwrap().is_empty());
    }

    /// Apply must not silently delete. When a file leaves the diff between the
    /// last refresh and pressing Apply, the orphaned comment takes no part —
    /// it neither blocks the live comments nor gets deleted here, because this is
    /// the one path that cannot report a deletion. The next compose drops it and
    /// announces it (see `refresh_after_apply`).
    #[tokio::test]
    async fn apply_skips_orphaned_comments_without_deleting_them() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, repo) = seed_review_repo(&svc, &dir).await;

        // Both files are in the diff when the comments are written.
        std::fs::write(repo.join("reverted.txt"), "temporarily changed\n").unwrap();
        let orphan = svc
            .create_comment(
                &sid,
                NewComment {
                    file: "reverted.txt".to_string(),
                    side: CommentSide::New,
                    line_range: (1, 1),
                    snippet: "temporarily changed".to_string(),
                    comment: "about to be reverted".to_string(),
                },
            )
            .await
            .unwrap();
        let live = svc
            .create_comment(
                &sid,
                NewComment {
                    file: "changed.txt".to_string(),
                    side: CommentSide::New,
                    line_range: (1, 1),
                    snippet: "two".to_string(),
                    comment: "why two".to_string(),
                },
            )
            .await
            .unwrap();
        let snapshot = svc.open_review(&sid).await.unwrap();
        assert!(
            snapshot.dropped_comments.is_empty(),
            "precondition: nothing orphaned yet"
        );

        // The agent reverts one file after the view was composed.
        std::fs::write(repo.join("reverted.txt"), "stable\n").unwrap();

        // Apply proceeds with only the live comment...
        match svc.apply_comments(&sid).await.unwrap() {
            ApplyOutcome::Deferred { count, .. } => {
                assert_eq!(count, 1, "only the live comment should be delivered")
            }
            other => panic!("expected Deferred, got {other:?}"),
        }
        // ...and the orphan is still on disk, so the drop notice can still fire.
        let stored = svc.list_comments(&sid).await.unwrap();
        let orphan_stored = stored.iter().find(|c| c.id == orphan).expect(
            "the orphaned comment must not be deleted by apply — only the reporting \
             path may delete it",
        );
        // Both are still pending: a deferred brief hasn't been delivered, so
        // nothing is marked Applied yet and a re-apply is possible.
        assert_ne!(orphan_stored.status, CommentStatus::Applied);
        assert!(
            stored
                .iter()
                .any(|c| c.id == live && c.status != CommentStatus::Applied)
        );

        // The next compose is what drops it, with a report.
        let after = svc.open_review(&sid).await.unwrap();
        assert_eq!(
            after
                .dropped_comments
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![orphan]
        );
    }

    /// Regression: the brief used to be written to one path per *session*
    /// (`cc-comments-<session>.md`) and overwritten on every Apply, so a second
    /// Apply before the agent had opened the first file replaced its contents —
    /// the first batch of comments was silently lost (and, once delivered, they
    /// were already marked Applied, so nothing brought them back). Each Apply
    /// must leave its own brief on disk, untouched by the next one.
    #[tokio::test]
    async fn each_apply_writes_its_own_brief_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (sid, _repo) = seed_review_repo(&svc, &dir).await;

        let comment = async |text: &str| {
            svc.create_comment(
                &sid,
                NewComment {
                    file: "changed.txt".to_string(),
                    side: CommentSide::New,
                    line_range: (1, 1),
                    snippet: "two".to_string(),
                    comment: text.to_string(),
                },
            )
            .await
            .unwrap()
        };
        let apply = async || match svc.apply_comments(&sid).await.unwrap() {
            ApplyOutcome::Deferred { path, .. } => path,
            other => panic!("expected Deferred, got {other:?}"),
        };

        comment("first note").await;
        let first = apply().await;
        comment("second note").await;
        let second = apply().await;

        assert_ne!(
            first, second,
            "a second Apply must not reuse the first brief's path"
        );
        assert!(
            first.starts_with(dir.path()),
            "briefs must honour the pinned `agent_temp_dir`, not the real /tmp (got {first:?})"
        );
        let first_md = std::fs::read_to_string(&first)
            .expect("the first brief must survive a second Apply, unread or not");
        assert!(
            first_md.contains("first note"),
            "the first brief must keep its own comments, got: {first_md}"
        );
        assert!(
            std::fs::read_to_string(&second)
                .unwrap()
                .contains("second note"),
            "the second brief must carry the newly staged comment"
        );
    }

    #[tokio::test]
    async fn cached_tmux_ok_serves_within_ttl_and_repopulates() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);

        // A fresh cache entry is served verbatim without re-probing tmux. Poking
        // BOTH booleans and getting each back proves a genuine cache hit (the
        // real probe can only match one of them).
        for poked in [true, false] {
            *svc.tmux_ok_cache.lock().unwrap() = Some((std::time::Instant::now(), poked));
            assert_eq!(
                svc.cached_tmux_ok().await,
                poked,
                "a fresh cache entry must be served without re-probing"
            );
        }

        // An empty cache populates a fresh entry.
        *svc.tmux_ok_cache.lock().unwrap() = None;
        let _ = svc.cached_tmux_ok().await;
        let (at, _) = svc
            .tmux_ok_cache
            .lock()
            .unwrap()
            .expect("cache must be populated after a probe");
        assert!(
            at.elapsed() < TMUX_OK_CACHE_TTL,
            "a miss must store a fresh entry"
        );
    }

    #[tokio::test]
    async fn snapshot_carries_projects_and_sessions() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (pid, sid) = seed_project_session(&svc).await;

        let snap = svc.snapshot().await.unwrap();
        assert_eq!(snap.projects.len(), 1);
        assert_eq!(snap.projects[0].id, pid);
        assert_eq!(snap.projects[0].session_ids, vec![sid]);
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].session_id, sid);
        assert_eq!(snap.sessions[0].worktree_path, PathBuf::from("/tmp/wt"));
        assert!(snap.sessions[0].tmux_session_name.starts_with("cc-"));
        assert!(snap.cascade_paused.is_none());
        assert!(snap.pending_comment_sessions.is_empty());
        assert!(snap.project_pull.is_empty());
        assert!(snap.operations.is_empty());
        assert_eq!(snap.server.version, env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn startup_reconcile_drops_stale_creating_sessions() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let project = Project::new("repo", PathBuf::from("/tmp/repo"), "main");
        let pid = project.id;
        // A session left in `Creating` from a crashed previous run.
        let mut creating = WorktreeSession::new(
            pid,
            "half",
            "branch-half",
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        creating.set_status(SessionStatus::Creating);
        let creating_id = creating.id;
        svc.store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(creating);
            })
            .await
            .unwrap();

        svc.startup_reconcile().await.unwrap();

        let state = svc.store().read().await;
        assert!(
            state.get_session(&creating_id).is_none(),
            "stale Creating session should be dropped"
        );
        assert!(state.projects.contains_key(&pid), "project must survive");
    }

    /// Seed a project plus one session pinned to `tmux_name`, with the given
    /// stored status and hibernation marker. Paths live under `dir` so nothing
    /// touches the real filesystem.
    async fn seed_session_for_tmux(
        svc: &CommanderService,
        dir: &tempfile::TempDir,
        tmux_name: &str,
        status: SessionStatus,
        hibernated: bool,
    ) -> SessionId {
        let project = Project::new("repo", dir.path().join("repo"), "main");
        let pid = project.id;
        let mut session =
            WorktreeSession::new(pid, "task", "branch-task", dir.path().join("wt"), "claude");
        session.tmux_session_name = tmux_name.to_string();
        session.set_status(status);
        session.hibernated = hibernated;
        let sid = session.id;
        svc.store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(session);
            })
            .await
            .unwrap();
        sid
    }

    /// Start a real tmux session with a live (long-sleeping) pane on the
    /// service's isolated socket dir. Requires tmux, like the rest of the
    /// tmux-backed tests.
    async fn start_live_pane(svc: &CommanderService, dir: &tempfile::TempDir, tmux_name: &str) {
        svc.session_manager()
            .tmux
            .create_session(tmux_name, dir.path(), Some("sleep 300"))
            .await
            .expect("isolated tmux server should accept a new session");
    }

    #[tokio::test]
    async fn startup_reconcile_revives_stopped_session_with_a_live_pane() {
        // `Stopped` with a live pane is an inconsistency: the only paths that
        // produce Stopped (kill, hibernate, reconcile) all kill the pane first.
        // It happens when a reconcile pass reads tmux as absent — a run pointed
        // at a different socket dir, or `no server running` from a server that
        // restarted — and its cleanup kill then no-ops on that same wrong
        // socket, so live panes keep running while state says stopped.
        // Reconciliation must therefore be bidirectional: without the revive,
        // the row renders `○ stopped` forever and agent-state detection stays
        // suppressed (it is gated on `status.is_active()`), so the spinner and
        // waiting-for-input glyphs never appear again for that session.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let tmux_name = "cc-revive-live";
        start_live_pane(&svc, &dir, tmux_name).await;
        let sid = seed_session_for_tmux(&svc, &dir, tmux_name, SessionStatus::Stopped, true).await;

        svc.startup_reconcile().await.unwrap();

        {
            let state = svc.store().read().await;
            let session = state.get_session(&sid).unwrap();
            assert_eq!(
                session.status,
                SessionStatus::Running,
                "a Stopped session whose tmux pane is alive must be revived to Running"
            );
            assert!(
                !session.hibernated,
                "a live pane leaves nothing to resume into, so the hibernation \
                 marker must be cleared with the revive"
            );
        }
        let _ = svc.session_manager().tmux.kill_session(tmux_name).await;
    }

    #[tokio::test]
    async fn ensure_attachable_marks_a_live_stopped_session_running() {
        // The attach path already probes `session_exists` + `is_pane_dead` and
        // finds the pane alive, but only its *recreate* branch wrote the status
        // back — so attaching to a wrongly-Stopped session left it Stopped, and
        // its glyph stayed `○` while the user sat in the live agent pane.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let tmux_name = "cc-attach-live";
        start_live_pane(&svc, &dir, tmux_name).await;
        let sid = seed_session_for_tmux(&svc, &dir, tmux_name, SessionStatus::Stopped, true).await;

        let resolved = svc
            .session_manager()
            .ensure_attachable(&sid)
            .await
            .expect("a live pane is attachable");
        assert_eq!(resolved, tmux_name, "the live session must be reused as-is");

        {
            let state = svc.store().read().await;
            let session = state.get_session(&sid).unwrap();
            assert_eq!(
                session.status,
                SessionStatus::Running,
                "attaching to a live pane must reflect that in the stored status"
            );
            assert!(!session.hibernated, "a live pane is not hibernated");
        }
        let _ = svc.session_manager().tmux.kill_session(tmux_name).await;
    }

    #[tokio::test]
    async fn startup_reconcile_leaves_a_stopped_session_with_no_tmux_alone() {
        // The other half of the bidirectional rule: absence must not be
        // mistaken for a revive signal, and a genuinely hibernated session must
        // keep its marker so the next wake still resumes its conversation.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let sid = seed_session_for_tmux(
            &svc,
            &dir,
            "cc-no-such-session",
            SessionStatus::Stopped,
            true,
        )
        .await;

        svc.startup_reconcile().await.unwrap();

        let state = svc.store().read().await;
        let session = state.get_session(&sid).unwrap();
        assert_eq!(session.status, SessionStatus::Stopped);
        assert!(
            session.hibernated,
            "a hibernated session with no live pane must keep its resume marker"
        );
    }

    #[tokio::test]
    async fn startup_reconcile_clears_stale_merging_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (_pid, sid) = seed_project_session(&svc).await;
        svc.store()
            .mutate(move |state| {
                state
                    .get_session_mut(&sid)
                    .unwrap()
                    .set_status(SessionStatus::Merging);
            })
            .await
            .unwrap();

        svc.startup_reconcile().await.unwrap();

        // The transient Merging state must be cleared. With no live tmux session
        // the reconcile then marks it Stopped; either way it is no longer stuck
        // spinning in Merging.
        let state = svc.store().read().await;
        assert_ne!(
            state.get_session(&sid).unwrap().status,
            SessionStatus::Merging
        );
    }

    #[tokio::test]
    async fn mark_unread_sets_flag_for_batch() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (_pid, sid) = seed_project_session(&svc).await;
        svc.mark_unread(vec![sid]).await.unwrap();
        let state = svc.store().read().await;
        assert!(state.get_session(&sid).unwrap().unread);
    }

    #[tokio::test]
    async fn apply_pr_results_sets_and_clears_pr_fields() {
        use crate::git::{PrCheckResult, PrInfo};
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (_pid, sid) = seed_project_session(&svc).await;

        let info = PrInfo {
            number: 42,
            url: "https://example/pr/42".to_string(),
            state: crate::git::PrState::Open,
            is_draft: false,
            labels: vec!["x".to_string()],
            review_decision: None,
            reviewers: vec![],
            base_ref_name: Some("main".to_string()),
        };
        svc.apply_pr_results(vec![(sid, PrCheckResult::Found(info))])
            .await
            .unwrap();
        {
            let state = svc.store().read().await;
            let s = state.get_session(&sid).unwrap();
            assert_eq!(s.pr_number, Some(42));
            assert_eq!(s.pr_base_branch.as_deref(), Some("main"));
        }

        // NotFound authoritatively clears; FetchFailed would preserve.
        svc.apply_pr_results(vec![(sid, PrCheckResult::NotFound)])
            .await
            .unwrap();
        let state = svc.store().read().await;
        let s = state.get_session(&sid).unwrap();
        assert!(s.pr_number.is_none());
        assert!(s.pr_base_branch.is_none());
    }

    /// Run `git` in `dir`, panicking on failure. Signing is forced off by
    /// [`fixture_git_std`], so the developer's policy can't break isolated tests.
    fn run_git(dir: &Path, args: &[&str]) {
        let status = fixture_git_std()
            .current_dir(dir)
            .args(["-c", "gc.auto=0"])
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    // -- Workspaces --

    fn one_commit_repo(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let repo = dir.path().join(name);
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "t@t.t"]);
        run_git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        repo
    }

    async fn project_tag(svc: &CommanderService, id: &ProjectId) -> Option<String> {
        svc.store().read().await.projects[id].workspace.clone()
    }

    fn def_names(svc: &CommanderService) -> Vec<String> {
        svc.read_config()
            .workspaces
            .into_iter()
            .map(|d| d.name)
            .collect()
    }

    #[tokio::test]
    async fn moving_a_project_tags_it_and_defines_a_missing_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (pid, _) = seed_project_session(&svc).await;

        svc.set_project_workspace(&pid, Some(" Work ".into()))
            .await
            .unwrap();
        assert_eq!(project_tag(&svc, &pid).await.as_deref(), Some("Work"));
        assert_eq!(
            def_names(&svc),
            ["Work"],
            "the move self-heals the definition"
        );

        // Moving again neither duplicates the definition nor loses the tag.
        svc.set_project_workspace(&pid, Some("Work".into()))
            .await
            .unwrap();
        assert_eq!(def_names(&svc), ["Work"]);

        svc.set_project_workspace(&pid, None).await.unwrap();
        assert_eq!(project_tag(&svc, &pid).await, None, "None is Main");
        assert_eq!(def_names(&svc), ["Work"], "leaving a workspace keeps it");
    }

    #[tokio::test]
    async fn moving_a_project_refuses_bad_names_and_unknown_projects() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (pid, _) = seed_project_session(&svc).await;
        let err = svc
            .set_project_workspace(&pid, Some("last".into()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::Error::Session(SessionError::InvalidName { .. })),
            "{err:?}"
        );
        assert!(
            def_names(&svc).is_empty(),
            "nothing defined for a refused name"
        );
        let err = svc
            .set_project_workspace(&ProjectId::new(), Some("Work".into()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::Error::Session(SessionError::ProjectNotFound(_))),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rename_workspace_rewrites_tags_definition_and_startup_pin() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (pid, _) = seed_project_session(&svc).await;
        svc.set_project_workspace(&pid, Some("Work".into()))
            .await
            .unwrap();
        svc.set_workspace_defs(SetWorkspacesRequest {
            workspaces: vec![WorkspaceDef::named("Work"), WorkspaceDef::named("Play")],
            main: None,
            startup_workspace: Some(StartupWorkspace::Named("Work".into())),
        })
        .unwrap();

        assert!(svc.rename_workspace("Work", "Job").await.unwrap());
        assert_eq!(project_tag(&svc, &pid).await.as_deref(), Some("Job"));
        assert_eq!(def_names(&svc), ["Job", "Play"], "order is kept");
        assert_eq!(
            svc.read_config().startup_workspace,
            StartupWorkspace::Named("Job".into())
        );

        // Idempotent on a host that never had it — eager propagation is safe.
        assert!(!svc.rename_workspace("Nope", "Other").await.unwrap());
        assert!(
            svc.rename_workspace("Job", "Play").await.is_err(),
            "collision"
        );
    }

    /// A tag whose definition was lost (an older binary rewrote config.toml)
    /// still renames, and the new name gets a definition back.
    #[tokio::test]
    async fn rename_workspace_heals_an_orphaned_tag() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (pid, _) = seed_project_session(&svc).await;
        svc.store()
            .mutate(move |s| s.projects.get_mut(&pid).unwrap().workspace = Some("Lost".into()))
            .await
            .unwrap();
        assert!(svc.rename_workspace("Lost", "Found").await.unwrap());
        assert_eq!(project_tag(&svc, &pid).await.as_deref(), Some("Found"));
        assert_eq!(def_names(&svc), ["Found"]);
    }

    #[tokio::test]
    async fn delete_workspace_moves_its_projects_to_main() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (pid, _) = seed_project_session(&svc).await;
        svc.set_project_workspace(&pid, Some("Work".into()))
            .await
            .unwrap();
        svc.set_workspace_defs(SetWorkspacesRequest {
            workspaces: vec![WorkspaceDef::named("Work")],
            main: None,
            startup_workspace: Some(StartupWorkspace::Named("Work".into())),
        })
        .unwrap();

        assert!(svc.delete_workspace("Work").await.unwrap());
        assert_eq!(project_tag(&svc, &pid).await, None);
        assert!(def_names(&svc).is_empty());
        assert_eq!(svc.read_config().startup_workspace, StartupWorkspace::Main);
        assert!(!svc.delete_workspace("Work").await.unwrap(), "idempotent");
    }

    fn seed_workspace_themes(svc: &CommanderService, names: &[&str]) {
        svc.config_store
            .mutate(|c| {
                for (i, name) in names.iter().enumerate() {
                    c.workspace_themes.insert(
                        (*name).to_string(),
                        crate::config::ThemeOverrides {
                            preset: Some(format!("p{i}")),
                            ..Default::default()
                        },
                    );
                }
            })
            .unwrap();
    }

    fn theme_keys(svc: &CommanderService) -> Vec<String> {
        svc.read_config().workspace_themes.into_keys().collect()
    }

    /// Renaming a workspace carries its local theme to the new name, and Main's
    /// entry is untouched.
    #[tokio::test]
    async fn rename_workspace_moves_its_local_theme() {
        use crate::config::MAIN_WORKSPACE_THEME_KEY;
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        svc.set_workspace_defs(SetWorkspacesRequest {
            workspaces: vec![WorkspaceDef::named("Work")],
            main: None,
            startup_workspace: None,
        })
        .unwrap();
        seed_workspace_themes(&svc, &["Work", MAIN_WORKSPACE_THEME_KEY]);
        let before = svc.read_config().workspace_themes["Work"].clone();

        assert!(svc.rename_workspace("Work", "Job").await.unwrap());
        assert_eq!(theme_keys(&svc), ["Job", MAIN_WORKSPACE_THEME_KEY]);
        assert_eq!(svc.read_config().workspace_themes["Job"], before);

        // A refused rename moves nothing.
        svc.set_workspace_defs(SetWorkspacesRequest {
            workspaces: vec![WorkspaceDef::named("Job"), WorkspaceDef::named("Play")],
            main: None,
            startup_workspace: None,
        })
        .unwrap();
        assert!(svc.rename_workspace("Job", "play").await.is_err());
        assert_eq!(theme_keys(&svc), ["Job", MAIN_WORKSPACE_THEME_KEY]);
    }

    /// The theme is local config, so it follows a rename even on a host that
    /// has neither the definition nor a tagged project — the TUI's local
    /// service, say, when the workspace lives only on a remote server.
    #[tokio::test]
    async fn rename_workspace_moves_a_theme_this_host_has_no_definition_for() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        seed_workspace_themes(&svc, &["Remote"]);
        svc.rename_workspace("Remote", "Elsewhere").await.unwrap();
        assert_eq!(theme_keys(&svc), ["Elsewhere"]);
    }

    /// Deleting a workspace drops its theme; Main's reserved key cannot be
    /// renamed or deleted through the workspace paths.
    #[tokio::test]
    async fn delete_workspace_removes_its_local_theme_but_never_mains() {
        use crate::config::MAIN_WORKSPACE_THEME_KEY;
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        svc.set_workspace_defs(SetWorkspacesRequest {
            workspaces: vec![WorkspaceDef::named("Work")],
            main: None,
            startup_workspace: None,
        })
        .unwrap();
        seed_workspace_themes(&svc, &["Work", "Orphan", MAIN_WORKSPACE_THEME_KEY]);

        assert!(svc.delete_workspace("Work").await.unwrap());
        assert_eq!(theme_keys(&svc), ["Orphan", MAIN_WORKSPACE_THEME_KEY]);
        // No definition here, but the local theme still goes.
        svc.delete_workspace("Orphan").await.unwrap();
        assert_eq!(theme_keys(&svc), [MAIN_WORKSPACE_THEME_KEY]);

        let _ = svc.delete_workspace(MAIN_WORKSPACE_THEME_KEY).await;
        let _ = svc
            .rename_workspace(MAIN_WORKSPACE_THEME_KEY, "Stolen")
            .await;
        assert_eq!(theme_keys(&svc), [MAIN_WORKSPACE_THEME_KEY]);
    }

    #[tokio::test]
    async fn set_workspace_defs_validates_and_leaves_unset_fields_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        svc.set_workspace_defs(SetWorkspacesRequest {
            workspaces: vec![WorkspaceDef::named("Work")],
            main: Some(WorkspaceDef::named("Home")),
            startup_workspace: Some(StartupWorkspace::Main),
        })
        .unwrap();
        // A later replace without main/startup keeps both.
        svc.set_workspace_defs(SetWorkspacesRequest {
            workspaces: vec![WorkspaceDef::named("Play")],
            main: None,
            startup_workspace: None,
        })
        .unwrap();
        let c = svc.read_config();
        assert_eq!(def_names(&svc), ["Play"]);
        assert_eq!(c.main_workspace, Some(WorkspaceDef::named("Home")));
        assert_eq!(c.startup_workspace, StartupWorkspace::Main);

        let err = svc
            .set_workspace_defs(SetWorkspacesRequest {
                workspaces: vec![WorkspaceDef::named("A"), WorkspaceDef::named("a")],
                main: None,
                startup_workspace: None,
            })
            .unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Session(SessionError::InvalidName { .. })
        ));
        assert_eq!(def_names(&svc), ["Play"], "a refused list writes nothing");
    }

    /// `main: None` means "leave Main's label alone", not "there is no Main
    /// label": a replace from a client that sends no `main` (every create
    /// and reorder, and any older client) must still be refused
    /// when a name clashes with the label this server stores.
    #[tokio::test]
    async fn set_workspace_defs_checks_names_against_the_stored_main_label() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        svc.set_workspace_defs(SetWorkspacesRequest {
            workspaces: vec![],
            main: Some(WorkspaceDef::named("Home")),
            startup_workspace: None,
        })
        .unwrap();
        let err = svc
            .set_workspace_defs(SetWorkspacesRequest {
                workspaces: vec![WorkspaceDef::named("home")],
                main: None,
                startup_workspace: None,
            })
            .unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Session(SessionError::InvalidName { .. })
        ));
        assert!(def_names(&svc).is_empty(), "a refused list writes nothing");
        assert_eq!(
            svc.read_config().main_workspace,
            Some(WorkspaceDef::named("Home"))
        );
    }

    #[tokio::test]
    async fn snapshot_carries_workspace_config_and_project_tags() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (pid, _) = seed_project_session(&svc).await;
        svc.set_project_workspace(&pid, Some("Work".into()))
            .await
            .unwrap();
        svc.set_workspace_defs(SetWorkspacesRequest {
            workspaces: vec![WorkspaceDef::named("Work")],
            main: Some(WorkspaceDef::named("Home")),
            startup_workspace: Some(StartupWorkspace::Named("Work".into())),
        })
        .unwrap();
        let snap = svc.snapshot().await.unwrap();
        assert_eq!(snap.projects[0].workspace.as_deref(), Some("Work"));
        assert_eq!(snap.workspaces, vec![WorkspaceDef::named("Work")]);
        assert_eq!(snap.main_workspace, Some(WorkspaceDef::named("Home")));
        assert_eq!(
            snap.startup_workspace,
            StartupWorkspace::Named("Work".into())
        );
    }

    #[tokio::test]
    async fn a_new_project_lands_in_the_requested_workspace_but_ensure_never_retags() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let repo = one_commit_repo(&dir, "repo");
        let id = svc
            .ensure_project(repo.clone(), Some("Work".into()))
            .await
            .unwrap();
        assert_eq!(project_tag(&svc, &id).await.as_deref(), Some("Work"));
        assert_eq!(def_names(&svc), ["Work"]);

        let again = svc
            .ensure_project(repo.clone(), Some("Play".into()))
            .await
            .unwrap();
        assert_eq!(again, id);
        assert_eq!(project_tag(&svc, &id).await.as_deref(), Some("Work"));
        assert_eq!(def_names(&svc), ["Work"], "no definition for an unused tag");

        let other = one_commit_repo(&dir, "other");
        let added = svc.add_project(other, None).await.unwrap();
        assert_eq!(project_tag(&svc, &added).await, None);
        assert!(
            svc.add_project(one_commit_repo(&dir, "bad"), Some("main".into()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reconcile_session_branches_adopts_local_rename() {
        // Regression: renaming a worktree's branch (`git branch -m`) left
        // `session.branch` stale, so PR polling queried a branch with no PR and
        // silently dropped the session's PR + stack topology. Reconcile must
        // adopt the live branch when the old name no longer exists.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);

        // A real repo on `main` with one commit, plus a linked worktree on
        // branch `feature`.
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "t@t.t"]);
        run_git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        let wt = dir.path().join("wt-feature");
        run_git(
            &repo,
            &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
        );

        let project = Project::new("repo", repo.clone(), "main");
        let pid = project.id;
        let session = WorktreeSession::new(pid, "feature", "feature", wt.clone(), "claude");
        let sid = session.id;
        svc.store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(session);
            })
            .await
            .unwrap();

        // Rename the branch in the worktree; the old name ceases to exist.
        run_git(&wt, &["branch", "-m", "feature", "renamed-feature"]);

        svc.reconcile_session_branches().await.unwrap();

        let state = svc.store().read().await;
        assert_eq!(
            state.get_session(&sid).unwrap().branch,
            "renamed-feature",
            "a renamed worktree branch must be adopted as the session's branch"
        );
    }

    #[tokio::test]
    async fn reconcile_then_pr_poll_targets_use_new_branch() {
        // The property the whole fix exists to provide: reconciling a rename and
        // then gathering PR-poll targets (as the poll tick does, in order) yields
        // the corrected `--head` branch — so the same tick polls the right PR.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);

        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "t@t.t"]);
        run_git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        let wt = dir.path().join("wt-feature");
        run_git(
            &repo,
            &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
        );

        let project = Project::new("repo", repo.clone(), "main");
        let pid = project.id;
        let session = WorktreeSession::new(pid, "feature", "feature", wt.clone(), "claude");
        let sid = session.id;
        svc.store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(session);
            })
            .await
            .unwrap();

        run_git(&wt, &["branch", "-m", "feature", "renamed-feature"]);

        svc.reconcile_session_branches().await.unwrap();
        let targets = svc.pr_poll_targets().await;

        let target = targets.iter().find(|(id, ..)| *id == sid).unwrap();
        assert_eq!(
            target.1, "renamed-feature",
            "the tick that reconciles must poll GitHub with the corrected branch"
        );
    }

    #[tokio::test]
    async fn pr_poll_targets_carry_branch_ownership_time() {
        // The PR check discards PRs that had already settled on this branch name
        // before the session held it, so the poll must hand it each session's
        // `branch_owned_since` — not just the branch.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);

        let repo = dir.path().join("repo");
        let project = Project::new("repo", repo.clone(), "main");
        let pid = project.id;
        let session = WorktreeSession::new(pid, "feature", "feature", repo.join("wt"), "claude");
        let sid = session.id;
        let owned_since = session.created_at;
        svc.store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(session);
            })
            .await
            .unwrap();

        let targets = svc.pr_poll_targets().await;
        let target = targets.iter().find(|(id, ..)| *id == sid).unwrap();
        assert_eq!(target.3, owned_since);
    }

    #[tokio::test]
    async fn reconcile_stamps_branch_adoption_and_moves_the_pr_window() {
        // Adopting a rename re-points the PR query at a name this session has
        // only just taken. Without stamping the adoption, the poll would still
        // vouch for the whole span back to session creation, so an unrelated PR
        // that settled on the new name during that span would be adopted.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);

        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "t@t.t"]);
        run_git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        let wt = dir.path().join("wt-feature");
        run_git(
            &repo,
            &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
        );

        let project = Project::new("repo", repo.clone(), "main");
        let pid = project.id;
        let mut session = WorktreeSession::new(pid, "feature", "feature", wt.clone(), "claude");
        // Backdate creation so the adoption stamp is unambiguously later.
        session.created_at = Utc::now() - chrono::Duration::days(30);
        let sid = session.id;
        let created_at = session.created_at;
        svc.store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(session);
            })
            .await
            .unwrap();

        run_git(&wt, &["branch", "-m", "feature", "renamed-feature"]);
        svc.reconcile_session_branches().await.unwrap();

        let adopted_at = {
            let state = svc.store().read().await;
            state.get_session(&sid).unwrap().branch_adopted_at
        };
        let adopted_at = adopted_at.expect("adopting a rename must stamp branch_adopted_at");
        assert!(
            adopted_at > created_at,
            "the adoption stamp must be the moment of adoption, not creation"
        );

        let targets = svc.pr_poll_targets().await;
        let target = targets.iter().find(|(id, ..)| *id == sid).unwrap();
        assert_eq!(target.1, "renamed-feature");
        assert_eq!(
            target.3, adopted_at,
            "the poll must only vouch for PRs settling after the rename was adopted"
        );
    }

    #[tokio::test]
    async fn reconcile_session_branches_ignores_plain_switch() {
        // Switching a worktree to another *existing* branch is transient — the
        // session must stay anchored to its original branch, not follow.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);

        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "t@t.t"]);
        run_git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        let wt = dir.path().join("wt-feature");
        run_git(
            &repo,
            &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
        );
        // A second branch exists to switch to; `feature` is kept.
        run_git(&wt, &["branch", "other"]);

        let project = Project::new("repo", repo.clone(), "main");
        let pid = project.id;
        let session = WorktreeSession::new(pid, "feature", "feature", wt.clone(), "claude");
        let sid = session.id;
        svc.store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(session);
            })
            .await
            .unwrap();

        run_git(&wt, &["switch", "other"]);

        svc.reconcile_session_branches().await.unwrap();

        let state = svc.store().read().await;
        assert_eq!(
            state.get_session(&sid).unwrap().branch,
            "feature",
            "a plain switch to an existing branch must not move the session"
        );
    }

    #[tokio::test]
    async fn snapshot_reports_paused_cascade() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (_pid, sid) = seed_project_session(&svc).await;
        svc.store()
            .mutate(move |state| state.cascade_paused_at = Some(sid))
            .await
            .unwrap();
        let snap = svc.snapshot().await.unwrap();
        assert_eq!(snap.cascade_paused, Some(sid));
    }

    #[tokio::test]
    async fn list_projects_sorted_by_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        svc.store()
            .mutate(|state| {
                state.add_project(Project::new("zzz", PathBuf::from("/z"), "main"));
                state.add_project(Project::new("aaa", PathBuf::from("/a"), "main"));
            })
            .await
            .unwrap();
        let projects = svc.list_projects().await;
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "aaa");
        assert_eq!(projects[1].name, "zzz");
    }

    #[tokio::test]
    async fn create_options_from_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = Config {
            default_program: Some("claude".to_string()),
            programs: vec![crate::config::ProgramEntry {
                label: "Claude (Opus)".to_string(),
                command: "claude --model opus".to_string(),
            }],
            sections: vec![crate::session::SectionConfig {
                name: "Open PRs".to_string(),
                ..Default::default()
            }],
            ..Config::default()
        };
        let svc = service_with_config(&dir, config);
        let opts = svc.create_options();
        // Post-programs-migration semantics: the first configured program is
        // the default; the legacy `default_program` scalar is only a fallback.
        assert_eq!(opts.default_program, "claude --model opus");
        assert_eq!(opts.programs.len(), 1);
        assert_eq!(opts.programs[0].label, "Claude (Opus)");
        assert_eq!(opts.sections, vec!["Open PRs".to_string()]);
    }

    #[tokio::test]
    async fn set_programs_persists_and_reflects_in_create_options() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        // Default config has no configured programs; create_options reports the raw
        // (empty) list — the `claude` fallback lives in the picker, not here.
        assert_eq!(svc.create_options().programs.len(), 0);

        svc.set_programs(vec![
            crate::config::ProgramEntry {
                label: "Claude (Opus)".to_string(),
                command: "claude --model opus".to_string(),
            },
            crate::config::ProgramEntry {
                label: "Shell".to_string(),
                command: "bash".to_string(),
            },
        ])
        .unwrap();

        let opts = svc.create_options();
        assert_eq!(opts.programs.len(), 2);
        assert_eq!(opts.programs[0].command, "claude --model opus");
        assert_eq!(opts.programs[1].label, "Shell");
        // First configured program is the default.
        assert_eq!(opts.default_program, "claude --model opus");

        // The change is durable: it was written to the config file on disk.
        let toml = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(toml.contains("claude --model opus"));

        // An empty list is accepted and persists as empty.
        svc.set_programs(vec![]).unwrap();
        assert_eq!(svc.create_options().programs.len(), 0);
    }

    #[tokio::test]
    async fn rename_session_updates_title() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (_pid, sid) = seed_project_session(&svc).await;
        svc.rename_session(&sid, "renamed").await.unwrap();
        let state = svc.store().read().await;
        assert_eq!(state.get_session(&sid).unwrap().title, "renamed");
    }

    #[tokio::test]
    async fn rename_session_rejects_empty_title() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (_pid, sid) = seed_project_session(&svc).await;
        let err = svc.rename_session(&sid, "   ").await.unwrap_err();
        assert!(err.to_string().contains("empty"));
        // The original title is untouched.
        let state = svc.store().read().await;
        assert_eq!(state.get_session(&sid).unwrap().title, "task");
    }

    #[tokio::test]
    async fn rename_missing_session_is_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let err = svc
            .rename_session(&SessionId::new(), "x")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Session(SessionError::NotFound(_))
        ));
    }

    /// The happy path: a session with no PR is restacked onto a sibling, and the
    /// outcome reports there was no PR to retarget.
    #[tokio::test]
    async fn set_session_base_restacks_and_reports_no_pr() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (pid, sid) = seed_project_session(&svc).await;
        let target = WorktreeSession::new(
            pid,
            "other",
            "branch-other",
            PathBuf::from("/tmp/wt2"),
            "claude",
        );
        let target_id = target.id;
        svc.store()
            .mutate(move |state| state.add_session(target))
            .await
            .unwrap();

        let outcome = svc.set_session_base(&sid, Some(target_id)).await.unwrap();
        assert_eq!(outcome.new_base_branch, "branch-other");
        assert_eq!(outcome.pr, PrRetarget::NoPr);

        let state = svc.store().read().await;
        let s = state.get_session(&sid).unwrap();
        assert_eq!(s.stack_parent_session_id, Some(target_id));
        assert_eq!(s.base_branch.as_deref(), Some("branch-other"));
    }

    /// An unknown session must keep the API's 404 semantics rather than falling
    /// through the rejection enum's 400/409 mapping.
    #[tokio::test]
    async fn set_session_base_on_missing_session_is_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let err = svc
            .set_session_base(&SessionId::new(), None)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Session(SessionError::NotFound(_))
        ));
    }

    /// A refused retarget surfaces as `InvalidBase` and writes nothing — the
    /// whole validation runs inside the `try_mutate` closure, which discards the
    /// state when the closure errors.
    #[tokio::test]
    async fn set_session_base_rejection_surfaces_and_writes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (_pid, sid) = seed_project_session(&svc).await;

        let err = svc.set_session_base(&sid, Some(sid)).await.unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Session(SessionError::InvalidBase(
                crate::session::SetBaseRejection::SelfParent
            ))
        ));

        let state = svc.store().read().await;
        assert!(state.get_session(&sid).unwrap().base_branch.is_none());
    }

    /// Characterisation: when `gh pr edit` fails, the next PR poll re-asserts
    /// GitHub's base over the local mirror while `base_branch` keeps the new
    /// value. This is exactly why a failed retarget is reported to the user
    /// rather than logged — pinned here so the divergence cannot go unnoticed.
    #[tokio::test]
    async fn pr_poll_reverts_the_mirror_after_a_failed_retarget() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (pid, sid) = seed_project_session(&svc).await;
        let target = WorktreeSession::new(
            pid,
            "other",
            "branch-other",
            PathBuf::from("/tmp/wt2"),
            "claude",
        );
        let target_id = target.id;
        svc.store()
            .mutate(move |state| {
                state.add_session(target);
                let s = state.get_session_mut(&sid).unwrap();
                s.pr_number = Some(7);
                s.pr_state = Some(crate::git::PrState::Open);
                s.pr_base_branch = Some("main".to_string());
            })
            .await
            .unwrap();

        // The retarget applies locally regardless of whether `gh` is reachable.
        svc.set_session_base(&sid, Some(target_id)).await.unwrap();
        assert_eq!(
            svc.store()
                .read()
                .await
                .get_session(&sid)
                .unwrap()
                .pr_base_branch
                .as_deref(),
            Some("branch-other")
        );

        // GitHub still reports the old base (the edit never landed).
        svc.apply_pr_results(vec![(
            sid,
            crate::git::PrCheckResult::Found(crate::git::PrInfo {
                number: 7,
                url: "https://example.invalid/pr/7".to_string(),
                state: crate::git::PrState::Open,
                is_draft: false,
                labels: Vec::new(),
                review_decision: None,
                reviewers: Vec::new(),
                base_ref_name: Some("main".to_string()),
            }),
        )])
        .await
        .unwrap();

        let state = svc.store().read().await;
        let s = state.get_session(&sid).unwrap();
        assert_eq!(
            s.pr_base_branch.as_deref(),
            Some("main"),
            "the poll re-asserts GitHub's base over the local mirror"
        );
        assert_eq!(
            s.base_branch.as_deref(),
            Some("branch-other"),
            "while base_branch keeps the retargeted value — the two diverge"
        );
    }

    #[tokio::test]
    async fn set_section_sets_override_and_clears_it() {
        let dir = tempfile::TempDir::new().unwrap();
        // A predicate-less section is a valid manual override target.
        let config = Config {
            sections: vec![crate::session::SectionConfig {
                name: "Parking".to_string(),
                ..Default::default()
            }],
            ..Config::default()
        };
        let svc = service_with_config(&dir, config);
        let (_pid, sid) = seed_project_session(&svc).await;

        svc.set_section(&sid, Some("Parking".to_string()))
            .await
            .unwrap();
        {
            let state = svc.store().read().await;
            let s = state.get_session(&sid).unwrap();
            assert_eq!(s.section_override.as_deref(), Some("Parking"));
            assert_eq!(s.current_section.as_deref(), Some("Parking"));
        }

        svc.set_section(&sid, None).await.unwrap();
        {
            let state = svc.store().read().await;
            let s = state.get_session(&sid).unwrap();
            assert!(s.section_override.is_none());
        }
    }

    /// Config with a single predicate-less section, a valid manual-override target.
    fn config_with_section(name: &str) -> Config {
        Config {
            sections: vec![crate::session::SectionConfig {
                name: name.to_string(),
                ..Default::default()
            }],
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn rename_section_carries_its_sessions_across() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service_with_config(&dir, config_with_section("Parking"));
        let (_pid, sid) = seed_project_session(&svc).await;
        svc.set_section(&sid, Some("Parking".to_string()))
            .await
            .unwrap();
        let stamp = {
            let state = svc.store().read().await;
            state.get_session(&sid).unwrap().entered_section_at
        };

        assert!(svc.rename_section("Parking", "Waiting").await.unwrap());

        assert_eq!(
            svc.read_config()
                .sections
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            vec!["Waiting".to_string()]
        );
        let state = svc.store().read().await;
        let s = state.get_session(&sid).unwrap();
        assert_eq!(s.current_section.as_deref(), Some("Waiting"));
        assert_eq!(s.section_override.as_deref(), Some("Waiting"));
        assert_eq!(s.entered_section_at, stamp, "a relabel is not a move");
    }

    #[tokio::test]
    async fn rename_section_rejects_blank_unknown_duplicate_and_reserved_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut config = config_with_section("Parking");
        config.sections.push(crate::session::SectionConfig {
            name: "Drafts".to_string(),
            ..Default::default()
        });
        let svc = service_with_config(&dir, config);
        let (_pid, sid) = seed_project_session(&svc).await;
        svc.set_section(&sid, Some("Parking".to_string()))
            .await
            .unwrap();

        for (old, new) in [
            ("Parking", "   "),
            ("Parking", "Parking"),
            ("Parking", "Drafts"),
            ("Parking", crate::session::IN_PROGRESS),
            ("Nonexistent", "Waiting"),
        ] {
            assert!(
                !svc.rename_section(old, new).await.unwrap(),
                "{old:?} → {new:?} should be refused"
            );
        }

        // Config and the session's placement are both untouched.
        assert_eq!(
            svc.read_config()
                .sections
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            vec!["Parking".to_string(), "Drafts".to_string()]
        );
        let state = svc.store().read().await;
        assert_eq!(
            state.get_session(&sid).unwrap().current_section.as_deref(),
            Some("Parking")
        );
    }

    #[tokio::test]
    async fn mark_read_clears_unread() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (_pid, sid) = seed_project_session(&svc).await;
        svc.store()
            .mutate(move |state| {
                state.get_session_mut(&sid).unwrap().unread = true;
            })
            .await
            .unwrap();
        svc.mark_read(&sid).await.unwrap();
        let state = svc.store().read().await;
        assert!(!state.get_session(&sid).unwrap().unread);
    }

    #[tokio::test]
    async fn operation_ledger_records_and_caps() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        // Record more than the cap; the ledger keeps only the newest CAP entries.
        for i in 0..(OPERATION_LEDGER_CAP + 5) {
            svc.record_operation(
                OperationKind::Cascade,
                OperationOutcome::Succeeded {
                    detail: format!("op {i}"),
                },
            );
        }
        let ops = svc.operations_snapshot();
        assert_eq!(ops.len(), OPERATION_LEDGER_CAP);
        // Ids are monotonic; the oldest survivors dropped the first 5.
        assert_eq!(ops.first().unwrap().id, 6);
        assert!(ops.last().unwrap().id > ops.first().unwrap().id);
    }

    #[tokio::test]
    async fn record_cascade_outcome_maps_variants() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let at = SessionId::new();
        let paused = svc.record_cascade_outcome(Ok(CascadeOutcome::PausedOnConflict {
            at,
            sessions_merged: 2,
        }));
        assert!(matches!(paused.outcome, OperationOutcome::Paused { .. }));
        let done = svc.record_cascade_outcome(Ok(CascadeOutcome::Complete { sessions_merged: 3 }));
        match done.outcome {
            OperationOutcome::Succeeded { detail } => assert!(detail.contains('3')),
            other => panic!("expected Succeeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_pr_refresh_notifies_without_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        // With no loop running the notification is simply dropped; the call must
        // still succeed (a manual refresh before startup is harmless).
        assert!(svc.request_pr_refresh().is_ok());
    }

    // -- Background-loop helpers + caches (Phase D) --

    #[test]
    fn unread_transition_working_to_idle_is_flagged() {
        let sid = SessionId::new();
        let prev = BTreeMap::from([(sid, AgentState::Working)]);
        let new = BTreeMap::from([(sid, AgentState::Idle)]);
        assert_eq!(detect_unread_transitions(&prev, &new), vec![sid]);
    }

    #[test]
    fn unread_transition_idle_to_idle_is_not_flagged() {
        let sid = SessionId::new();
        let prev = BTreeMap::from([(sid, AgentState::Idle)]);
        let new = BTreeMap::from([(sid, AgentState::Idle)]);
        assert!(detect_unread_transitions(&prev, &new).is_empty());
    }

    #[test]
    fn unread_transition_empty_prev_is_not_flagged() {
        // A cleared baseline (never polled, or reset after an attach) must not
        // fabricate an unread from the first observation.
        let sid = SessionId::new();
        let new_idle = BTreeMap::from([(sid, AgentState::Idle)]);
        let new_working = BTreeMap::from([(sid, AgentState::Working)]);
        assert!(detect_unread_transitions(&BTreeMap::new(), &new_idle).is_empty());
        assert!(detect_unread_transitions(&BTreeMap::new(), &new_working).is_empty());
    }

    #[test]
    fn unread_transition_flags_commander_sentinel_so_loop_must_filter() {
        // The detector treats the sentinel like any other id, so a commander
        // Working→Idle WOULD be reported; the poll loop filters it out (the
        // commander has no `WorktreeSession` to mark unread).
        let sentinel = claude_commander_protocol::session::COMMANDER_SENTINEL_ID;
        let prev = BTreeMap::from([(sentinel, AgentState::Working)]);
        let new = BTreeMap::from([(sentinel, AgentState::Idle)]);
        assert_eq!(detect_unread_transitions(&prev, &new), vec![sentinel]);
    }

    #[test]
    fn poll_tick_skip_and_send_decisions() {
        // Skip only when there's nothing to detect and the commander's running
        // state is unchanged.
        assert!(poll_tick_can_skip(true, false, false));
        assert!(!poll_tick_can_skip(true, true, true));
        assert!(!poll_tick_can_skip(false, true, true));
        // A commander flip (either edge) must not be skipped.
        assert!(!poll_tick_can_skip(true, false, true));
        assert!(!poll_tick_can_skip(true, true, false));

        // Send on fresh states, or on a commander flip (so the chip can turn
        // off on the trailing edge); stay quiet otherwise.
        assert!(poll_tick_should_send(false, false, false));
        assert!(poll_tick_should_send(true, true, false));
        assert!(poll_tick_should_send(true, false, true));
        assert!(!poll_tick_should_send(true, true, true));
        assert!(!poll_tick_should_send(true, false, false));
    }

    #[test]
    fn reconciled_status_syncs_both_directions() {
        use PaneProbe::*;
        use SessionStatus::*;

        // Gone or exited ⇒ Stopped.
        assert_eq!(reconciled_status(Running, Absent), Some(Stopped));
        assert_eq!(reconciled_status(Running, Dead), Some(Stopped));
        // …and back again: a live pane under a Stopped record is the
        // inconsistency this reconciler exists to repair.
        assert_eq!(reconciled_status(Stopped, Live), Some(Running));

        // Already agreeing: no write, no change-feed churn.
        assert_eq!(reconciled_status(Running, Live), None);
        assert_eq!(reconciled_status(Stopped, Absent), None);
        assert_eq!(reconciled_status(Stopped, Dead), None);
    }

    #[test]
    fn reconciled_status_never_acts_on_a_tmux_failure() {
        // A crashed/timed-out tmux command says nothing about the session. Acting
        // on it is what mass-stops live sessions, so every stored status must
        // survive an Unknown probe untouched.
        for stored in [
            SessionStatus::Running,
            SessionStatus::Stopped,
            SessionStatus::Creating,
            SessionStatus::Merging,
            SessionStatus::Pushing,
            SessionStatus::CascadePaused,
        ] {
            assert_eq!(
                reconciled_status(stored, PaneProbe::Unknown),
                None,
                "{stored:?} must be left alone when tmux itself failed"
            );
        }
    }

    #[test]
    fn reconciled_status_leaves_creating_and_cascade_states_alone() {
        // A `Creating` session's pane does not exist *yet* — stale records are
        // dropped by step 1 of the reconcile, not stopped here.
        assert_eq!(
            reconciled_status(SessionStatus::Creating, PaneProbe::Absent),
            None
        );
        assert_eq!(
            reconciled_status(SessionStatus::Creating, PaneProbe::Live),
            None
        );
        // The transient cascade states already imply a live pane and carry
        // meaning beyond "running" (step 2 resets the stale ones), so a live
        // probe must not flatten them to Running.
        for stored in [
            SessionStatus::Merging,
            SessionStatus::Pushing,
            SessionStatus::CascadePaused,
        ] {
            assert_eq!(reconciled_status(stored, PaneProbe::Live), None);
            // But a vanished pane still stops them — that is how a crashed
            // mid-cascade session stops spinning.
            assert_eq!(
                reconciled_status(stored, PaneProbe::Absent),
                Some(SessionStatus::Stopped)
            );
        }
    }

    #[test]
    fn pr_check_debounce_allows_first_and_blocks_bursts() {
        let base = std::time::Instant::now();
        let debounce = Duration::from_secs(2);
        assert!(pr_check_debounce_passed(None, base, debounce));
        assert!(!pr_check_debounce_passed(
            Some(base),
            base + Duration::from_millis(500),
            debounce
        ));
        assert!(pr_check_debounce_passed(
            Some(base),
            base + Duration::from_secs(2),
            debounce
        ));
    }

    #[test]
    fn with_commander_target_appends_sentinel_only_when_running() {
        // The poll loop and the fresh rebuild both include the commander's
        // sentinel target when it's live, so a fresh `agent_states(true)` after
        // an attach carries the commander's own state (no one-tick chip blink).
        let sentinel = claude_commander_protocol::session::COMMANDER_SENTINEL_ID;

        let running = with_commander_target(Vec::new(), true, "claude");
        assert_eq!(running.len(), 1, "the sentinel target must be appended");
        assert_eq!(running[0].0, sentinel);
        assert_eq!(running[0].1, crate::commander::COMMANDER_TMUX_NAME);
        assert_eq!(running[0].2, "claude", "the commander program is carried");

        assert!(
            with_commander_target(Vec::new(), false, "claude").is_empty(),
            "no sentinel when the commander isn't running"
        );
    }

    #[tokio::test]
    async fn agent_states_fresh_recomputes_commander_running_into_cache() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        // Seed a stale `commander_running = true` in the cache, as the poll loop
        // might have left it while the commander was up. The test config has the
        // commander disabled, so the correct recomputed value is `false`.
        svc.agent_states_cache.write().await.commander_running = true;

        // A fresh call re-detects state AND recomputes commander liveness the
        // same way the poll loop does, folding the corrected flag into the
        // cache — rather than rewriting only `states` and leaving the stale
        // `commander_running` untouched.
        let fresh = svc.agent_states(true).await;
        assert!(fresh.states.is_empty());
        assert!(
            !fresh.commander_running,
            "fresh must recompute commander_running (disabled → false)"
        );
        assert!(
            !svc.agent_states_cache.read().await.commander_running,
            "the corrected flag must be folded into the cache"
        );

        // Once primed, the non-fresh read is served from the cache and so
        // reflects the corrected commander_running.
        let after = svc.agent_states(false).await;
        assert!(!after.commander_running);
    }

    #[tokio::test]
    async fn agent_states_unprimed_reports_commander_running_honestly() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        // No background loop has run and no fresh call has primed the cache, so
        // this exercises the on-demand fallback arm. With the commander disabled
        // in the test config it must report `commander_running = false`, not the
        // old hardcoded `true`.
        assert!(
            !svc.agent_states_primed
                .load(std::sync::atomic::Ordering::Relaxed),
            "precondition: the cache must be unprimed"
        );
        let snap = svc.agent_states(false).await;
        assert!(
            !snap.commander_running,
            "unprimed fallback must compute commander_running (disabled → false)"
        );
    }

    #[tokio::test]
    async fn clear_stale_agent_states_empties_cache_and_wakes_feed() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        // Seed the cache as if a running session had been detected on a prior
        // tick (the state the quiet path would otherwise leave behind).
        svc.agent_states_cache
            .write()
            .await
            .states
            .insert(SessionId::new(), AgentState::Working);
        let gen_before = svc.store.generation();

        // The last running session stopped: the quiet tick clears the cache once
        // and wakes the change-feed so clients don't see ghosts.
        assert!(svc.clear_stale_agent_states().await, "cleared stale states");
        assert!(svc.agent_states_cache.read().await.states.is_empty());
        assert!(
            svc.store.generation() > gen_before,
            "clearing must wake the change-feed"
        );

        // Idempotent: a second quiet tick on an already-empty cache is a no-op.
        let gen_after = svc.store.generation();
        assert!(!svc.clear_stale_agent_states().await);
        assert_eq!(
            svc.store.generation(),
            gen_after,
            "no redundant wake on an already-empty cache"
        );
    }

    #[tokio::test]
    async fn spawn_background_tasks_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let first = svc.spawn_background_tasks(BackgroundOpts::default());
        assert!(!first.is_empty(), "first spawn starts the loops");
        let second = svc.spawn_background_tasks(BackgroundOpts::default());
        assert!(second.is_empty(), "second spawn is a no-op");
        first.abort();
    }

    #[tokio::test]
    async fn snapshot_surfaces_project_pull_cache() {
        use crate::api::{PullBlockReason, PullStatus};
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let (pid, _sid) = seed_project_session(&svc).await;
        // The pull loop maintains this cache; inject an outcome directly to prove
        // `snapshot` surfaces it in `project_pull`.
        svc.pull_status.lock().unwrap().insert(
            pid,
            PullStatus::Blocked {
                reason: PullBlockReason::Dirty,
            },
        );
        let snap = svc.snapshot().await.unwrap();
        assert_eq!(
            snap.project_pull.get(&pid),
            Some(&PullStatus::Blocked {
                reason: PullBlockReason::Dirty
            })
        );
    }

    // -- In-session switcher revive (ensure_attachable_by_tmux_name) --

    #[tokio::test]
    async fn ensure_attachable_by_tmux_name_unknown_name_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let err = svc
            .ensure_attachable_by_tmux_name("cc-deadbeef")
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                crate::Error::Session(SessionError::TmuxSessionNotFound(ref n))
                    if n == "cc-deadbeef"
            ),
            "unknown tmux name should be a TmuxSessionNotFound error, got: {err}"
        );
    }

    /// Resolution must accept either of a session's tmux names (primary or
    /// shell pair) and reach the id-based attach validation: a `Creating`
    /// session can't attach, so `InvalidState` for the right id proves the
    /// shell-pair name resolved to the session before any tmux command ran.
    #[tokio::test]
    async fn ensure_attachable_by_tmux_name_resolves_shell_pair_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);

        let project = Project::new("repo", PathBuf::from("/tmp/repo"), "main");
        let mut session =
            WorktreeSession::new_creating(project.id, "task", "branch-task", "claude");
        session.shell_tmux_session_name = Some("cc-task-sh".to_string());
        let sid = session.id;
        svc.store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(session);
            })
            .await
            .unwrap();

        let err = svc
            .ensure_attachable_by_tmux_name("cc-task-sh")
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                crate::Error::Session(SessionError::InvalidState(id)) if id == sid
            ),
            "shell-pair name should resolve to the session and hit its attach guard, got: {err}"
        );
    }

    // -- Repository clone (start_clone / clone_job) --

    /// Seed a bare repo with one commit on `main` inside `dir`, network-free, and
    /// return its **plain local path** — what a user pasting a path into the
    /// clone box gives us, and what `validate_clone_url` admits absolute local
    /// paths for.
    ///
    /// A second copy of `git::clone`'s fixture rather than a shared one: both live
    /// in `#[cfg(test)]` modules, so neither is reachable from the other.
    fn seed_bare_repo(dir: &tempfile::TempDir) -> PathBuf {
        let remote = dir.path().join("remote.git");
        let seed = dir.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        run_git(dir.path(), &["init", "--bare", "-b", "main", "remote.git"]);
        run_git(&seed, &["init", "-b", "main"]);
        run_git(&seed, &["config", "user.email", "t@t.t"]);
        run_git(&seed, &["config", "user.name", "t"]);
        std::fs::write(seed.join("README"), "v1\n").unwrap();
        run_git(&seed, &["add", "."]);
        run_git(&seed, &["commit", "-m", "initial"]);
        run_git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run_git(&seed, &["push", "origin", "main"]);
        remote
    }

    /// Poll a clone job until it leaves `Running`, bounded so a job that never
    /// finishes fails the test instead of hanging it.
    ///
    /// A short sleep rather than the bare `yield_now` spin `CloneJobs`' own tests
    /// use: these jobs wait on a `git` **subprocess**, and a yield loop can burn
    /// its whole iteration budget in milliseconds while the child is still
    /// starting up. The budget below is ~10s of real time, reached only by a
    /// genuinely stuck job.
    async fn await_clone(svc: &CommanderService, id: CloneJobId) -> CloneJob {
        for _ in 0..2_000 {
            let job = svc.clone_job(id).await.expect("job disappeared");
            if !matches!(job.status, CloneStatus::Running) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("clone job {id} never reached a terminal status");
    }

    fn url_source(path: &Path) -> CloneSource {
        CloneSource::Url {
            url: path.to_string_lossy().into_owned(),
        }
    }

    /// The whole point of the flow: a finished clone is a *registered project*,
    /// checked out under the configured projects dir, not just a directory on
    /// disk.
    #[tokio::test]
    async fn clone_registers_the_result_as_a_project() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let remote = seed_bare_repo(&dir);

        let id = svc
            .start_clone(CloneRequest {
                source: url_source(&remote),
                dest_name: None,
                workspace: None,
            })
            .await
            .unwrap()
            .id;

        let job = await_clone(&svc, id).await;
        let CloneStatus::Succeeded { project_id } = job.status else {
            panic!("{:?}", job.status)
        };
        // The destination is derived from the source (`remote.git` → `remote`)
        // and sits under the pinned projects dir.
        let expected = svc.read_config().projects_dir().unwrap().join("remote");
        assert_eq!(job.dest, expected);
        assert_eq!(
            std::fs::read_to_string(expected.join("README")).unwrap(),
            "v1\n"
        );
        let projects = svc.list_projects().await;
        assert!(
            projects.iter().any(|p| p.id == project_id),
            "the clone was not registered as a project: {projects:?}"
        );
    }

    /// `dest_name` renames the checkout directory; nothing else about the clone
    /// changes.
    #[tokio::test]
    async fn a_destination_name_override_is_honoured() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let remote = seed_bare_repo(&dir);

        let id = svc
            .start_clone(CloneRequest {
                source: url_source(&remote),
                dest_name: Some("renamed".to_string()),
                workspace: None,
            })
            .await
            .unwrap()
            .id;

        let job = await_clone(&svc, id).await;
        assert!(matches!(job.status, CloneStatus::Succeeded { .. }));
        assert_eq!(
            job.dest,
            svc.read_config().projects_dir().unwrap().join("renamed")
        );
    }

    /// An occupied destination is a *terminal job*, not a started clone: the
    /// frontend gets `DestinationExists` with the flag that decides whether the
    /// sensible offer is "register the existing checkout" or "pick another name".
    #[tokio::test]
    async fn an_occupied_destination_short_circuits_without_cloning() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let remote = seed_bare_repo(&dir);
        let projects_dir = svc.read_config().projects_dir().unwrap();

        // A plain directory with a file in it: occupied, not a repo.
        let plain = projects_dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("precious"), "mine\n").unwrap();

        // An existing git checkout at the destination.
        let checkout = projects_dir.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        run_git(&checkout, &["init", "-b", "main"]);

        for (name, is_git_repo) in [("plain", false), ("checkout", true)] {
            let id = svc
                .start_clone(CloneRequest {
                    source: url_source(&remote),
                    dest_name: Some(name.to_string()),
                    workspace: None,
                })
                .await
                .unwrap()
                .id;
            let job = await_clone(&svc, id).await;
            assert_eq!(
                job.status,
                CloneStatus::DestinationExists {
                    dest: projects_dir.join(name),
                    is_git_repo
                },
                "name: {name}"
            );
        }

        // Nothing was cloned over either directory, and no project was created.
        assert_eq!(
            std::fs::read_to_string(plain.join("precious")).unwrap(),
            "mine\n"
        );
        assert!(!plain.join(".git").exists());
        assert!(svc.list_projects().await.is_empty());
    }

    /// A failed clone must leave nothing behind, so a retry into the same
    /// directory is not blocked by a half-written checkout.
    #[tokio::test]
    async fn a_failed_clone_leaves_no_partial_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);

        let id = svc
            .start_clone(CloneRequest {
                source: url_source(&dir.path().join("does-not-exist")),
                dest_name: None,
                workspace: None,
            })
            .await
            .unwrap()
            .id;

        let job = await_clone(&svc, id).await;
        let CloneStatus::Failed { message } = &job.status else {
            panic!("{:?}", job.status)
        };
        // git's own message, which is already phrased for a human.
        assert!(message.contains("does not exist"), "unhelpful: {message}");
        assert!(
            !job.dest.exists(),
            "a failed clone left {} behind",
            job.dest.display()
        );
        assert!(svc.list_projects().await.is_empty());
    }

    /// The cleanup is *shared*, which is the whole reason clone-then-register is
    /// one `?`-scoped block: a step that fails **after** the checkout exists has to
    /// remove it too, or a retry hits `DestinationExists` on a directory nothing
    /// knows about.
    ///
    /// Registration is made to fail by putting a *directory* where `state.json`
    /// belongs, so every persist errors — the one failure that can be injected
    /// after a clone has genuinely succeeded (`GitBackend::discover` and
    /// `detect_main_branch` both cope with whatever a real clone produces,
    /// including an empty repo).
    #[tokio::test]
    async fn a_failure_after_the_clone_removes_the_checkout_too() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("state.json")).unwrap();
        let svc = service(&dir);
        let remote = seed_bare_repo(&dir);

        let id = svc
            .start_clone(CloneRequest {
                source: url_source(&remote),
                dest_name: None,
                workspace: None,
            })
            .await
            .unwrap()
            .id;

        let job = await_clone(&svc, id).await;
        assert!(
            matches!(job.status, CloneStatus::Failed { .. }),
            "{:?}",
            job.status
        );
        assert!(
            !job.dest.exists(),
            "a checkout survived a failure after the clone: {}",
            job.dest.display()
        );
    }

    /// Validation happens before a job exists, so a doomed request is an error
    /// the caller sees immediately rather than a job it has to poll.
    #[tokio::test]
    async fn an_invalid_source_is_rejected_before_a_job_is_created() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);

        for source in [
            CloneSource::Url {
                url: "ftp://example.com/o/r".to_string(),
            },
            // A leading `-` would be read by git as an option, not a source.
            CloneSource::Url {
                url: "--upload-pack=evil".to_string(),
            },
            CloneSource::Github {
                full_name: "not-a-slug".to_string(),
            },
        ] {
            assert!(
                svc.start_clone(CloneRequest {
                    source: source.clone(),
                    dest_name: None,
                    workspace: None,
                })
                .await
                .is_err(),
                "accepted {source:?}"
            );
        }
        // Nothing landed in the projects dir, and no job is pollable.
        let projects_dir = svc.read_config().projects_dir().unwrap();
        assert!(!projects_dir.join("r").exists());
    }

    /// A caller-supplied `dest_name` gets the same directory-name rules as a
    /// derived one — otherwise `..` would escape the projects dir entirely.
    #[tokio::test]
    async fn an_unsafe_destination_name_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let remote = seed_bare_repo(&dir);
        let projects_dir = svc.read_config().projects_dir().unwrap();

        for name in ["..", "../escaped", "sub/dir", "", "-flag"] {
            assert!(
                svc.start_clone(CloneRequest {
                    source: url_source(&remote),
                    dest_name: Some(name.to_string()),
                    workspace: None,
                })
                .await
                .is_err(),
                "accepted dest_name {name:?}"
            );
        }
        // The one that would have escaped: the parent of the projects dir is
        // untouched.
        assert!(!projects_dir.parent().unwrap().join("escaped").exists());
    }

    /// The security property carried through this whole feature: a pasted
    /// `https://user:token@…` must not reach an error string, a log line or a job
    /// label. `start_clone` builds one user-facing string from the source (the
    /// rejection message), and it must be redacted.
    #[tokio::test]
    async fn a_credentialed_source_never_reaches_an_error_or_a_job() {
        const SECRET: &str = "ghp_s3cr3tt0ken";
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);

        // A URL pasted into the slug field: always malformed, and the one path
        // that echoes the caller's raw input back.
        let err = svc
            .start_clone(CloneRequest {
                source: CloneSource::Github {
                    full_name: format!("https://sizeak:{SECRET}@github.com/o/r"),
                },
                dest_name: None,
                workspace: None,
            })
            .await
            .unwrap_err();
        assert!(!err.to_string().contains(SECRET), "token leaked: {err}");
        assert!(
            !format!("{err:?}").contains(SECRET),
            "token leaked: {err:?}"
        );

        // A credentialed URL that *validates*: the job it creates is polled,
        // rendered and logged, so its label must be redacted too. Network-free —
        // git resolves a `file://` source on disk, and this one is not there, so
        // the clone fails locally and its message is checked as well.
        let missing = dir.path().join("missing.git");
        let id = svc
            .start_clone(CloneRequest {
                source: CloneSource::Url {
                    url: format!("file://sizeak:{SECRET}@{}", missing.display()),
                },
                dest_name: None,
                workspace: None,
            })
            .await
            .unwrap()
            .id;
        let job = svc.clone_job(id).await.unwrap();
        assert_eq!(
            job.source_label,
            format!("file://***@{}", missing.display())
        );
        assert!(
            !format!("{job:?}").contains(SECRET),
            "token leaked: {job:?}"
        );

        // And the failure message git hands back, which quotes the source.
        let job = await_clone(&svc, id).await;
        assert!(
            matches!(job.status, CloneStatus::Failed { .. }),
            "{:?}",
            job.status
        );
        assert!(
            !format!("{job:?}").contains(SECRET),
            "token leaked: {job:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_clone_job_id_is_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        assert!(svc.clone_job(CloneJobId::new()).await.is_none());
    }

    /// A directory scan registers what it finds into the caller's workspace,
    /// the same way a single add does — so a scan run from the TUI and one run
    /// from the Flutter app (through `POST /projects/scan`) land alike. A
    /// refused name is refused before anything is registered.
    #[tokio::test]
    async fn scan_directory_tags_new_projects_with_the_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let root = dir.path().join("repos");
        let repo = root.join("one");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "t@t.t"]);
        run_git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("README"), "v1\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "initial"]);

        let err = svc
            .scan_directory(&root, Some("last".into()))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Session(SessionError::InvalidName { .. })
        ));
        assert!(svc.list_projects().await.is_empty(), "nothing registered");

        let result = svc
            .scan_directory(&root, Some("Work".into()))
            .await
            .unwrap();
        assert_eq!(result.added, 1);
        let projects = svc.list_projects().await;
        assert_eq!(projects[0].workspace.as_deref(), Some("Work"));
        assert_eq!(def_names(&svc), ["Work"], "the workspace is defined");
    }

    /// A clone's `workspace` is checked before anything is cloned: a refused
    /// name would otherwise only surface after the whole checkout, which
    /// `clone_then_register` then deletes.
    #[tokio::test]
    async fn start_clone_refuses_a_bad_workspace_before_cloning() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let remote = seed_bare_repo(&dir);
        for bad in ["main", &"x".repeat(41)] {
            let err = svc
                .start_clone(CloneRequest {
                    source: url_source(&remote),
                    dest_name: None,
                    workspace: Some(bad.to_string()),
                })
                .await
                .unwrap_err();
            assert!(
                matches!(err, crate::Error::Session(SessionError::InvalidName { .. })),
                "{bad}: {err:?}"
            );
        }
        assert!(
            !svc.read_config().projects_dir().unwrap().exists(),
            "nothing was cloned"
        );
    }

    /// The projects directory is created on demand: `Config::projects_dir` only
    /// *resolves* a path (matching `worktrees_dir`), so a first clone on a fresh
    /// machine has nowhere to land unless `start_clone` creates it.
    #[tokio::test]
    async fn start_clone_creates_the_projects_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = service(&dir);
        let remote = seed_bare_repo(&dir);
        let projects_dir = svc.read_config().projects_dir().unwrap();
        assert!(
            !projects_dir.exists(),
            "fixture pre-created the projects dir"
        );

        let id = svc
            .start_clone(CloneRequest {
                source: url_source(&remote),
                dest_name: None,
                workspace: None,
            })
            .await
            .unwrap()
            .id;

        assert!(projects_dir.exists());
        assert!(matches!(
            await_clone(&svc, id).await.status,
            CloneStatus::Succeeded { .. }
        ));
    }
}
