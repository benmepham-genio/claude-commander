//! The backend seam the TUI drives, in place of wiring `CommanderService` and
//! the stores together itself.
//!
//! A [`CommanderBackend`] is everything the session tree, preview, review view,
//! and attach flow need, expressed over **protocol DTOs** wherever a dedicated
//! wire type exists. A handful of core domain types that are themselves
//! `Serialize`/`Deserialize` still cross the surface where no separate DTO was
//! carved out — [`Comment`](crate::comment::Comment) and
//! [`ApplyOutcome`](crate::comment::ApplyOutcome) (review comments),
//! [`ScanResult`](crate::session::ScanResult) (directory scan), and
//! [`PrCheckResult`](crate::git::PrCheckResult) — so they serialize over the
//! wire unchanged. Two implementations exist:
//!
//! - [`LocalBackend`](local::LocalBackend): wraps an in-process
//!   [`CommanderService`](crate::api::CommanderService); this is what the TUI
//!   and CLI use today.
//! - (Phase F) a remote client that talks to `claude-commander-server` over
//!   HTTP + WebSocket, so the same TUI can drive a machine across the network.
//!
//! Because the trait is object-safe and `Send + Sync` with `Send` futures, the
//! TUI holds an `Arc<dyn CommanderBackend>` and can hand clones to background
//! tasks (`tokio::spawn`) — the poll loops, the review refresh, etc.
//!
//! The interactive attach loop (raw mode, keystroke interception, SIGWINCH) is
//! **not** rewritten here — Phase C lifts it out of `tmux/attach.rs` to run over
//! [`AttachConnection`], which this module defines so both a local PTY and a
//! remote WebSocket can back it.

pub mod error;
pub mod local;
// Test scaffolding, not part of the supported API. Reachable under the
// `test-support` feature as well as `cfg(test)` because the TUI crate's tests
// need it and a `cfg(test)` item is invisible across a crate boundary. The
// feature is off by default and enabled only by a dev-dependency, so the double
// never reaches a release build.
#[cfg(any(test, feature = "test-support"))]
pub mod mock;
pub mod placeholder;
pub mod run_local;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use uuid::Uuid;

use crate::api::{
    AgentStatesSnapshot, BranchInfo, CreateOptions, CreateSessionOpts, DiffSide, NewComment,
    OperationStatus, PreviewData, PreviewTarget, ProgramInfo, ReviewSnapshot, ServerStatus,
    SessionDetail, SetSessionBaseOutcome, Snapshot,
};
use crate::comment::ApplyOutcome;
use crate::session::{ProjectId, SessionId};
use claude_commander_protocol::api::ReviewedFileMark;
use claude_commander_protocol::github::{CloneJob, CloneJobId, CloneRequest, GithubRepo};
use claude_commander_protocol::workspace::SetWorkspacesRequest;

pub use error::{BResult, BackendError};
pub use local::LocalBackend;
pub use placeholder::PlaceholderBackend;
pub use run_local::{RunLocalError, run_local};

/// Builds a remote [`CommanderBackend`] from its [`RemoteServerConfig`], injected
/// into `claude_commander_tui::App` at construction so **core never depends on
/// the remote client crate** — the binary owns the dependency direction and
/// passes a closure that calls `claude_commander_remote::RemoteBackend::new`.
/// (Not an intra-doc link: the TUI is downstream of core, so core cannot name it
/// as a resolvable path.)
///
/// Returning `Err` means the backend couldn't be constructed at all (a malformed
/// URL, say); the TUI substitutes a permanently-degraded
/// [`PlaceholderBackend`] so the server still shows in the tree with its error,
/// rather than crashing or vanishing.
pub type RemoteBackendFactory = Arc<
    dyn Fn(&crate::config::RemoteServerConfig) -> BResult<Arc<dyn CommanderBackend>> + Send + Sync,
>;

/// A [`RemoteBackendFactory`] that constructs no remote backends — every server
/// is rejected as unavailable. For contexts with no remote client wired in
/// (tests, and any frontend that only drives the local backend). When the
/// config has no `remote_servers`, it is never actually invoked.
pub fn no_remote_backends() -> RemoteBackendFactory {
    Arc::new(|cfg| {
        Err(BackendError::Unavailable {
            reason: format!(
                "remote backends are not available in this context ({})",
                cfg.name
            ),
        })
    })
}

/// Whether a backend runs in-process or talks to a remote server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// In-process [`LocalBackend`](local::LocalBackend).
    Local,
    /// A client of a remote `claude-commander-server`.
    Remote,
}

/// Identifies a backend for status display and logging.
#[derive(Debug, Clone)]
pub struct BackendDescriptor {
    /// Human-readable name (e.g. `"local"`, or a remote host label).
    pub name: String,
    pub kind: BackendKind,
}

/// Which UI affordances a backend supports. A remote backend can't drive the
/// operator's local editor or create a tmux session on the server host from
/// here, so the TUI hides those actions when the capability is off. The local
/// backend has them all.
///
/// The in-session switcher is deliberately *not* one of these: the TUI draws it
/// over the pane itself, so it works on every backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    /// Open the operator's `$EDITOR`/GUI editor on a session worktree.
    pub open_editor: bool,
    /// A dedicated commander tmux session.
    pub commander_session: bool,
    /// Ctrl+\ agent↔shell pane toggle.
    pub shell_toggle: bool,
    /// The agent runs on a remote host, so image paste must be captured from the
    /// *client's* local clipboard and uploaded (the remote agent can't read the
    /// operator's clipboard on Ctrl+V). False for a local backend, where the
    /// co-located agent reads the local clipboard itself. Gates the attach
    /// loop's Ctrl+V interception + [`CommanderBackend::paste_image`] upload.
    pub client_side_image_paste: bool,
}

