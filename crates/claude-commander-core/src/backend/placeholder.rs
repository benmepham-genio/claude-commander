//! A backend that never connects.
//!
//! [`PlaceholderBackend`] occupies a [`BackendHandle`](super::BackendHandle)
//! slot for a remote server whose backend couldn't be *constructed* at all —
//! e.g. the [`RemoteBackendFactory`](super::RemoteBackendFactory) rejected a
//! malformed URL, or building the HTTP client failed. Rather than crash startup
//! or silently drop the server, the TUI keeps it in the tree as a permanently
//! [`Degraded`](super::ConnectionState::Degraded) server header showing the
//! construction error, and every operation against it returns
//! [`Unavailable`](BackendError::Unavailable) carrying that same reason.
//!
//! Its change-feed never fires (the sender is held here, alive but idle), so the
//! per-backend change-feed task parks on `changed().await` forever without
//! polling.

use async_trait::async_trait;
use tokio::sync::watch;
use uuid::Uuid;

use crate::api::{
    AgentStatesSnapshot, BranchInfo, CreateOptions, CreateSessionOpts, DiffSide, NewComment,
    OperationStatus, PreviewData, PreviewTarget, ProgramInfo, ReviewSnapshot, SessionDetail,
    SetSessionBaseOutcome, Snapshot,
};
use crate::comment::{ApplyOutcome, Comment};
use crate::session::{ProjectId, ScanResult, SessionId};
use claude_commander_protocol::api::ReviewedFileMark;
use claude_commander_protocol::github::{CloneJob, CloneJobId, CloneRequest, GithubRepo};
use claude_commander_protocol::workspace::SetWorkspacesRequest;

use super::{
    AttachConnection, AttachKind, BResult, BackendCapabilities, BackendChangeFeed,
    BackendDescriptor, BackendError, BackendKind, CommanderBackend,
};

/// A backend that failed to construct. See the module docs.
pub struct PlaceholderBackend {
    descriptor: BackendDescriptor,
    reason: String,
    /// Held so [`Self::change_feed`]'s receiver stays open (its `changed()`
    /// future never resolves) rather than the task seeing an immediate close.
    _gen_tx: watch::Sender<u64>,
    gen_rx: watch::Receiver<u64>,
}

impl PlaceholderBackend {
    /// A placeholder for the remote server named `name` that failed to build,
    /// with `reason` surfaced through its header and every operation.
    pub fn new(name: impl Into<String>, reason: impl Into<String>) -> Self {
        let (gen_tx, gen_rx) = watch::channel(0u64);
        Self {
            descriptor: BackendDescriptor {
                name: name.into(),
                kind: BackendKind::Remote,
            },
            reason: reason.into(),
            _gen_tx: gen_tx,
            gen_rx,
        }
    }

    fn unavailable<T>(&self) -> BResult<T> {
        Err(BackendError::Unavailable {
            reason: self.reason.clone(),
        })
    }
}

#[async_trait]
impl CommanderBackend for PlaceholderBackend {
    fn descriptor(&self) -> BackendDescriptor {
        self.descriptor.clone()
    }

