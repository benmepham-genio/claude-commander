//! In-process backend: drives an owned [`CommanderService`] directly.
//!
//! This is the backend the TUI and CLI use today. Every method delegates to the
//! matching service method and maps its [`crate::error::Error`] onto a
//! [`BackendError`] (the `?` operator does the classifying conversion). The
//! `!Send` service futures — the ones that build a `gix::Repository` and hold it
//! across an `.await` (session/project mutations, cascade, branch listing) — are
//! driven through [`run_local`] so a `LocalBackend` future stays `Send` and can
//! be `.await`ed on a multi-thread runtime or a `tokio::spawn`ed task.

use std::path::PathBuf;

use async_trait::async_trait;
use uuid::Uuid;

use crate::api::{
    AgentStatesSnapshot, BranchInfo, CommanderService, CreateOptions, CreateSessionOpts, DiffSide,
    NewComment, OperationStatus, PreviewData, PreviewTarget, ProgramInfo, ReviewSnapshot,
    SessionDetail, SetSessionBaseOutcome, Snapshot,
};
use crate::comment::ApplyOutcome;
use crate::session::{ProjectId, ScanResult, SessionId};
use crate::tmux::HeadlessAttach;
use claude_commander_protocol::api::ReviewedFileMark;
use claude_commander_protocol::github::{CloneJob, CloneJobId, CloneRequest, GithubRepo};
use claude_commander_protocol::workspace::SetWorkspacesRequest;

use super::error::BResult;
use super::run_local::run_local;
use super::{
    AttachConnection, AttachKind, AttachStreams, BackendCapabilities, BackendChangeFeed,
    BackendDescriptor, BackendKind, CommanderBackend,
};

/// A [`CommanderBackend`] backed by an in-process [`CommanderService`]. Cheap to
/// clone (the service is a bundle of `Arc`s).
#[derive(Clone)]
pub struct LocalBackend {
    service: CommanderService,
}

impl LocalBackend {
    pub fn new(service: CommanderService) -> Self {
        Self { service }
    }

    /// The wrapped service, for callers that still need direct access during the
    /// Phase C migration.
    pub fn service(&self) -> &CommanderService {
        &self.service
    }

    // -- Local-only affordances (reached via `CommanderBackend::as_any`) --
    //
    // These have no place on the trait: they attach by tmux *name* (the
    // commander and project shells have no `SessionId`) or drive the operator's
    // own tmux server, which a remote backend can't do. The TUI gates them
    // behind `capabilities()` and downcasts to `LocalBackend` to call them.

    /// Attach to a tmux session by name (the commander session or a project
    /// shell, which have no `SessionId`). Unlike [`CommanderBackend::attach`]
    /// this does not stamp last-attached time or revive the session — the
    /// caller ensures it exists first.
    pub async fn attach_by_tmux_name(
        &self,
        tmux_name: &str,
        cols: u16,
        rows: u16,
    ) -> BResult<Box<dyn AttachConnection>> {
        let tmux_tmpdir = self.service.read_config().tmux_tmpdir;
        let bridge = HeadlessAttach::spawn(tmux_name, cols, rows, tmux_tmpdir.as_deref())?;
        Ok(Box::new(LocalAttachConnection { bridge }))
    }