impl BackendCapabilities {
    /// Every affordance enabled — the local backend's set, and the default a
    /// fresh selection assumes until it resolves the owning backend.
    pub const LOCAL: Self = Self {
        open_editor: true,
        commander_session: true,
        shell_toggle: true,
        // The local agent reads the operator's clipboard directly on Ctrl+V, so
        // no client-side capture/upload is needed.
        client_side_image_paste: false,
    };
}

/// A change-feed handle: its generation counter advances whenever the backend's
/// observable state changes, so a consumer re-reads a snapshot on each bump
/// rather than polling on a fixed tick. Backed by a [`watch`] channel — for
/// [`LocalBackend`](local::LocalBackend) it forwards the
/// [`StateStore`](crate::config::StateStore) generation; a remote backend drives
/// it from its own poll/subscription loop.
pub struct BackendChangeFeed {
    rx: watch::Receiver<u64>,
}

impl BackendChangeFeed {
    pub fn new(rx: watch::Receiver<u64>) -> Self {
        Self { rx }
    }

    /// The current generation. Two reads returning the same value mean nothing
    /// changed in between.
    pub fn generation(&self) -> u64 {
        *self.rx.borrow()
    }

    /// Wait until the generation changes. Returns `false` if the sending side is
    /// gone (the backend was dropped), so a consumer loop can exit cleanly.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }
}

/// Index of a backend in the TUI's `Vec<BackendHandle>`. Stable for the process
/// lifetime; `BackendId(0)` is the local backend (the only one this phase).
///
/// The TUI qualifies every session/project reference with a `BackendId` so a
/// future multi-backend tree routes each action to the backend that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BackendId(pub usize);

/// The always-present local backend.
pub const LOCAL_BACKEND_ID: BackendId = BackendId(0);

/// A session qualified by the backend that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionRef {
    pub backend: BackendId,
    pub id: SessionId,
}

impl SessionRef {
    pub fn new(backend: BackendId, id: SessionId) -> Self {
        Self { backend, id }
    }

    /// A ref on the local backend.
    pub fn local(id: SessionId) -> Self {
        Self {
            backend: LOCAL_BACKEND_ID,
            id,
        }
    }
}

/// A backend's connection health, rendered in its server header. The local
/// backend is always [`Connected`](ConnectionState::Connected).
///
/// The definition lives in `claude-commander-protocol` (a plain data enum shared
/// with the transport client, which drives it from its poll loop); it is
/// re-exported here so every existing `backend::ConnectionState` call site is
/// unchanged.
pub use claude_commander_protocol::connection::ConnectionState;

/// The TUI's cached view of one backend: the latest snapshots plus connection
/// health. A per-backend change-feed task refreshes it; the render path reads it
/// synchronously (no `.await` on the hot path).
#[derive(Debug, Clone)]
pub struct BackendView {
    pub snapshot: Snapshot,
    pub agent_states: AgentStatesSnapshot,
    pub connection: ConnectionState,
}

impl BackendView {
    /// An empty view for a backend whose first snapshot hasn't arrived. The
    /// tree renders nothing for it until the change-feed task fills it in.
    pub fn connecting() -> Self {
        Self {
            snapshot: empty_snapshot(),
            agent_states: AgentStatesSnapshot {
                states: Default::default(),
                commander_running: false,
            },
            connection: ConnectionState::Connecting,
        }
    }
}

/// An empty [`Snapshot`] placeholder (no projects/sessions). Used to
/// seed a [`BackendView`] before its first real snapshot lands (and by tests to
/// stand up a `mock::MockBackend` — plain text, since that module only exists
/// under `cfg(test)` or the `test-support` feature, so a link to it would be
/// unresolvable in a normal doc build).
pub fn empty_snapshot() -> Snapshot {
    Snapshot {
        projects: Vec::new(),
        sessions: Vec::new(),
        cascade_paused: None,
        pending_comment_sessions: Vec::new(),
        project_pull: Default::default(),
        operations: Vec::new(),
        server: ServerStatus {
            gh_available: false,
            tmux_ok: false,
            // The client's own version. The version-mismatch warning
            // (`server_version_mismatch`) depends on this: seeding the
            // placeholder with the client version means `server == client`
            // before the first real snapshot lands, so a connecting backend
            // never flags a false mismatch. A `"0.0.0"` seed would wrongly warn
            // during "connecting…".
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        workspaces: Vec::new(),
        main_workspace: None,
        startup_workspace: Default::default(),
    }
}

/// A detected skew where a backend's server build is behind the client build.
/// Rendered as a non-blocking annotation on the server header; carries both
/// full version strings verbatim for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMismatch {
    pub server: String,
    pub client: String,
}