    fn capabilities(&self) -> BackendCapabilities {
        // A never-connected backend has no operator-local affordances.
        BackendCapabilities {
            open_editor: false,
            commander_session: false,
            shell_toggle: false,
            client_side_image_paste: false,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn change_feed(&self) -> BackendChangeFeed {
        BackendChangeFeed::new(self.gen_rx.clone())
    }

    async fn snapshot(&self) -> BResult<Snapshot> {
        self.unavailable()
    }

    async fn agent_states(&self, _fresh: bool) -> BResult<AgentStatesSnapshot> {
        self.unavailable()
    }

    async fn session_detail(
        &self,
        _query: &str,
        _lines: Option<usize>,
    ) -> BResult<Option<SessionDetail>> {
        self.unavailable()
    }

    async fn preview(&self, _target: PreviewTarget) -> BResult<PreviewData> {
        self.unavailable()
    }

    async fn branch_diff(&self, _id: SessionId) -> BResult<String> {
        self.unavailable()
    }

    async fn list_branches(&self, _project: ProjectId, _fetch: bool) -> BResult<Vec<BranchInfo>> {
        self.unavailable()
    }

    async fn create_options(&self) -> BResult<CreateOptions> {
        self.unavailable()
    }

    async fn set_programs(&self, _programs: Vec<ProgramInfo>) -> BResult<()> {
        self.unavailable()
    }

    async fn pending_comment_sessions(&self) -> BResult<Vec<SessionId>> {
        self.unavailable()
    }

    async fn create_session(&self, _opts: CreateSessionOpts) -> BResult<SessionId> {
        self.unavailable()
    }

    async fn kill_session(&self, _id: SessionId) -> BResult<()> {
        self.unavailable()
    }

    async fn restart_session(&self, _id: SessionId) -> BResult<()> {
        self.unavailable()
    }

    async fn restart_session_fresh(&self, _id: SessionId) -> BResult<()> {
        self.unavailable()
    }

    async fn delete_session(&self, _id: SessionId) -> BResult<()> {
        self.unavailable()
    }

    async fn rename_session(&self, _id: SessionId, _title: String) -> BResult<()> {
        self.unavailable()
    }

    async fn change_program(&self, _id: SessionId, _program: String) -> BResult<()> {
        self.unavailable()
    }

    async fn set_section(&self, _id: SessionId, _section: Option<String>) -> BResult<()> {
        self.unavailable()
    }

    async fn set_session_base(
        &self,
        _id: SessionId,
        _parent: Option<SessionId>,
    ) -> BResult<SetSessionBaseOutcome> {
        self.unavailable()
    }

    async fn mark_read(&self, _id: SessionId) -> BResult<()> {
        self.unavailable()
    }

    async fn toggle_keep_alive(&self, _id: SessionId) -> BResult<bool> {
        self.unavailable()
    }

    async fn mark_unread(&self, _ids: Vec<SessionId>) -> BResult<()> {
        self.unavailable()
    }

    async fn request_pr_refresh(&self) -> BResult<()> {
        self.unavailable()
    }

    async fn add_project(
        &self,
        _path: std::path::PathBuf,
        _workspace: Option<String>,
    ) -> BResult<ProjectId> {
        self.unavailable()
    }

    async fn ensure_project(
        &self,
        _path: std::path::PathBuf,
        _workspace: Option<String>,
    ) -> BResult<ProjectId> {
        self.unavailable()
    }

    async fn set_workspaces(&self, _req: SetWorkspacesRequest) -> BResult<()> {
        self.unavailable()
    }

    async fn rename_workspace(&self, _from: String, _to: String) -> BResult<()> {
        self.unavailable()
    }

    async fn delete_workspace(&self, _name: String) -> BResult<()> {
        self.unavailable()
    }

    async fn set_project_workspace(
        &self,
        _id: ProjectId,
        _workspace: Option<String>,
    ) -> BResult<()> {
        self.unavailable()
    }

    async fn remove_project(&self, _id: ProjectId) -> BResult<()> {
        self.unavailable()
    }

    async fn scan_directory(
        &self,
        _dir: std::path::PathBuf,
        _workspace: Option<String>,
    ) -> BResult<ScanResult> {
        self.unavailable()
    }

    async fn list_github_repos(&self) -> BResult<Vec<GithubRepo>> {
        self.unavailable()
    }

    async fn start_clone(&self, _req: CloneRequest) -> BResult<CloneJob> {
        self.unavailable()
    }

    async fn clone_job(&self, _id: CloneJobId) -> BResult<Option<CloneJob>> {
        // `Unavailable`, not `Ok(None)`: this backend never connected, so it
        // cannot know the job is absent — reporting absence would tell a poll
        // loop to stop when the truth is that nothing was ever asked.
        self.unavailable()
    }

    async fn cascade_merge(&self, _id: SessionId) -> BResult<OperationStatus> {
        self.unavailable()
    }

    async fn cascade_resume(&self) -> BResult<OperationStatus> {
        self.unavailable()
    }

    async fn cascade_abandon(&self) -> BResult<()> {
        self.unavailable()
    }

    async fn push_stack(&self, _id: SessionId) -> BResult<OperationStatus> {
        self.unavailable()
    }

    async fn list_comments(&self, _id: SessionId) -> BResult<Vec<Comment>> {
        self.unavailable()
    }

    async fn open_review(&self, _id: SessionId) -> BResult<ReviewSnapshot> {
        self.unavailable()
    }

    async fn refresh_review_if_changed(
        &self,
        _id: SessionId,
        _prev_hash: u64,
    ) -> BResult<Option<ReviewSnapshot>> {
        self.unavailable()
    }

    async fn create_comment(&self, _id: SessionId, _draft: NewComment) -> BResult<Uuid> {
        self.unavailable()
    }

    async fn delete_comment(&self, _id: SessionId, _comment_id: Uuid) -> BResult<()> {
        self.unavailable()
    }

    async fn apply_comments(&self, _id: SessionId) -> BResult<ApplyOutcome> {
        self.unavailable()
    }

    async fn set_file_reviewed(
        &self,
        _id: SessionId,
        _display_path: String,
        _reviewed: bool,
    ) -> BResult<bool> {
        self.unavailable()
    }

    async fn reviewed_marks(&self, _id: SessionId) -> BResult<Vec<ReviewedFileMark>> {
        self.unavailable()
    }

    async fn fetch_diff_blob(
        &self,
        _id: SessionId,
        _side: DiffSide,
        _path: String,
    ) -> BResult<Vec<u8>> {
        self.unavailable()
    }

    async fn attach(
        &self,
        _id: SessionId,
        _cols: u16,
        _rows: u16,
        _kind: AttachKind,
    ) -> BResult<Box<dyn AttachConnection>> {
        self.unavailable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn placeholder_reports_reason_and_is_remote() {
        let b = PlaceholderBackend::new("buildbox", "invalid url");
        assert_eq!(b.descriptor().name, "buildbox");
        assert_eq!(b.descriptor().kind, BackendKind::Remote);
        let err = b.snapshot().await.unwrap_err();
        match err {
            BackendError::Unavailable { reason } => assert_eq!(reason, "invalid url"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    /// The clone surface is refused like everything else, carrying the same
    /// construction reason — a repo picker opened against a never-connected
    /// server must say *why*, not show an empty list.
    #[tokio::test]
    async fn placeholder_refuses_the_clone_surface() {
        use claude_commander_protocol::github::{CloneJobId, CloneRequest, CloneSource};

        let b = PlaceholderBackend::new("buildbox", "invalid url");
        let expect_unavailable = |err: BackendError| match err {
            BackendError::Unavailable { reason } => assert_eq!(reason, "invalid url"),
            other => panic!("expected Unavailable, got {other:?}"),
        };
        expect_unavailable(b.list_github_repos().await.unwrap_err());
        expect_unavailable(
            b.start_clone(CloneRequest {
                source: CloneSource::Github {
                    full_name: "octo/widget".to_string(),
                },
                dest_name: None,
                workspace: None,
            })
            .await
            .unwrap_err(),
        );
        expect_unavailable(b.clone_job(CloneJobId::new()).await.unwrap_err());
    }

    #[test]
    fn placeholder_has_no_capabilities() {
        let b = PlaceholderBackend::new("x", "y");
        let caps = b.capabilities();
        assert!(!caps.open_editor);
        assert!(!caps.commander_session);
        assert!(!caps.shell_toggle);
    }
}