    /// Resolve the Ctrl+\ shell-toggle partner for a tmux session reached via
    /// the in-session switcher (which lands on an arbitrary session by name, so
    /// there's no `SessionId`/[`AttachKind`] to flip). A Claude session toggles
    /// to its `-sh` shell (created on demand); a shell toggles back to its
    /// Claude session; a project shell toggles to itself.
    pub async fn resolve_shell_toggle_pair(&self, current_tmux_name: &str) -> BResult<String> {
        if let Some(claude_name) = current_tmux_name.strip_suffix("-sh") {
            let claude_name = claude_name.to_string();
            if self
                .service
                .session_manager()
                .tmux
                .session_exists(&claude_name)
                .await?
            {
                return Ok(claude_name);
            }
            return Err(crate::error::Error::Session(
                crate::error::SessionError::TmuxSessionNotFound(claude_name),
            )
            .into());
        }

        let session_id = {
            let state = self.service.store().read().await;
            state
                .sessions
                .values()
                .find(|s| s.tmux_session_name == current_tmux_name)
                .map(|s| s.id)
        };
        if let Some(session_id) = session_id {
            return Ok(self
                .service
                .session_manager()
                .ensure_shell_session(&session_id)
                .await?);
        }

        let project_id = {
            let state = self.service.store().read().await;
            state
                .projects
                .values()
                .find(|p| p.shell_tmux_session_name.as_deref() == Some(current_tmux_name))
                .map(|p| p.id)
        };
        if let Some(project_id) = project_id {
            return Ok(self
                .service
                .session_manager()
                .ensure_project_shell_session(&project_id)
                .await?);
        }

        Err(
            crate::error::Error::Session(crate::error::SessionError::TmuxSessionNotFound(format!(
                "No session found for tmux name: {current_tmux_name}"
            )))
            .into(),
        )
    }

    /// Ensure a project's shell tmux session exists and return its name (the
    /// project-shell equivalent of [`CommanderBackend::attach`]'s agent-pane
    /// resolution). Recreates the shell if its pane died.
    pub async fn project_shell_name(&self, project_id: ProjectId) -> BResult<String> {
        Ok(self
            .service
            .session_manager()
            .get_project_shell_session_name(&project_id)
            .await?)
    }

    /// Create or revive the persistent commander tmux session and return its
    /// name. Delegates to [`crate::commander::ensure_session`] with the local
    /// tmux executor.
    ///
    /// Returns the core [`Result`](crate::error::Result) (not [`BResult`]) so
    /// the caller can match [`SessionError::CommanderDisabled`](crate::error::SessionError::CommanderDisabled)
    /// for its specific "enable it in settings" message.
    pub async fn ensure_commander(
        &self,
        config: &crate::config::Config,
        cli_reference: &str,
    ) -> crate::error::Result<String> {
        crate::commander::ensure_session(
            config,
            &self.service.session_manager().tmux,
            cli_reference,
        )
        .await
    }
}

#[async_trait]
impl CommanderBackend for LocalBackend {
    fn descriptor(&self) -> BackendDescriptor {
        BackendDescriptor {
            name: "local".to_string(),
            kind: BackendKind::Local,
        }
    }