/// The leading ASCII-digit run of `s` as a `u64`, or `None` if it doesn't start
/// with a digit (so a component like `"0-rc"` yields `0` and `"v0"` yields
/// `None`). Overflowing values also yield `None` (via `parse`). Allocation-free:
/// this runs per backend on every `refresh_list_items`.
fn leading_number(s: &str) -> Option<u64> {
    let end = s.bytes().take_while(u8::is_ascii_digit).count();
    // `end` is a valid boundary (ASCII digits are single-byte); an empty prefix
    // parses to `Err` → `None`.
    s.get(..end)?.parse().ok()
}

/// Parse `(major, minor)` from a version string, ignoring patch and any
/// pre-release/build suffix. Splits on `.`, takes the first two components, and
/// reads the leading digit run of each. `None` if there are fewer than two
/// components or either leading-digit run is empty.
fn major_minor(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.split('.');
    let major = leading_number(parts.next()?)?;
    let minor = leading_number(parts.next()?)?;
    Some((major, minor))
}

/// `Some(VersionMismatch)` when the server's major.minor is strictly behind the
/// client's — the direction where the client may call endpoints the older
/// server lacks. `None` when equal, newer, or unparseable (conservative: no
/// false alarms).
///
/// Deliberately one-directional: a client *older* than its server is not warned.
/// That skew tends to surface as deserialization errors driving the remote
/// [`Degraded`](ConnectionState::Degraded) anyway, and warning both ways would
/// nag every user who hasn't upgraded to a server's latest point release.
pub fn server_version_mismatch(server: &str, client: &str) -> Option<VersionMismatch> {
    let server_mm = major_minor(server)?;
    let client_mm = major_minor(client)?;
    (server_mm < client_mm).then(|| VersionMismatch {
        server: server.to_string(),
        client: client.to_string(),
    })
}

/// One backend the TUI drives: its id, the trait object (cloneable across
/// `tokio::spawn`), and the cached [`BackendView`].
///
/// A handle owns the background tasks feeding its view (the change-feed and, for
/// a remote backend, the connection-watch task). Dropping the handle aborts
/// them, so removing a backend on config hot-reload tears down its polling
/// rather than leaking a task that would poll a server forever (the task holds
/// its own `Arc` to the backend, so dropping the handle alone wouldn't stop it).
pub struct BackendHandle {
    pub id: BackendId,
    pub backend: Arc<dyn CommanderBackend>,
    pub view: BackendView,
    /// Background feed tasks; aborted on drop. Populated by the TUI after
    /// spawning; empty until then and for backends with no feeds.
    pub feed_tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl BackendHandle {
    /// Wrap a backend as handle `id` with an empty (connecting) view.
    pub fn new(id: BackendId, backend: Arc<dyn CommanderBackend>) -> Self {
        Self {
            id,
            backend,
            view: BackendView::connecting(),
            feed_tasks: Vec::new(),
        }
    }
}

impl Drop for BackendHandle {
    fn drop(&mut self) {
        for task in &self.feed_tasks {
            task.abort();
        }
    }
}

/// Which pane of a session to attach to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKind {
    /// The agent (e.g. Claude) pane — the session's primary tmux session.
    Agent,
    /// The paired shell pane (Ctrl+\ toggles here), created on demand.
    Shell,
}

/// Why an [`AttachConnection`] ended, so a driving loop can decide whether to
/// auto-restart, return to the tree, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEnd {
    /// The pane's process/session ended (PTY EOF or the child exited non-clean).
    SessionEnded,
    /// A clean detach — the attach child was killed but the session lives on.
    Detached,
    /// The transport failed.
    Error(String),
}

/// Resizes an attached terminal. Cheaply cloneable and `Send + Sync`, so a
/// SIGWINCH task (or a `resize` control-frame handler) can hold a clone while
/// the main loop owns the streams. Wraps the transport-specific resize action
/// (a local PTY `ioctl`, or a remote `resize` frame) behind one call.
#[derive(Clone)]
pub struct AttachResizer(Arc<dyn Fn(u16, u16) + Send + Sync>);

impl AttachResizer {
    pub fn new(f: impl Fn(u16, u16) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// Resize to `cols`×`rows`. Fire-and-forget: a failed resize is non-fatal
    /// (the terminal keeps its previous size).
    pub fn resize(&self, cols: u16, rows: u16) {
        (self.0)(cols, rows)
    }
}

/// Forces the attached terminal to repaint its whole visible screen.
///
/// The in-session switcher draws the palette *over* the live pane, so when it
/// closes something has to restore the region it covered. Rather than remember
/// what was underneath, we ask the renderer that owns it — tmux — to paint it
/// again. Verified against a real server: `refresh-client -t <client-tty>` emits
/// the full visible screen, and in copy mode it repaints the scrolled content
/// while leaving `scroll_position`/`copy_cursor_y` untouched. That last part is
/// why this is its own operation and not a resize round-trip: a resize is
/// exactly what loses a scrolled copy-mode anchor.
///
/// Transport-specific, like [`AttachResizer`]: locally a `tmux refresh-client`
/// subprocess, remotely a `refresh` control frame the server turns into the same
/// command. Async because both are I/O; cheaply cloneable so a paused attach can
/// hold one.
#[derive(Clone)]
pub struct AttachRefresher(Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>);

impl AttachRefresher {
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self(Arc::new(move || Box::pin(f())))
    }