    fn capabilities(&self) -> BackendCapabilities {
        // The local backend runs on the operator's own machine, so every
        // affordance is available.
        BackendCapabilities::LOCAL
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn change_feed(&self) -> BackendChangeFeed {
        BackendChangeFeed::new(self.service.store().subscribe())
    }

    async fn startup_reconcile(&self) -> BResult<()> {
        // `sync_worktrees` opens a gix repo across an `.await` → `!Send`; route
        // the whole reconcile through `run_local` so the future stays `Send`.
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.startup_reconcile().await }).await?)
    }

    async fn reconcile_sections(&self) -> BResult<()> {
        Ok(self.service.reconcile_all_section_assignments().await?)
    }

    async fn reconcile_one_section(&self, id: SessionId) -> BResult<()> {
        Ok(self.service.reconcile_one_section_assignment(id).await?)
    }

    fn record_feature(&self, feature: &'static str) {
        self.service.telemetry().feature(feature);
    }

    async fn flush_telemetry(&self) {
        self.service.telemetry().flush().await;
    }

    // -- Queries (all `Send`: store reads, tmux, git CLI) --

    async fn snapshot(&self) -> BResult<Snapshot> {
        Ok(self.service.snapshot().await?)
    }

    async fn agent_states(&self, fresh: bool) -> BResult<AgentStatesSnapshot> {
        Ok(self.service.agent_states(fresh).await)
    }

    async fn session_detail(
        &self,
        query: &str,
        lines: Option<usize>,
    ) -> BResult<Option<SessionDetail>> {
        Ok(self.service.get_session_detail(query, lines).await?)
    }

    async fn preview(&self, target: PreviewTarget) -> BResult<PreviewData> {
        Ok(self.service.preview(target).await?)
    }

    async fn branch_diff(&self, id: SessionId) -> BResult<String> {
        Ok(self.service.branch_diff(&id).await?)
    }

    async fn list_branches(&self, project: ProjectId, fetch: bool) -> BResult<Vec<BranchInfo>> {
        // `list_branches` opens a gix repo → `!Send`; route through `run_local`.
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.list_branches(&project, fetch).await }).await?)
    }

    async fn create_options(&self) -> BResult<CreateOptions> {
        Ok(self.service.create_options())
    }

    async fn set_programs(&self, programs: Vec<ProgramInfo>) -> BResult<()> {
        Ok(self
            .service
            .set_programs(programs.into_iter().map(Into::into).collect())?)
    }

    async fn pending_comment_sessions(&self) -> BResult<Vec<SessionId>> {
        let mut ids: Vec<SessionId> = self
            .service
            .sessions_with_pending_comments()
            .await?
            .into_iter()
            .collect();
        ids.sort();
        Ok(ids)
    }

    // -- Session mutations (gix-backed → `run_local`) --

    async fn create_session(&self, opts: CreateSessionOpts) -> BResult<SessionId> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.create_session(opts).await }).await?)
    }

    async fn kill_session(&self, id: SessionId) -> BResult<()> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.kill_session(&id).await }).await?)
    }

    async fn restart_session(&self, id: SessionId) -> BResult<()> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.restart_session(&id).await }).await?)
    }

    async fn restart_session_fresh(&self, id: SessionId) -> BResult<()> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.restart_session_fresh(&id).await }).await?)
    }

    async fn delete_session(&self, id: SessionId) -> BResult<()> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.delete_session(&id).await }).await?)
    }

    async fn rename_session(&self, id: SessionId, title: String) -> BResult<()> {
        // Store-only mutation → `Send`, delegate directly.
        Ok(self.service.rename_session(&id, title).await?)
    }

    async fn change_program(&self, id: SessionId, program: String) -> BResult<()> {
        // Relaunches the pane (tmux) → not `Send`; run on the local pool.
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.change_program(&id, program).await }).await?)
    }

    async fn set_section(&self, id: SessionId, section: Option<String>) -> BResult<()> {
        Ok(self.service.set_section(&id, section).await?)
    }

    async fn set_session_base(
        &self,
        id: SessionId,
        parent: Option<SessionId>,
    ) -> BResult<SetSessionBaseOutcome> {
        // Store mutation plus a `gh` subprocess — both `Send`, no tmux or gix.
        // Delegate directly rather than going through the local pool.
        Ok(self.service.set_session_base(&id, parent).await?)
    }

    async fn mark_read(&self, id: SessionId) -> BResult<()> {
        Ok(self.service.mark_read(&id).await?)
    }

    async fn toggle_keep_alive(&self, id: SessionId) -> BResult<bool> {
        Ok(self.service.toggle_keep_alive(&id).await?)
    }

    async fn mark_unread(&self, ids: Vec<SessionId>) -> BResult<()> {
        Ok(self.service.mark_unread(ids).await?)
    }

    async fn apply_pr_results(
        &self,
        results: Vec<(SessionId, crate::git::PrCheckResult)>,
    ) -> BResult<()> {
        Ok(self.service.apply_pr_results(results).await?)
    }

    async fn request_pr_refresh(&self) -> BResult<()> {
        Ok(self.service.request_pr_refresh()?)
    }

    // -- Projects (gix-backed → `run_local`) --

    async fn add_project(&self, path: PathBuf, workspace: Option<String>) -> BResult<ProjectId> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.add_project(path, workspace).await }).await?)
    }

    async fn ensure_project(&self, path: PathBuf, workspace: Option<String>) -> BResult<ProjectId> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.ensure_project(path, workspace).await }).await?)
    }

    async fn set_workspaces(&self, req: SetWorkspacesRequest) -> BResult<()> {
        Ok(self.service.set_workspace_defs(req)?)
    }

    async fn rename_workspace(&self, from: String, to: String) -> BResult<()> {
        self.service.rename_workspace(&from, &to).await?;
        Ok(())
    }

    async fn delete_workspace(&self, name: String) -> BResult<()> {
        self.service.delete_workspace(&name).await?;
        Ok(())
    }

    async fn set_project_workspace(&self, id: ProjectId, workspace: Option<String>) -> BResult<()> {
        Ok(self.service.set_project_workspace(&id, workspace).await?)
    }

    async fn remove_project(&self, id: ProjectId) -> BResult<()> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.remove_project(&id).await }).await?)
    }

    async fn scan_directory(&self, dir: PathBuf, workspace: Option<String>) -> BResult<ScanResult> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.scan_directory(&dir, workspace).await }).await?)
    }

    // -- Repository clone --
    //
    // No `run_local` here, unlike the project methods above: the `gix` work in
    // this flow (registering the finished checkout as a project) lives inside the
    // background job future, which `CloneJobs::spawn` already requires to be
    // `Send + 'static` (`git/clone_jobs.rs:147-150`) and which routes itself
    // through `run_local` (`clone_then_register` in `api.rs`). What these three
    // methods await is a subprocess, a `create_dir_all` and an in-memory map read
    // — all `Send`, which is what lets these be plain delegations.

    async fn list_github_repos(&self) -> BResult<Vec<GithubRepo>> {
        Ok(self.service.list_github_repos().await?)
    }

    async fn start_clone(&self, req: CloneRequest) -> BResult<CloneJob> {
        Ok(self.service.start_clone(req).await?)
    }

    async fn clone_job(&self, id: CloneJobId) -> BResult<Option<CloneJob>> {
        // Infallible locally — the registry is in-process, so absence is the only
        // "failure" and it is `None`. The `BResult` is for a remote backend, whose
        // poll can fail on the transport before it can report absence.
        Ok(self.service.clone_job(id).await)
    }

    // -- Cascade / push-stack (gix-backed → `run_local`) --

    async fn cascade_merge(&self, id: SessionId) -> BResult<OperationStatus> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.cascade_merge(&id).await }).await?)
    }

    async fn cascade_resume(&self) -> BResult<OperationStatus> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.cascade_resume().await }).await?)
    }

    async fn cascade_abandon(&self) -> BResult<()> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.cascade_abandon().await }).await?)
    }

    async fn push_stack(&self, id: SessionId) -> BResult<OperationStatus> {
        let svc = self.service.clone();
        Ok(run_local(move || async move { svc.push_stack(&id).await }).await?)
    }

    // -- Review / comments (git CLI + stores → `Send`) --

    async fn list_comments(&self, id: SessionId) -> BResult<Vec<crate::comment::Comment>> {
        Ok(self.service.list_comments(&id).await?)
    }

    async fn open_review(&self, id: SessionId) -> BResult<ReviewSnapshot> {
        Ok(self.service.open_review(&id).await?)
    }

    async fn refresh_review_if_changed(
        &self,
        id: SessionId,
        prev_hash: u64,
    ) -> BResult<Option<ReviewSnapshot>> {
        Ok(self
            .service
            .refresh_review_if_changed(&id, prev_hash)
            .await?)
    }

    async fn create_comment(&self, id: SessionId, draft: NewComment) -> BResult<Uuid> {
        Ok(self.service.create_comment(&id, draft).await?)
    }

    async fn delete_comment(&self, id: SessionId, comment_id: Uuid) -> BResult<()> {
        Ok(self.service.delete_comment(&id, comment_id).await?)
    }

    async fn apply_comments(&self, id: SessionId) -> BResult<ApplyOutcome> {
        Ok(self.service.apply_comments(&id).await?)
    }

    async fn set_file_reviewed(
        &self,
        id: SessionId,
        display_path: String,
        reviewed: bool,
    ) -> BResult<bool> {
        Ok(self
            .service
            .set_file_reviewed_by_path(&id, &display_path, reviewed)
            .await?)
    }

    async fn reviewed_marks(&self, id: SessionId) -> BResult<Vec<ReviewedFileMark>> {
        Ok(self.service.reviewed_marks(&id).await?)
    }

    async fn fetch_diff_blob(
        &self,
        id: SessionId,
        side: DiffSide,
        path: String,
    ) -> BResult<Vec<u8>> {
        Ok(self.service.fetch_diff_blob(&id, side, &path).await?)
    }

    // -- Attach --

    async fn attach(
        &self,
        id: SessionId,
        cols: u16,
        rows: u16,
        kind: AttachKind,
    ) -> BResult<Box<dyn AttachConnection>> {
        // Resolve the tmux session name for the requested pane. The agent pane
        // is the session's primary tmux session — `ensure_attachable` validates
        // it can be attached and revives it (resume + status bar) if the tmux
        // session died. The shell pane is created on demand.
        let tmux_name = match kind {
            AttachKind::Agent => {
                self.service
                    .session_manager()
                    .ensure_attachable(&id)
                    .await?
            }
            AttachKind::Shell => {
                self.service
                    .session_manager()
                    .ensure_shell_session(&id)
                    .await?
            }
        };

        // Stamp last-attached time (MRU ordering) — moved here from the TUI so
        // every frontend records the attach identically.
        self.service.mark_attached(&id).await?;

        // Honour the socket-dir isolation knob so a hermetic test attaches to
        // the throwaway tmux server its session was created on, matching the
        // server's WebSocket attach handler.
        let tmux_tmpdir = self.service.read_config().tmux_tmpdir;
        let bridge = HeadlessAttach::spawn(&tmux_name, cols, rows, tmux_tmpdir.as_deref())?;
        Ok(Box::new(LocalAttachConnection { bridge }))
    }
}