    /// Repaint the visible screen. Fire-and-forget: a failed refresh leaves a
    /// stale rectangle, which the next pane output overwrites anyway.
    pub async fn refresh(&self) {
        (self.0)().await
    }
}

impl std::fmt::Debug for AttachRefresher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttachRefresher")
    }
}

impl std::fmt::Debug for AttachResizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttachResizer")
    }
}

/// Teardown + termination-signal half of an attach. Kept separate from the byte
/// streams so a loop can `select!` on `wait()` (natural end) while pumping the
/// reader/writer, and call `detach()` on an explicit user detach.
#[async_trait]
pub trait AttachTerminator: Send {
    /// Explicitly end the attach (deterministic teardown). Locally this kills
    /// the `tmux attach-session` child — detaching the client while the tmux
    /// session and its program keep running; remotely it sends a `detach` frame.
    /// Idempotent.
    async fn detach(&mut self);

    /// Wait for the attach to end on its own (the user pressed tmux's detach
    /// key, the pane's process exited, or the remote sent `detached`) and report
    /// why. After [`Self::detach`] this resolves promptly.
    async fn wait(&mut self) -> AttachEnd;
}

/// The independently-ownable parts of a live attach, ready for an interactive
/// I/O loop: raw byte streams both ways, a resize handle, and a terminator. The
/// reader/writer are boxed trait objects so a local PTY and a remote WebSocket
/// present the same shape.
pub struct AttachStreams {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    pub resizer: AttachResizer,
    pub refresher: AttachRefresher,
    pub terminator: Box<dyn AttachTerminator>,
    /// The tty of the **operator's own** tmux client backing this attach, when
    /// there is one — i.e. only for a local PTY attach. `None` for a remote
    /// attach, whose tmux client lives on the server.
    ///
    /// This is what tells the in-session switcher whether it may move the user
    /// with `tmux switch-client` instead of re-attaching. Getting that wrong is
    /// not a no-op: `switch-client` run without a live local client of our own
    /// either fails, or — worse — succeeds against some *unrelated* client
    /// (another terminal on the same server, or the outer client when commander
    /// itself runs inside tmux) and yanks that one to a session the user never
    /// asked for. Naming the client explicitly makes both mistakes
    /// unrepresentable, so the capability travels with the transport rather than
    /// being inferred at the call site.
    pub local_client_tty: Option<String>,
}

/// A live attach to a session's pane. Transport-agnostic: [`LocalBackend`] backs
/// it with a PTY (`tmux attach-session`), a remote backend with a WebSocket.
///
/// The value is opaque until [`Self::split`], which yields the [`AttachStreams`]
/// an interactive loop drives.
#[async_trait]
pub trait AttachConnection: Send {
    /// Break the connection into its byte streams, resize handle, and
    /// terminator. Consumes the connection.
    fn split(self: Box<Self>) -> AttachStreams;
}

/// Everything the TUI needs from a backend, over protocol DTOs. See the module
/// docs for the two implementations and the object-safety contract.
///
/// Every id-taking method uses the protocol [`SessionId`]/[`ProjectId`] types
/// (which core re-exports), and every response is a protocol DTO — no
/// `WorktreeSession`/`Project` domain type appears here, so a remote backend can
/// satisfy the same trait over the wire.
#[async_trait]
pub trait CommanderBackend: Send + Sync {
    // -- Identity / capabilities (sync; no I/O) --

    fn descriptor(&self) -> BackendDescriptor;
    fn capabilities(&self) -> BackendCapabilities;

    /// Downcast hook for the TUI to reach a concrete backend's local-only
    /// affordances — the ones the trait deliberately doesn't expose because a
    /// remote backend can't satisfy them (name-based attach for the commander /
    /// project shell, the in-session switcher's shell-toggle resolution). The
    /// TUI only downcasts to [`LocalBackend`] after checking
    /// [`Self::capabilities`], so a remote backend never has these called.
    fn as_any(&self) -> &dyn std::any::Any;

    /// A change-feed whose generation advances when observable state changes.
    fn change_feed(&self) -> BackendChangeFeed;

    /// A reactive watch on this backend's [`ConnectionState`], if it has one.
    /// The local and placeholder backends have a fixed health (always
    /// `Connected` / permanently `Degraded`), so the default is `None`; a remote
    /// backend overrides this to expose its poller's connection watch, letting
    /// the TUI update the server header live as the link comes and goes.
    fn connection_watch(&self) -> Option<watch::Receiver<ConnectionState>> {
        None
    }

    /// One-time startup reconciliation (drop stale `Creating` sessions, reset
    /// transient stack states, sync status against live tmux, re-run section
    /// assignment). Run once by the TUI when a backend attaches. A remote
    /// backend's server reconciles itself, so the default is a no-op.
    async fn startup_reconcile(&self) -> BResult<()> {
        Ok(())
    }

    /// Re-run section assignment over every session against current config —
    /// used after a live `[[sections]]` config change. A remote backend
    /// reconciles server-side, so the default is a no-op.
    async fn reconcile_sections(&self) -> BResult<()> {
        Ok(())
    }

    /// Re-run section assignment for a single (freshly created) session. Default
    /// no-op for remote backends.
    async fn reconcile_one_section(&self, _id: SessionId) -> BResult<()> {
        Ok(())
    }

    /// Record a UI-only usage feature (fire-and-forget). Domain features are
    /// recorded inside the service's mutation methods; this covers TUI-only
    /// interactions the trait surface can't otherwise see. A remote backend
    /// records telemetry server-side, so the default is a no-op.
    fn record_feature(&self, _feature: &'static str) {}

    /// Flush any queued telemetry before the frontend exits, so the last
    /// session's events aren't lost to the flush interval. No-op for a remote
    /// backend (telemetry is the server's concern).
    async fn flush_telemetry(&self) {}

    // -- Queries --

    async fn snapshot(&self) -> BResult<Snapshot>;

    /// Bulk agent-state snapshot for active sessions. `fresh` bypasses any TTL
    /// cache and forces a re-capture.
    async fn agent_states(&self, fresh: bool) -> BResult<AgentStatesSnapshot>;

    /// A session's live detail (agent sub-state, diff summary, optional pane
    /// snapshot). `None` when the query resolves to no session.
    async fn session_detail(
        &self,
        query: &str,
        lines: Option<usize>,
    ) -> BResult<Option<SessionDetail>>;

    /// Preview payload for a session or project (agent pane, diff, shell pane).
    async fn preview(&self, target: PreviewTarget) -> BResult<PreviewData>;

    /// The full branch diff (committed vs origin/main plus uncommitted changes)
    /// used for the AI summary.
    async fn branch_diff(&self, id: SessionId) -> BResult<String>;

    /// A project's git branches; `fetch` runs a best-effort `git fetch` first.
    async fn list_branches(&self, project: ProjectId, fetch: bool) -> BResult<Vec<BranchInfo>>;

    /// New-session dialog options (default program, program list, sections).
    async fn create_options(&self) -> BResult<CreateOptions>;

    /// Replace this backend's configured program list (the new-session picker
    /// options). For the local backend this rewrites the local config; for a
    /// remote backend it PUTs to the server, editing *its* config.
    async fn set_programs(&self, programs: Vec<ProgramInfo>) -> BResult<()>;

    /// Session ids with at least one not-yet-applied review comment.
    async fn pending_comment_sessions(&self) -> BResult<Vec<SessionId>>;

    // -- Session mutations --

    async fn create_session(&self, opts: CreateSessionOpts) -> BResult<SessionId>;
    async fn kill_session(&self, id: SessionId) -> BResult<()>;
    async fn restart_session(&self, id: SessionId) -> BResult<()>;
    /// Restart a session with a *fresh* agent conversation (no `--resume`),
    /// discarding whatever the agent would otherwise have resumed. Two callers:
    /// the attach loop, when the agent process exits mid-attach, and the
    /// operator's Reset command.
    ///
    /// Deliberately **required**, not defaulted. It was defaulted to
    /// `restart_session` while only [`LocalBackend`] implemented it, which meant
    /// a remote session's "fresh" restart silently *resumed* — the one thing the
    /// caller asked it not to do. Every backend now answers for itself, so a
    /// backend that cannot restart fresh has to say so rather than quietly
    /// doing the opposite.
    async fn restart_session_fresh(&self, id: SessionId) -> BResult<()>;
    async fn delete_session(&self, id: SessionId) -> BResult<()>;
    async fn rename_session(&self, id: SessionId, title: String) -> BResult<()>;
    /// Change a session's launch program (the agent harness that runs) and
    /// relaunch its pane fresh so the new program takes effect. Runs on the
    /// session's owning host — the local backend delegates to the service; a
    /// remote backend PATCHes the server, which relaunches server-side.
    async fn change_program(&self, id: SessionId, program: String) -> BResult<()>;
    /// Move a session to `section`, or clear its manual override (`None`).
    async fn set_section(&self, id: SessionId, section: Option<String>) -> BResult<()>;
    /// Retarget a session's stack base onto `parent`, or unstack it onto the
    /// project's main branch (`None`). Metadata and PR base only — git history
    /// is not rewritten. Runs on the session's owning host, since the durable
    /// half is a `gh pr edit` against that host's checkout.
    async fn set_session_base(
        &self,
        id: SessionId,
        parent: Option<SessionId>,
    ) -> BResult<SetSessionBaseOutcome>;
    /// Clear a session's unread flag.
    async fn mark_read(&self, id: SessionId) -> BResult<()>;
    /// Flip a session's keep-alive (hibernation-exempt) flag; returns the new
    /// value. The flag lives with the session's owning host, so remote
    /// backends toggle it server-side.
    async fn toggle_keep_alive(&self, id: SessionId) -> BResult<bool>;
    /// Mark a batch of sessions unread (paired with [`Self::mark_read`]).
    async fn mark_unread(&self, ids: Vec<SessionId>) -> BResult<()>;

    /// Upload a pasted image (PNG bytes) for a session and inject its file path
    /// into the agent pane. Only meaningful for backends whose
    /// [`Self::capabilities`] set `client_side_image_paste` (i.e. remote): the
    /// TUI captures the operator's local clipboard image and hands the bytes
    /// here. The default rejects the call — a local backend never needs it (the
    /// co-located agent reads the clipboard itself).
    async fn paste_image(&self, _id: SessionId, _png: Vec<u8>) -> BResult<()> {
        Err(BackendError::InvalidRequest(
            "image paste is not supported by this backend".into(),
        ))
    }

    /// Persist a batch of PR-check results and refresh status bars. Takes the
    /// core `PrCheckResult` because PR polling is a *local* capability (the
    /// TUI's background loop drives it); a remote backend's server polls and
    /// persists PR state itself, so the default is a no-op.
    async fn apply_pr_results(
        &self,
        _results: Vec<(SessionId, crate::git::PrCheckResult)>,
    ) -> BResult<()> {
        Ok(())
    }

    /// Trigger an immediate PR-metadata refresh. [`LocalBackend`] wakes its
    /// background PR-status loop; a remote backend POSTs `/api/pr-refresh` so the
    /// server re-checks. No default — every backend must route the request to
    /// where PR polling actually happens.
    async fn request_pr_refresh(&self) -> BResult<()>;

    // -- Projects --

    /// Register `path` as a project, tagged with `workspace` (`None` = Main) —
    /// frontends pass their active workspace so a new project lands in it.
    async fn add_project(
        &self,
        path: std::path::PathBuf,
        workspace: Option<String>,
    ) -> BResult<ProjectId>;

    /// Register `path` as a project, or answer with the id of the project already
    /// registered for it.
    ///
    /// The idempotent counterpart to [`Self::add_project`], and the one a
    /// "register this existing checkout" offer must use — the path it was handed
    /// is frequently already a project (that is *why* the destination was
    /// occupied), and `add_project` would register a second entry for the same
    /// repository.
    ///
    /// No default: the dedupe belongs where the projects live, so a backend has to
    /// route it to its own host rather than inherit an answer composed from a
    /// snapshot a frontend happens to hold.
    ///
    /// `workspace` tags the project only if this call registers it; an existing
    /// project keeps its workspace.
    async fn ensure_project(
        &self,
        path: std::path::PathBuf,
        workspace: Option<String>,
    ) -> BResult<ProjectId>;

    async fn remove_project(&self, id: ProjectId) -> BResult<()>;

    // -- Workspaces --
    //
    // No defaults: a workspace edit is sent eagerly to every connected backend,
    // and each must say for itself whether it landed (a frontend toasts the
    // ones that failed). Rename and delete are idempotent on a backend that
    // never had the workspace, so that propagation is safe to repeat.

    /// Replace this backend's workspace definitions (and, when set, Main's
    /// label/colour and the startup choice). Never re-tags projects.
    async fn set_workspaces(&self, req: SetWorkspacesRequest) -> BResult<()>;

    /// Rename a workspace, rewriting every project tagged with it. A no-op
    /// success when this backend has no such workspace.
    async fn rename_workspace(&self, from: String, to: String) -> BResult<()>;

    /// Delete a workspace, moving its projects to Main. A no-op success when
    /// this backend has no such workspace.
    async fn delete_workspace(&self, name: String) -> BResult<()>;

    /// Move a project into `workspace` (`None` = Main), defining the workspace
    /// on this backend if it is new here.
    async fn set_project_workspace(&self, id: ProjectId, workspace: Option<String>) -> BResult<()>;
    /// Scan `dir` for git repositories and register them, each new one tagged
    /// with `workspace` (`None` = Main).
    async fn scan_directory(
        &self,
        dir: std::path::PathBuf,
        workspace: Option<String>,
    ) -> BResult<crate::session::ScanResult>;

    // -- Repository clone --
    //
    // No capability flag gates these: both real backends genuinely support them.
    // A clone runs where the *sessions* run (a project registered on the server
    // host has to be checked out on the server host), so the local backend clones
    // locally and a remote backend asks its server to clone — same feature, and
    // nothing for a frontend to gate on. `list_github_repos` can still *fail* as
    // `Unavailable` when the host has no `gh`; that is a runtime state a picker
    // renders, not a static capability.

    /// Every GitHub repo the backend's host can clone, for the repo picker.
    /// Resolved by `gh` on that host, so its authenticated account decides the
    /// list — a remote backend shows the *server's* repos, not the operator's.
    ///
    /// A host without `gh` fails with [`BackendError::Unavailable`] rather than
    /// answering an empty list: "install the GitHub CLI" and "this account has no
    /// repos" are different states and a picker renders them differently.
    async fn list_github_repos(&self) -> BResult<Vec<GithubRepo>>;

    /// Start cloning a repository into the host's projects directory, answering
    /// with the created job.
    ///
    /// The whole [`CloneJob`] rather than just its id, matching the server's 202
    /// body: a caller gets the id, the destination and the redacted source label
    /// in one call and can start polling without a second round trip.
    ///
    /// **Its status is not a terminal status** — the clone has only been accepted.
    /// Every outcome, including a destination that was already occupied, arrives
    /// through [`Self::clone_job`]; a caller that reads this status as final will
    /// miss every result.
    ///
    /// A refused source (empty, flag-shaped, unsupported scheme, escaping
    /// destination name) is [`BackendError::InvalidRequest`] on every backend, and
    /// its message is redacted where it is built, so a credentialed URL cannot
    /// come back through it.
    async fn start_clone(&self, req: CloneRequest) -> BResult<CloneJob>;

    /// Poll a clone job. `Ok(None)` once it has been pruned (or was never issued)
    /// — absence rather than an error, so a poll loop can tell "gone" from "the
    /// backend is broken" and only stop retrying for one of them.
    ///
    /// No polling cadence is implied here: a frontend owns the interval.
    async fn clone_job(&self, id: CloneJobId) -> BResult<Option<CloneJob>>;

    // -- Cascade / push-stack --

    async fn cascade_merge(&self, id: SessionId) -> BResult<OperationStatus>;
    async fn cascade_resume(&self) -> BResult<OperationStatus>;
    async fn cascade_abandon(&self) -> BResult<()>;
    async fn push_stack(&self, id: SessionId) -> BResult<OperationStatus>;

    // -- Review / comments --

    /// A session's stored comments without re-anchoring (the lighter refresh
    /// the review view uses when only comments, not the diff, may have changed).
    async fn list_comments(&self, id: SessionId) -> BResult<Vec<crate::comment::Comment>>;

    async fn open_review(&self, id: SessionId) -> BResult<ReviewSnapshot>;
    /// Re-compose the review diff; `None` when unchanged from `prev_hash`.
    async fn refresh_review_if_changed(
        &self,
        id: SessionId,
        prev_hash: u64,
    ) -> BResult<Option<ReviewSnapshot>>;
    async fn create_comment(&self, id: SessionId, draft: NewComment) -> BResult<Uuid>;
    async fn delete_comment(&self, id: SessionId, comment_id: Uuid) -> BResult<()>;
    async fn apply_comments(&self, id: SessionId) -> BResult<ApplyOutcome>;
    /// Set or clear a file's reviewed mark by display path against the current
    /// diff, returning the new state. Explicit rather than a toggle: the view
    /// says which way the user meant, so a mark set elsewhere since the view
    /// last refreshed can't turn a "mark" keypress into an unmark.
    async fn set_file_reviewed(
        &self,
        id: SessionId,
        display_path: String,
        reviewed: bool,
    ) -> BResult<bool>;
    /// The session's stored reviewed marks, unpruned. Polled by an open
    /// review view to pick up marks set elsewhere (the CLI, another frontend);
    /// a mark applies only to the file content it is pinned to
    /// (`claude_commander_viewmodel::review::fold_marks`).
    async fn reviewed_marks(&self, id: SessionId) -> BResult<Vec<ReviewedFileMark>>;
    /// Raw bytes of one side of a binary file in a session's review diff.
    async fn fetch_diff_blob(
        &self,
        id: SessionId,
        side: DiffSide,
        path: String,
    ) -> BResult<Vec<u8>>;

    // -- Attach --

    /// Open a live attach to a session's `kind` pane, sized `cols`×`rows`. Stamps
    /// the session's last-attached time. The returned connection is split by the
    /// caller into the streams an interactive loop drives.
    async fn attach(
        &self,
        id: SessionId,
        cols: u16,
        rows: u16,
        kind: AttachKind,
    ) -> BResult<Box<dyn AttachConnection>>;
}

/// Compile-time proof the trait is object-safe and usable across `tokio::spawn`:
/// an `Arc<dyn CommanderBackend>` must be `Send + Sync + 'static`.
#[allow(dead_code)]
fn _assert_object_safe(b: Arc<dyn CommanderBackend>) {
    fn is_send_sync_static<T: Send + Sync + 'static>(_: &T) {}
    is_send_sync_static(&b);
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_commander_protocol::github::{CloneSource, CloneStatus};

    #[test]
    fn backend_id_local_is_zero() {
        assert_eq!(LOCAL_BACKEND_ID, BackendId(0));
    }

    #[test]
    fn session_ref_local_uses_local_backend() {
        let id = SessionId::new();
        let r = SessionRef::local(id);
        assert_eq!(r.backend, LOCAL_BACKEND_ID);
        assert_eq!(r.id, id);
        assert_eq!(r, SessionRef::new(LOCAL_BACKEND_ID, id));
    }

    #[test]
    fn connecting_view_is_empty_and_connecting() {
        let v = BackendView::connecting();
        assert!(v.snapshot.projects.is_empty());
        assert!(v.snapshot.sessions.is_empty());
        assert!(v.agent_states.states.is_empty());
        assert_eq!(v.connection, ConnectionState::Connecting);
    }

    #[test]
    fn version_mismatch_flags_older_server() {
        // Server behind at the minor level, or the major level, warns.
        assert_eq!(
            server_version_mismatch("0.24.3", "0.25.0"),
            Some(VersionMismatch {
                server: "0.24.3".to_string(),
                client: "0.25.0".to_string(),
            })
        );
        assert!(server_version_mismatch("0.9.0", "1.0.0").is_some());
    }

    #[test]
    fn version_mismatch_silent_when_not_older() {
        // Equal, newer, and patch-only-older all stay quiet.
        assert_eq!(server_version_mismatch("0.25.0", "0.25.0"), None);
        assert_eq!(server_version_mismatch("0.26.0", "0.25.0"), None);
        // Patch-only difference: major.minor are equal, so no warning.
        assert_eq!(server_version_mismatch("0.25.0", "0.25.1"), None);
    }

    #[test]
    fn version_mismatch_ignores_prerelease_and_build_suffixes() {
        // Suffix on the patch component doesn't affect major.minor.
        assert!(server_version_mismatch("0.24.0-rc.1", "0.25.0").is_some());
        assert_eq!(server_version_mismatch("0.25.0-rc.1", "0.25.0"), None);
        assert_eq!(server_version_mismatch("0.25.0+abc123", "0.25.0"), None);
    }

    #[test]
    fn version_mismatch_none_on_unparseable() {
        // A malformed server version (or client) never warns.
        for bad in ["", "abc", "1", "v0.25.0"] {
            assert_eq!(server_version_mismatch(bad, "0.25.0"), None, "server={bad}");
            assert_eq!(server_version_mismatch("0.24.0", bad), None, "client={bad}");
        }
    }

    #[test]
    fn version_mismatch_absent_for_connecting_placeholder() {
        // The placeholder seeds the client's own version, so a backend that has
        // not yet received a real snapshot must not flag a mismatch.
        let v = BackendView::connecting();
        assert_eq!(
            server_version_mismatch(&v.snapshot.server.version, crate::VERSION),
            None
        );
    }

    /// A [`GithubRepo`] with the fields a picker actually reads, for the
    /// trait-surface round trip below.
    fn repo(full_name: &str) -> GithubRepo {
        let (owner, name) = full_name.split_once('/').unwrap();
        GithubRepo {
            full_name: full_name.to_string(),
            owner: owner.to_string(),
            name: name.to_string(),
            description: None,
            private: false,
            fork: false,
            archived: false,
            default_branch: "main".to_string(),
            clone_url: format!("https://github.com/{full_name}.git"),
            ssh_url: format!("git@github.com:{full_name}.git"),
            pushed_at: None,
        }
    }

    /// The clone surface has to be reachable through the **trait object** the TUI
    /// holds — a palette command drives `Arc<dyn CommanderBackend>`, never a
    /// concrete backend — so this exercises all three methods there: the repo
    /// list, the start (which answers with the whole job, matching the server's
    /// 202 body), and the poll that resolves the id it handed back.
    #[tokio::test]
    async fn clone_surface_round_trips_through_a_trait_object() {
        let mock = Arc::new(mock::MockBackend::new("buildbox", empty_snapshot()));
        mock.set_github_repos(vec![repo("octo/widget")]);
        let backend: Arc<dyn CommanderBackend> = mock.clone();

        let repos = backend.list_github_repos().await.unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].full_name, "octo/widget");