/// A live local attach: a `tmux attach-session` running in a PTY via the
/// transport-agnostic [`HeadlessAttach`] bridge. Splits into the shared
/// [`AttachStreams`] the generalized attach loop drives (see
/// [`HeadlessAttach::into_streams`]).
pub struct LocalAttachConnection {
    bridge: HeadlessAttach,
}

impl AttachConnection for LocalAttachConnection {
    fn split(self: Box<Self>) -> AttachStreams {
        self.bridge.into_streams()
    }
}

#[cfg(test)]
mod tests {
    use super::super::error::BackendError;
    use super::*;
    use crate::config::storage::AppState as CoreState;
    use crate::config::{Config, ConfigStore, StateStore};
    use crate::session::{Project, WorktreeSession};
    use crate::telemetry::FrontendInfo;
    use claude_commander_protocol::github::{CloneSource, CloneStatus};
    use std::sync::Arc;

    /// Build a hermetic backend over `TempDir`-backed stores: telemetry off,
    /// tmux isolated onto a throwaway socket dir, projects dir pinned under
    /// `dir` (per the project's test-isolation rules).
    fn backend(dir: &tempfile::TempDir) -> LocalBackend {
        let mut config = Config::default();
        config.telemetry.enabled = false;
        let tmux_tmpdir = dir.path().join("tmux");
        std::fs::create_dir_all(&tmux_tmpdir).unwrap();
        config.tmux_tmpdir = Some(tmux_tmpdir);
        // `projects_dir` defaults to the user's REAL `~/Projects`, which the
        // repo-clone paths write into. Pin it under `dir`.
        config.projects_dir = Some(dir.path().join("projects"));
        let config_store = Arc::new(ConfigStore::with_path(
            config,
            dir.path().join("config.toml"),
        ));
        let store = Arc::new(StateStore::with_path(
            CoreState::default(),
            dir.path().join("state.json"),
        ));
        let service =
            CommanderService::new(config_store, store, FrontendInfo::new("test", "0.0.0"));
        LocalBackend::new(service)
    }