        let req = CloneRequest {
            source: CloneSource::Github {
                full_name: "octo/widget".to_string(),
            },
            dest_name: None,
            workspace: None,
        };
        let job = backend.start_clone(req.clone()).await.unwrap();
        assert!(
            matches!(job.status, CloneStatus::Running),
            "a freshly started clone is running, got {:?}",
            job.status
        );
        assert_eq!(mock.clone_requests(), vec![req]);

        // The id the start handed back resolves through the poll method…
        assert_eq!(backend.clone_job(job.id).await.unwrap(), Some(job.clone()));
        // …and an id that was never issued is absence, not a failure: a pruned or
        // bogus job must not read as a transport error.
        assert_eq!(backend.clone_job(CloneJobId::new()).await.unwrap(), None);

        // The outcome arrives through the *poll*, never through the start's
        // response — which is why the trait documents that status as non-terminal.
        let project_id = ProjectId::new();
        mock.set_clone_status(job.id, CloneStatus::Succeeded { project_id });
        assert_eq!(
            backend.clone_job(job.id).await.unwrap().unwrap().status,
            CloneStatus::Succeeded { project_id }
        );
    }

    /// A downed backend has to surface as `Unavailable` on the clone surface too,
    /// not as an empty repo list — a picker that renders "no repos" for an
    /// unreachable server is indistinguishable from an empty account.
    #[tokio::test]
    async fn clone_surface_reports_a_failing_backend() {
        let mock = mock::MockBackend::new("buildbox", empty_snapshot());
        mock.set_failing(true);
        assert!(matches!(
            mock.list_github_repos().await.unwrap_err(),
            BackendError::Unavailable { .. }
        ));
        assert!(matches!(
            mock.start_clone(CloneRequest {
                source: CloneSource::Github {
                    full_name: "octo/widget".to_string()
                },
                dest_name: None,
                workspace: None,
            })
            .await
            .unwrap_err(),
            BackendError::Unavailable { .. }
        ));
        assert!(matches!(
            mock.clone_job(CloneJobId::new()).await.unwrap_err(),
            BackendError::Unavailable { .. }
        ));
    }

    #[test]
    fn backend_handle_new_seeds_connecting_view() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut config = crate::config::Config::default();
        config.telemetry.enabled = false;
        // `projects_dir` defaults to the user's REAL `~/Projects`, which the
        // repo-clone paths write into. Pin it under the temp dir.
        config.projects_dir = Some(dir.path().join("projects"));
        let config_store = std::sync::Arc::new(crate::config::ConfigStore::with_path(
            config,
            dir.path().join("config.toml"),
        ));
        let store = std::sync::Arc::new(crate::config::StateStore::with_path(
            crate::config::storage::AppState::default(),
            dir.path().join("state.json"),
        ));
        let service = crate::api::CommanderService::new(
            config_store,
            store,
            crate::telemetry::FrontendInfo::new("test", "0.0.0"),
        );
        let backend: Arc<dyn CommanderBackend> = Arc::new(LocalBackend::new(service));
        let handle = BackendHandle::new(LOCAL_BACKEND_ID, backend);
        assert_eq!(handle.id, LOCAL_BACKEND_ID);
        assert_eq!(handle.view.connection, ConnectionState::Connecting);
    }
}