    /// Seed one project + one session; return their ids.
    async fn seed(be: &LocalBackend) -> (ProjectId, SessionId) {
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
        be.service()
            .store()
            .mutate(move |state| {
                state.add_project(project);
                state.add_session(session);
            })
            .await
            .unwrap();
        (pid, sid)
    }

    #[test]
    fn descriptor_and_capabilities_are_local() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let d = be.descriptor();
        assert_eq!(d.name, "local");
        assert_eq!(d.kind, BackendKind::Local);
        let c = be.capabilities();
        assert!(c.open_editor && c.commander_session && c.shell_toggle);
    }

    #[tokio::test]
    async fn change_feed_tracks_store_generation() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let mut feed = be.change_feed();
        let before = feed.generation();

        // A mutation through the backend bumps the feed the TUI subscribes to.
        seed(&be).await;

        assert!(feed.changed().await, "sender should still be alive");
        assert!(feed.generation() > before);
    }

    #[tokio::test]
    async fn snapshot_delegates() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let (pid, sid) = seed(&be).await;
        let snap = be.snapshot().await.unwrap();
        assert_eq!(snap.projects.len(), 1);
        assert_eq!(snap.projects[0].id, pid);
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].session_id, sid);
    }

    #[tokio::test]
    async fn agent_states_empty_without_active_sessions() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let snap = be.agent_states(false).await.unwrap();
        assert!(snap.states.is_empty());
        // The test config has the commander disabled, so the unprimed fallback
        // must report it as not running (it no longer hardcodes `true`).
        assert!(!snap.commander_running);
    }

    #[tokio::test]
    async fn create_options_delegates() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let opts = be.create_options().await.unwrap();
        assert!(!opts.default_program.is_empty());
    }

    /// Delegated directly, not via `run_local`: the operation is a store
    /// mutation plus a `gh` subprocess, both `Send` — no tmux and no gix. If
    /// this ever stops compiling, something non-`Send` crept into the path.
    #[tokio::test]
    async fn set_session_base_delegates() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let (pid, sid) = seed(&be).await;
        // The seeded session is already based on main (the load-time backfill
        // fills it in), so retarget onto a sibling to get a real change.
        let target = WorktreeSession::new(
            pid,
            "other",
            "branch-other",
            PathBuf::from("/tmp/wt2"),
            "claude",
        );
        let target_id = target.id;
        be.service()
            .store()
            .mutate(move |state| state.add_session(target))
            .await
            .unwrap();

        let outcome = be.set_session_base(sid, Some(target_id)).await.unwrap();
        assert_eq!(outcome.new_base_branch, "branch-other");
        let state = be.service().store().read().await;
        let s = state.get_session(&sid).unwrap();
        assert_eq!(s.base_branch.as_deref(), Some("branch-other"));
        assert_eq!(s.stack_parent_session_id, Some(target_id));
    }

    #[tokio::test]
    async fn rename_session_delegates() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let (_pid, sid) = seed(&be).await;
        be.rename_session(sid, "renamed".to_string()).await.unwrap();
        let state = be.service().store().read().await;
        assert_eq!(state.get_session(&sid).unwrap().title, "renamed");
    }

    #[tokio::test]
    async fn rename_missing_session_maps_to_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let err = be
            .rename_session(SessionId::new(), "x".to_string())
            .await
            .unwrap_err();
        // The core `NotFound` classifies to the transport-neutral `NotFound`.
        assert!(matches!(err, BackendError::NotFound), "got {err:?}");
    }

    #[tokio::test]
    async fn rename_empty_title_maps_to_invalid_request() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let (_pid, sid) = seed(&be).await;
        let err = be.rename_session(sid, "   ".to_string()).await.unwrap_err();
        assert!(
            matches!(err, BackendError::InvalidRequest(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn mark_read_and_mark_attached_via_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let (_pid, sid) = seed(&be).await;
        be.service()
            .store()
            .mutate(move |state| state.get_session_mut(&sid).unwrap().unread = true)
            .await
            .unwrap();
        be.mark_read(sid).await.unwrap();
        let state = be.service().store().read().await;
        let s = state.get_session(&sid).unwrap();
        assert!(!s.unread);
        // `attach` stamps last_attached_at; here we just prove the service hook
        // the backend calls works.
        assert!(s.last_attached_at.is_none());
    }

    #[tokio::test]
    async fn pending_comment_sessions_sorted_and_empty_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        assert!(be.pending_comment_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_comments_empty_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let (_pid, sid) = seed(&be).await;
        assert!(be.list_comments(sid).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn startup_reconcile_via_backend_drops_stale_creating() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let (_pid, sid) = seed(&be).await;
        be.service()
            .store()
            .mutate(move |state| {
                state
                    .get_session_mut(&sid)
                    .unwrap()
                    .set_status(crate::session::SessionStatus::Creating)
            })
            .await
            .unwrap();
        be.startup_reconcile().await.unwrap();
        let state = be.service().store().read().await;
        assert!(state.get_session(&sid).is_none());
    }

    #[tokio::test]
    async fn attach_unknown_agent_session_is_not_found() {
        // The agent-pane resolution reads the store only (no tmux), so this
        // covers the attach error path hermetically without a live tmux server.
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        // `Box<dyn AttachConnection>` isn't `Debug`, so match rather than
        // `unwrap_err`.
        match be.attach(SessionId::new(), 80, 24, AttachKind::Agent).await {
            Err(BackendError::NotFound) => {}
            Err(other) => panic!("expected NotFound, got {other:?}"),
            Ok(_) => panic!("expected an error attaching to an unknown session"),
        }
    }

    // -- Repository clone --
    //
    // `list_github_repos` is deliberately untested here: it shells out to `gh`,
    // which on a developer's authenticated machine would hit the GitHub API. The
    // parsing and the gh-availability probe are covered in `git::github`; what is
    // left in the backend is a one-line delegation.

    /// Start → poll → terminal status through the backend seam, network-free: an
    /// occupied destination is the one outcome `start_clone` reaches *without*
    /// running `git`, so it exercises the whole local clone surface offline.
    #[tokio::test]
    async fn clone_through_the_backend_reports_an_occupied_destination() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let projects = be.service().read_config().projects_dir().unwrap();
        // Occupy the destination the source's name derives to, so no clone runs.
        std::fs::create_dir_all(projects.join("widget")).unwrap();

        let job = be
            .start_clone(CloneRequest {
                source: CloneSource::Url {
                    url: "https://example.invalid/octo/widget.git".to_string(),
                },
                dest_name: None,
                workspace: None,
            })
            .await
            .unwrap();
        assert_eq!(job.dest, projects.join("widget"));

        // The id in that response is pollable through the same trait surface.
        let mut status = job.status.clone();
        for _ in 0..2_000 {
            let polled = be
                .clone_job(job.id)
                .await
                .unwrap()
                .expect("a job started a moment ago must still be readable");
            status = polled.status;
            if !matches!(status, CloneStatus::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            matches!(
                status,
                CloneStatus::DestinationExists {
                    is_git_repo: false,
                    ..
                }
            ),
            "expected DestinationExists for an occupied dir, got {status:?}"
        );
    }

    /// A refused source must classify as a **request** error, the same category a
    /// remote backend builds from the server's 400 — otherwise the same mistake
    /// reads as "the backend broke" locally and "you sent something unusable"
    /// remotely, and a frontend cannot render one message for both.
    #[tokio::test]
    async fn a_refused_clone_source_is_an_invalid_request() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        let err = be
            .start_clone(CloneRequest {
                // Leading `-`: git would read this as a flag, so the validators
                // refuse it before any subprocess starts.
                source: CloneSource::Url {
                    url: "--upload-pack=evil".to_string(),
                },
                dest_name: None,
                workspace: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, BackendError::InvalidRequest(_)),
            "expected InvalidRequest, got {err:?}"
        );
    }

    /// An id that was never issued (or has been pruned) is absence, not an error:
    /// a poll loop must be able to tell "gone" from "the backend is broken".
    #[tokio::test]
    async fn an_unknown_clone_job_is_absent_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let be = backend(&dir);
        assert_eq!(be.clone_job(CloneJobId::new()).await.unwrap(), None);
    }

    /// The backend is usable as `Arc<dyn CommanderBackend>` and its futures are
    /// `Send` (drivable from `tokio::spawn`).
    #[tokio::test]
    async fn usable_as_trait_object_across_spawn() {
        let dir = tempfile::TempDir::new().unwrap();
        let be: Arc<dyn CommanderBackend> = Arc::new(backend(&dir));
        let handle = tokio::spawn(async move { be.create_options().await });
        assert!(handle.await.unwrap().is_ok());
    }
}
