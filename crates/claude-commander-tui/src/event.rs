//! Event handling for the TUI
//!
//! Provides an async event stream that combines:
//! - Terminal input events (keyboard, mouse)
//! - Application state updates
//! - Render ticks

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use claude_commander_core::config::keybindings::{BindableAction, KeyBindings};
use claude_commander_core::git::{DiffInfo, EnrichedPrInfo};
use claude_commander_core::session::{ProjectId, SessionId};

use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

/// Application events
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Terminal input event
    Input(InputEvent),
    /// State update from background task
    StateUpdate(StateUpdate),
    /// Render tick
    Tick,
    /// Request to quit the application
    Quit,
}

/// Input events from the terminal
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// Key press
    Key(KeyEvent),
    /// Mouse event (if enabled)
    Mouse(crossterm::event::MouseEvent),
    /// Terminal resize
    Resize(u16, u16),
    /// Bracketed paste
    Paste(String),
}

/// Which flavour of pane relaunch a [`StateUpdate::RestartFinished`] is
/// reporting, so the toast and the error prefix match what the operator asked
/// for. `Restart` covers both the plain restart and a program change (which
/// relaunches the pane); `Reset` is the no-resume relaunch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartKind {
    Restart,
    Reset,
}

impl RestartKind {
    /// Transient status-bar message on success.
    pub fn success_toast(self) -> &'static str {
        match self {
            Self::Restart => "Session restarted",
            Self::Reset => "Session reset — fresh conversation",
        }
    }

    /// Leading clause of the error modal's message, before `: {error}`.
    pub fn error_prefix(self) -> &'static str {
        match self {
            Self::Restart => "Failed to restart",
            Self::Reset => "Failed to reset",
        }
    }
}

/// State updates from background tasks
#[derive(Debug, Clone)]
pub enum StateUpdate {
    /// Session content updated
    ContentUpdated {
        session_id: SessionId,
        content_hash: u64,
    },
    /// Session status changed
    StatusChanged { session_id: SessionId },
    /// Diff updated
    DiffUpdated { session_id: SessionId },
    /// A project was registered on `backend_id` from a background task (the
    /// "register the existing checkout" answer to an occupied clone
    /// destination). The handler refreshes that backend's view and the tree.
    ProjectAdded {
        /// Backend the project was added to; indexes `Vec<BackendHandle>`.
        backend_id: usize,
        project_id: ProjectId,
    },
    /// Session added
    SessionAdded { session_id: SessionId },
    /// Session removed
    SessionRemoved { session_id: SessionId },
    /// Error occurred
    Error { message: String },
    /// Session creation completed successfully
    SessionCreated {
        session_id: SessionId,
        /// Backend the session was created on, so the handler refreshes the
        /// right view (and section-reconciles the right backend) before it
        /// tries to select the new row. Indexes the `Vec<BackendHandle>`.
        backend_id: usize,
    },
    /// Session creation failed. The backend removes its own half-created
    /// (`Creating`) session on failure, so this carries only the message.
    SessionCreateFailed { message: String },
    /// Result of the add-remote-server connection probe. `Ok(tmux_ok)` means
    /// the server answered a workspace-snapshot request (reachable + auth
    /// accepted); the flag carries its tmux health for the success toast.
    RemoteServerProbed {
        /// Matches `App::probe_nonce` at spawn time; a stale probe (the user
        /// dismissed the flow and possibly opened some other Loading modal)
        /// is dropped instead of writing config.
        nonce: u64,
        server: claude_commander_core::config::RemoteServerConfig,
        result: Result<bool, String>,
    },
    /// Enriched PR info ready from background fetch
    EnrichedPrReady {
        /// Generation token for the in-flight guard — see
        /// [`StateUpdate::PreviewReady::spawned_at`].
        spawned_at: Instant,
        session_id: SessionId,
        info: Option<EnrichedPrInfo>,
    },
    /// AI-generated branch summary ready
    AiSummaryReady {
        session_id: SessionId,
        result: Result<String, String>,
        /// Hash of the diff that was summarized (for cache keying)
        diff_hash: u64,
    },
    /// A backend's change-feed fired: a fresh workspace snapshot (and agent
    /// states) fetched off the render path, to fold into that backend's cached
    /// [`BackendView`](claude_commander_core::backend::BackendView). `backend_id` indexes the
    /// TUI's `Vec<BackendHandle>`.
    BackendChanged {
        backend_id: usize,
        snapshot: Box<claude_commander_core::api::Snapshot>,
        states: Box<claude_commander_core::api::AgentStatesSnapshot>,
    },
    /// A backend's connection health changed (a remote server's poller moved
    /// between Connecting/Connected/Degraded). Folded into that backend's
    /// [`BackendView::connection`](claude_commander_core::backend::BackendView) so its server
    /// header re-renders live. `backend_id` indexes the `Vec<BackendHandle>`.
    BackendConnection {
        backend_id: usize,
        state: claude_commander_core::backend::ConnectionState,
    },
    /// The Checkout modal's initial (no-fetch) branch listing finished on a
    /// background task — populate the modal's list without clearing `fetching`,
    /// since the fetch-refresh is still running and will clear it. Spawned so a
    /// slow remote listing never blocks the event loop before the modal opens.
    CheckoutBranchesLoaded {
        project_id: ProjectId,
        branches: Vec<(String, bool)>,
    },
    /// Background `git fetch origin` kicked off by the Checkout modal
    /// has finished — the modal should refresh its branch list if still open.
    CheckoutFetchComplete {
        project_id: ProjectId,
        /// Fresh branch list produced after the fetch completed.
        branches: Vec<(String, bool)>,
    },
    /// A background session restart finished. `Ok` toasts success; `Err` carries
    /// a transport/backend error string. `backend_id` indexes the backend the
    /// restart ran on, so the post-op refresh hits the right view. `kind` picks
    /// the wording, since a reset and a restart are different promises to the
    /// operator.
    RestartFinished {
        backend_id: usize,
        kind: RestartKind,
        result: std::result::Result<(), String>,
    },
    /// A background per-session mutation (rename, section move) applied — refresh
    /// the owning backend's view + tree and keep the session selected. Spawned so
    /// a slow remote mutation never blocks the event loop.
    SessionMutationApplied {
        backend_id: usize,
        session_id: SessionId,
    },
    /// A newly-opened New Session modal's options finished loading from a remote
    /// backend's `create_options` on a background task — patch the program and
    /// section pickers in place if that modal is still open for the same project.
    /// Spawned so a slow remote never blocks the event loop before the modal
    /// appears.
    NewSessionProgramsLoaded {
        project_id: ProjectId,
        /// The remote's supported programs, or `None` when it offered none (the
        /// local fallback picker is then left in place).
        picker: Option<super::app::ProgramPicker>,
        /// The backend's configured section names (catch-all excluded), so the
        /// modal's section picker can be rebuilt for the remote backend.
        sections: Vec<String>,
    },
    /// The owning backend's program list finished loading for the open
    /// change-program palette. Replaces the palette's fallback choices (seeded
    /// from local config) if it's still open for the same session. Spawned so a
    /// slow remote never blocks the event loop before the palette appears.
    ProgramChoicesLoaded {
        session_id: SessionId,
        choices: Vec<claude_commander_core::config::ProgramEntry>,
    },
    /// A remote backend's program list finished loading (or failed) for the
    /// Settings → Programs tab. Applied only if that tab is still open for the
    /// same `backend` and `gen` (a stale response for a superseded target is
    /// dropped). Unlike [`Self::NewSessionProgramsLoaded`], this carries the
    /// error so the tab can surface a failed fetch (it has no local fallback).
    ServerProgramsLoaded {
        backend: claude_commander_core::backend::BackendId,
        generation: u64,
        result: std::result::Result<Vec<claude_commander_core::config::ProgramEntry>, String>,
    },
    /// A background program-list PUT to a remote server failed. Surfaced as an
    /// in-tab error if the Programs tab is still open for that backend (the
    /// working copy is unaffected); otherwise logged, never hijacking the UI.
    ServerProgramsSaveFailed {
        backend: claude_commander_core::backend::BackendId,
        message: String,
    },
    /// Preview / shell / diff data ready from a background fetch, applied only
    /// if the same thing is still selected. One fetch feeds every consumer: the
    /// list views' right pane (`preview_content` / `shell_content`) and the Info
    /// modal's diffstat (`diff_info`) — `backend.preview()` returns all three in
    /// a single round trip, so splitting them would double the traffic.
    PreviewReady {
        /// The value the in-flight guard was set to when this fetch was spawned,
        /// used as a generation token: the guard is cleared only by the result
        /// that owns it. Comparing selections instead is not enough — a
        /// selection can change without a respawn (a `StateUpdate` that shifts
        /// the cursor), which would strand the guard until its 5s backstop.
        spawned_at: Instant,
        /// Selected session at spawn time, or `None` when a project was selected.
        session_id: Option<SessionId>,
        /// Selected project at spawn time, or `None` when a session was selected.
        project_id: Option<ProjectId>,
        /// Agent-pane capture (empty for a project, which has no agent pane).
        preview_content: String,
        /// Shell-pane capture, or a placeholder when no shell session exists.
        shell_content: String,
        diff_info: Arc<DiffInfo>,
    },
    /// Cascade-merge background task finished (completed, paused on conflict,
    /// or errored). The TUI refreshes and shows a toast from the recorded
    /// [`OperationStatus`](claude_commander_core::api::OperationStatus). `Err` carries a
    /// transport/backend error string (the operation never recorded).
    CascadeFinished {
        /// Backend the cascade ran on, so the post-op refresh hits the right view.
        backend_id: usize,
        result: std::result::Result<claude_commander_core::api::OperationStatus, String>,
    },
    /// A stack-base retarget finished. Carries `session_id` as well as the
    /// backend because the session re-parents and moves in the tree, so the
    /// handler must reselect it as well as refresh — and the outcome names
    /// whether the durable PR edit landed.
    SetSessionBaseFinished {
        backend_id: usize,
        session_id: claude_commander_core::session::SessionId,
        result: std::result::Result<claude_commander_core::api::SetSessionBaseOutcome, String>,
    },
    /// Push-stack background task finished. `Ok` carries the recorded
    /// [`OperationStatus`](claude_commander_core::api::OperationStatus) (its `detail` summarises
    /// how many branches pushed); `Err` carries a transport/backend error.
    PushStackFinished {
        /// Backend the push ran on, so the post-op refresh hits the right view.
        backend_id: usize,
        result: std::result::Result<claude_commander_core::api::OperationStatus, String>,
    },
    /// `Cascade abandon` background task finished — the paused cascade was
    /// cleared (or the clear failed). Spawned so a slow/remote backend never
    /// blocks the event loop; the TUI refreshes and toasts on arrival.
    CascadeAbandonFinished {
        /// Backend whose paused cascade was abandoned.
        backend_id: usize,
        result: std::result::Result<(), String>,
    },
    /// The GitHub repo listing for the open clone picker finished (or failed).
    /// Spawned off the event loop because `gh api --paginate` has no server-side
    /// timeout and a large account takes many seconds — blocking here would
    /// freeze the UI for the whole listing.
    GithubReposLoaded {
        /// Backend the listing was requested from; indexes `Vec<BackendHandle>`.
        backend_id: usize,
        /// The picker generation this fetch was spawned under. A late arrival
        /// whose generation no longer matches (the user pressed Ctrl-R again) is
        /// dropped rather than clobbering the newer listing's state.
        generation: u64,
        /// `Err` carries a *state*, not a dead end: a host without `gh`, or an
        /// unauthenticated one, still leaves the picker's URL path usable.
        result: std::result::Result<Vec<claude_commander_protocol::github::GithubRepo>, String>,
    },
    /// A poll of an in-flight clone job came back. Emitted roughly once a second
    /// by the poll task until the job reaches a terminal status; there is no
    /// cancellation — jobs are bounded server-side by `clone_timeout_secs`.
    CloneJobUpdated {
        /// Backend running the clone; indexes `Vec<BackendHandle>`.
        backend_id: usize,
        /// The source the clone was started from, carried so the handler can
        /// re-offer a destination-name prompt when the first name turned out to
        /// be occupied — without parking the pending request on `AppUiState`.
        ///
        /// This is the string the user typed, so a `CloneSource::Url` may carry
        /// `user:token@` credentials. Never log it and never render it: pass it
        /// through `redact_credentials` first (as `spawn_clone` and
        /// `apply_clone_job_update` do), or prefer the job's already-redacted
        /// `source_label`. Nothing Debug-prints a `StateUpdate` today, and that
        /// is load-bearing here.
        source: claude_commander_protocol::github::CloneSource,
        /// `Ok(Some(job))` is a poll result (terminal or not); `Ok(None)` means
        /// the job was pruned or never issued (absence, not failure); `Err`
        /// carries a transport/backend error and stops the poll.
        result: std::result::Result<Option<claude_commander_protocol::github::CloneJob>, String>,
    },
    /// A background `git lfs pull` for a freshly-created session finished
    /// (success or failure), so its `⇣ LFS` row marker can be cleared.
    LfsPullFinished { session_id: SessionId },
    /// Review diff prepared off-thread: the parsed diff plus its warmed render
    /// caches (word-diff segments + syntax highlighting), ready to replace the
    /// loading spinner with the full review view. Boxed — the payload is large.
    ReviewPrepared {
        prepared: Box<super::app::ReviewPrepared>,
    },
    /// The open-review fetch (now spawned off the event loop) finished without a
    /// viewable diff: `None` means the session has no changes (a status toast),
    /// `Some(err)` means the fetch failed (an error modal). Distinct from
    /// [`ReviewPrepared`](Self::ReviewPrepared), which carries a ready view.
    ReviewOpenFailed { error: Option<String> },
    /// A binary review image finished loading off-thread: decoded bytes for one
    /// side of one file, ready to build a render protocol from (on the main
    /// thread, which owns the `Picker`). `Arc` keeps the enum cheap to clone.
    ReviewImageLoaded {
        /// The review generation this fetch was spawned under. A late arrival
        /// whose generation no longer matches the open review is dropped.
        generation: u64,
        path: String,
        side: claude_commander_core::api::DiffSide,
        image: std::result::Result<Arc<image::DynamicImage>, String>,
    },
    /// A file's working-tree content finished loading off the event loop, for
    /// GitHub-style context expansion. Folded into the open review view's
    /// per-file line cache so revealed context has concrete text to render.
    ReviewFileLines {
        /// The review generation this fetch was spawned under; a stale arrival
        /// (review since closed/reopened) is dropped.
        generation: u64,
        session_id: SessionId,
        path: String,
        lines: std::result::Result<Arc<Vec<String>>, String>,
    },
    /// An open review view's diff was re-composed in the background (agent went
    /// idle, or a manual refresh). `refreshed` carries the fresh, warmed payload
    /// to fold into the view in place; `None` means the working tree was
    /// unchanged. `manual` distinguishes a user-pressed refresh (which reports
    /// an outcome) from the automatic idle-triggered one (silent). Boxed — the
    /// payload is large.
    ReviewRefreshed {
        refreshed: Option<Box<super::app::ReviewPrepared>>,
        manual: bool,
    },
    /// The open review's stored reviewed marks, polled so marks set elsewhere
    /// (the `review` CLI, another frontend) show up live. `generation` is the
    /// view's `reviewed_gen` at spawn time; a stale poll is dropped. `None`
    /// when the read failed (reported only to clear the in-flight guard).
    ReviewedMarksPolled {
        session_id: SessionId,
        generation: u64,
        marks: Option<Vec<claude_commander_protocol::api::ReviewedFileMark>>,
    },
}

/// User commands triggered by input
#[derive(Debug, Clone)]
pub enum UserCommand {
    /// Navigate up in the list
    NavigateUp,
    /// Navigate down in the list
    NavigateDown,
    /// Jump to the next project/section header in the list
    NextGroup,
    /// Jump to the previous project/section header in the list
    PreviousGroup,
    /// Jump to the first item in the list
    NavigateFirst,
    /// Jump to the last item in the list
    NavigateLast,
    /// Move to the previous board column (or sidebar)
    NavigateLeft,
    /// Move to the next board column (or sidebar)
    NavigateRight,
    /// Select/attach to current item
    Select,
    /// Open shell in worktree
    SelectShell,
    /// Create new session
    NewSession,
    /// Create a new session stacked on the selected session's branch
    NewStackedSession,
    /// Retarget the selected session's stack base onto another session, or
    /// unstack it onto the project's main branch
    SetSessionBase,
    /// Cascade-merge main through the selected session's stack
    CascadeMergeMain,
    /// Resume a cascade-merge that paused on conflicts
    CascadeResume,
    /// Abandon a paused cascade-merge without continuing
    CascadeAbandon,
    /// Push every branch in the selected session's stack to the remote
    PushStack,
    /// Create new project
    NewProject,
    /// Clone a repository into the projects directory and register it as a
    /// project: opens the GitHub repo picker (palette-only)
    CloneRepository,
    /// Checkout an existing branch into a new worktree session
    CheckoutBranch,
    /// Delete/kill current session
    DeleteSession,
    /// Delete every session whose PR has merged on GitHub (palette-only)
    DeleteMergedPrSessions,
    /// Rename the currently selected session (UI title only)
    RenameSession,
    /// Restart current session (kill tmux and recreate)
    RestartSession,
    /// Reset current session: restart it *without* resuming, discarding the
    /// agent's conversation (palette-only by default)
    ResetSession,
    /// Change the program (agent) of the selected session and relaunch it
    ChangeProgram,
    /// Toggle keep-alive on the selected session (opt out of auto-hibernation)
    ToggleKeepAlive,
    /// Remove an entire project
    RemoveProject,
    /// Open worktree in editor/IDE
    OpenInEditor,
    /// Open the Info modal for the selected session
    OpenInfo,
    /// Open the selected session's PR URL in a web browser
    OpenPullRequest,
    /// Force an immediate PR-status re-check for all sessions (palette-only)
    RefreshPrStatus,
    /// Add a remote server: chained name/URL/token inputs + connection test (palette-only)
    AddRemoteServer,
    /// Remove a configured remote server via a picker (palette-only)
    RemoveRemoteServer,
    /// Open (creating if needed) the persistent commander session
    OpenCommander,
    /// Open/close the full-screen conversation overlay (TTS conversation mode)
    ToggleConversationOverlay,
    /// Toggle voice input: start/stop recording the mic for transcription (STT)
    ToggleVoiceInput,
    /// Toggle dictation: record the mic and type the transcript into the
    /// attached session pane, or the open dialog's text field (STT)
    ToggleDictation,
    /// Open the full-screen review-diff-and-comment view for the session
    OpenReviewDiff,
    /// Show help
    ShowHelp,
    /// Show settings modal
    ShowSettings,
    /// Copy the embedded server's URL and bearer token to the OS clipboard, for
    /// pairing a client. Palette-only; unavailable when nothing is being served.
    CopyServerToken,
    /// Open the settings modal on the Programs tab, targeting the currently
    /// selected backend's program list (a server header, or the selected
    /// session/project's server). Also the action bound to the server-header cog.
    EditServerPrograms,
    /// Quit application
    Quit,
    /// Cancel current operation
    Cancel,
    /// Confirm current operation
    Confirm,
    /// Text input
    TextInput(char),
    /// Backspace in text input
    Backspace,
    /// Scroll up (one row in the board column, or a line in a scrolling modal)
    ScrollUp,
    /// Scroll down (one row in the board column, or a line in a scrolling modal)
    ScrollDown,
    /// First card in the board column, or page up in a scrolling modal
    PageUp,
    /// Last card in the board column, or page down in a scrolling modal
    PageDown,
    /// Move the list / board-column cursor up a screenful
    ListPageUp,
    /// Move the list / board-column cursor down a screenful
    ListPageDown,
    /// Open quick-switch session search modal
    QuickSwitch,
    /// Generate AI summary for the current session (Info modal only)
    GenerateSummary,
    /// Scan a directory for git repos and add them as projects
    ScanDirectory,
    /// Open the "Move to section" modal for the selected session.
    MoveToSection,
    /// Cycle the view: project / sections / stacks / board.
    ToggleViewMode,
    /// Switch the list views' right pane between Preview and Shell.
    TogglePane,
    /// Switch the right pane the other way. With two tabs this is the same
    /// toggle; kept as its own command so `BackTab` stays bindable and reads
    /// symmetrically with [`Self::TogglePane`].
    TogglePaneReverse,
    /// Narrow the session list (move the pane divider left).
    ShrinkLeftPane,
    /// Widen the session list (move the pane divider right).
    GrowLeftPane,
    /// Collapse or expand the section containing the selected item.
    ToggleSection,
    /// Switch to the next workspace (wrapping).
    NextWorkspace,
    /// Switch to the previous workspace (wrapping; palette-only by default).
    PreviousWorkspace,
    /// Open the workspace picker palette.
    WorkspacePicker,
    /// Create a workspace and switch to it (palette-only by default).
    NewWorkspace,
    /// Move the selected project to another workspace (palette-only by default).
    MoveProjectToWorkspace,
}

impl UserCommand {
    /// Convert a key event to a user command using the given keybinding table.
    ///
    /// Configurable actions are resolved via `bindings`. Structural keys
    /// (Esc/Cancel, Backspace, text input) are handled as hardcoded fallbacks
    /// since they are not user-rebindable.
    pub fn from_key(key: KeyEvent, bindings: &KeyBindings) -> Option<Self> {
        // Only process key press events; ignore release/repeat from terminals
        // that support the kitty keyboard protocol
        if key.kind != KeyEventKind::Press {
            return None;
        }

        // Try the configurable bindings first
        if let Some(action) = bindings.resolve(&key) {
            return Some(action.into());
        }

        // Structural keys (not rebindable)
        match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => Some(UserCommand::Cancel),
            (KeyCode::Backspace, KeyModifiers::NONE) => Some(UserCommand::Backspace),
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                Some(UserCommand::TextInput(c))
            }
            _ => None,
        }
    }

    /// The telemetry feature name to record when this command is dispatched, or
    /// `None` to record nothing.
    ///
    /// `None` is returned for two cases: (1) high-frequency navigation/scroll and
    /// modal-mechanics commands, which would be noise; and (2) commands whose
    /// effect runs through an already-instrumented [`CommanderService`] method
    /// (e.g. `NewSession` → `session.create`), so we don't double-count. Only
    /// UI-level features that don't otherwise reach the service are named here.
    pub fn telemetry_feature(&self) -> Option<&'static str> {
        match self {
            // -- Recorded at the service layer under the same feature name; skip
            // here to avoid double-counting. (Commands like CascadeMergeMain that
            // use a *distinct* UI name from their service method stay below.)
            UserCommand::NewSession
            | UserCommand::NewStackedSession
            | UserCommand::DeleteSession
            | UserCommand::RestartSession
            | UserCommand::ResetSession
            | UserCommand::ChangeProgram
            | UserCommand::ToggleKeepAlive
            | UserCommand::NewProject
            | UserCommand::ScanDirectory
            | UserCommand::OpenReviewDiff
            | UserCommand::RenameSession
            | UserCommand::RemoveProject
            | UserCommand::CascadeResume
            | UserCommand::PushStack => None,

            // -- Navigation / scroll / modal mechanics: pure noise.
            UserCommand::NavigateUp
            | UserCommand::NavigateDown
            | UserCommand::NextGroup
            | UserCommand::PreviousGroup
            | UserCommand::NavigateFirst
            | UserCommand::NavigateLast
            | UserCommand::NavigateLeft
            | UserCommand::NavigateRight
            | UserCommand::ScrollUp
            | UserCommand::ScrollDown
            | UserCommand::PageUp
            | UserCommand::PageDown
            | UserCommand::ListPageUp
            | UserCommand::ListPageDown
            | UserCommand::ShrinkLeftPane
            | UserCommand::GrowLeftPane
            | UserCommand::Confirm
            | UserCommand::Cancel
            | UserCommand::Backspace
            | UserCommand::TextInput(_)
            | UserCommand::Quit => None,

            // -- UI-level features worth tracking.
            UserCommand::Select => Some("session.attach"),
            UserCommand::SelectShell => Some("session.open_shell"),
            UserCommand::CheckoutBranch => Some("session.checkout_branch"),
            UserCommand::DeleteMergedPrSessions => Some("session.delete_merged_prs"),
            UserCommand::CascadeMergeMain => Some("cascade.merge_main"),
            UserCommand::CascadeAbandon => Some("cascade.abandon"),
            UserCommand::OpenInEditor => Some("editor.open"),
            UserCommand::OpenInfo => Some("ui.open_info"),
            UserCommand::OpenPullRequest => Some("pr.open"),
            UserCommand::RefreshPrStatus => Some("pr.refresh_status"),
            UserCommand::AddRemoteServer => Some("server.add_remote"),
            UserCommand::RemoveRemoteServer => Some("server.remove_remote"),
            UserCommand::OpenCommander => Some("commander.open"),
            UserCommand::ToggleConversationOverlay => Some("conversation.toggle"),
            UserCommand::ToggleVoiceInput => Some("stt.toggle_voice"),
            UserCommand::ToggleDictation => Some("stt.toggle_dictation"),
            UserCommand::GenerateSummary => Some("ai_summary.generate"),
            UserCommand::ShowHelp => Some("ui.help"),
            UserCommand::ShowSettings => Some("ui.settings"),
            UserCommand::CopyServerToken => Some("server.copy_token"),
            UserCommand::EditServerPrograms => Some("ui.edit_server_programs"),
            UserCommand::QuickSwitch => Some("ui.quick_switch"),
            // The *domain* feature (`clone_project`) is recorded inside
            // `CommanderService::start_clone`, which covers every frontend.
            // This names the distinct UI event of opening the repo picker —
            // reusing `clone_project` here would double-count it.
            UserCommand::CloneRepository => Some("ui.clone_repository"),
            UserCommand::MoveToSection => Some("ui.move_to_section"),
            // The domain half is recorded by `CommanderService::set_session_base`
            // for every frontend; this names opening the picker.
            UserCommand::SetSessionBase => Some("ui.set_session_base"),
            UserCommand::ToggleViewMode => Some("ui.toggle_view_mode"),
            UserCommand::ToggleSection => Some("ui.toggle_section"),
            // Switching is client-side only (a filter), so the UI is the only
            // place it can be counted. Creating / moving is counted again by
            // the service (`workspace.update_defs` / `workspace.move_project`)
            // under a distinct name, so these name opening the flow.
            UserCommand::NextWorkspace | UserCommand::PreviousWorkspace => {
                Some("ui.switch_workspace")
            }
            UserCommand::WorkspacePicker => Some("ui.workspace_picker"),
            UserCommand::NewWorkspace => Some("ui.new_workspace"),
            UserCommand::MoveProjectToWorkspace => Some("ui.move_project_to_workspace"),
            UserCommand::TogglePane | UserCommand::TogglePaneReverse => Some("ui.toggle_pane"),
        }
    }
}

impl From<BindableAction> for UserCommand {
    fn from(action: BindableAction) -> Self {
        match action {
            BindableAction::NavigateUp => Self::NavigateUp,
            BindableAction::NavigateDown => Self::NavigateDown,
            BindableAction::NextGroup => Self::NextGroup,
            BindableAction::PreviousGroup => Self::PreviousGroup,
            BindableAction::NavigateFirst => Self::NavigateFirst,
            BindableAction::NavigateLast => Self::NavigateLast,
            BindableAction::NavigateLeft => Self::NavigateLeft,
            BindableAction::NavigateRight => Self::NavigateRight,
            BindableAction::ListPageUp => Self::ListPageUp,
            BindableAction::ListPageDown => Self::ListPageDown,
            BindableAction::Select => Self::Select,
            BindableAction::SelectShell => Self::SelectShell,
            BindableAction::NewSession => Self::NewSession,
            BindableAction::NewStackedSession => Self::NewStackedSession,
            BindableAction::CascadeMergeMain => Self::CascadeMergeMain,
            BindableAction::CascadeResume => Self::CascadeResume,
            BindableAction::CascadeAbandon => Self::CascadeAbandon,
            BindableAction::PushStack => Self::PushStack,
            BindableAction::NewProject => Self::NewProject,
            BindableAction::CloneRepository => Self::CloneRepository,
            BindableAction::CheckoutBranch => Self::CheckoutBranch,
            BindableAction::DeleteSession => Self::DeleteSession,
            BindableAction::DeleteMergedPrSessions => Self::DeleteMergedPrSessions,
            BindableAction::RenameSession => Self::RenameSession,
            BindableAction::RestartSession => Self::RestartSession,
            BindableAction::ResetSession => Self::ResetSession,
            BindableAction::ChangeProgram => Self::ChangeProgram,
            BindableAction::ToggleKeepAlive => Self::ToggleKeepAlive,
            BindableAction::RemoveProject => Self::RemoveProject,
            BindableAction::OpenInEditor => Self::OpenInEditor,
            BindableAction::OpenInfo => Self::OpenInfo,
            BindableAction::OpenPullRequest => Self::OpenPullRequest,
            BindableAction::RefreshPrStatus => Self::RefreshPrStatus,
            BindableAction::OpenCommander => Self::OpenCommander,
            BindableAction::ToggleConversationOverlay => Self::ToggleConversationOverlay,
            BindableAction::ToggleVoiceInput => Self::ToggleVoiceInput,
            BindableAction::ToggleDictation => Self::ToggleDictation,
            BindableAction::OpenReviewDiff => Self::OpenReviewDiff,
            BindableAction::ShowHelp => Self::ShowHelp,
            BindableAction::ShowSettings => Self::ShowSettings,
            BindableAction::CopyServerToken => Self::CopyServerToken,
            BindableAction::EditServerPrograms => Self::EditServerPrograms,
            BindableAction::Quit => Self::Quit,
            BindableAction::ScrollUp => Self::ScrollUp,
            BindableAction::ScrollDown => Self::ScrollDown,
            BindableAction::PageUp => Self::PageUp,
            BindableAction::PageDown => Self::PageDown,
            BindableAction::GenerateSummary => Self::GenerateSummary,
            BindableAction::ScanDirectory => Self::ScanDirectory,
            BindableAction::MoveToSection => Self::MoveToSection,
            BindableAction::SetSessionBase => Self::SetSessionBase,
            BindableAction::ToggleViewMode => Self::ToggleViewMode,
            BindableAction::ToggleSection => Self::ToggleSection,
            BindableAction::NextWorkspace => Self::NextWorkspace,
            BindableAction::PreviousWorkspace => Self::PreviousWorkspace,
            BindableAction::WorkspacePicker => Self::WorkspacePicker,
            BindableAction::NewWorkspace => Self::NewWorkspace,
            BindableAction::MoveProjectToWorkspace => Self::MoveProjectToWorkspace,
            BindableAction::TogglePane => Self::TogglePane,
            BindableAction::TogglePaneReverse => Self::TogglePaneReverse,
            BindableAction::ShrinkLeftPane => Self::ShrinkLeftPane,
            BindableAction::GrowLeftPane => Self::GrowLeftPane,
            BindableAction::AddRemoteServer => Self::AddRemoteServer,
            BindableAction::RemoveRemoteServer => Self::RemoveRemoteServer,
        }
    }
}

/// How long the input reader sleeps in `poll` between checks of its stop flag —
/// and so the usual latency of [`EventLoop::stop_input`].
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long [`EventLoop::stop_input`] waits for the reader thread to exit before
/// giving up. Well past a poll interval. Only a *wedged* thread takes longer: a
/// terminal that delivers `ESC [` in one write and the rest of the sequence
/// later makes crossterm's `poll` loop a blocking `read()` on fd 0 until the
/// sequence completes (`crossterm-0.29.0/src/event/source/unix/mio.rs:94-121` —
/// it reads until the parser yields an event, and fd 0 is never `O_NONBLOCK`).
/// Nothing can interrupt that; the next burst releases it.
const INPUT_STOP_GRACE: Duration = Duration::from_millis(500);

/// Where the input reader thread gets terminal events.
///
/// Production is crossterm's synchronous `poll`/`read` pair ([`CrosstermSource`]).
/// Tests script one, so the reader's lifecycle — the part that races the attach
/// pump for the terminal — can be pinned with no tty.
pub(crate) trait InputSource: Send + 'static {
    /// Wait up to `timeout` for an event to become readable.
    fn poll(&mut self, timeout: Duration) -> std::io::Result<bool>;
    /// Read the event `poll` reported. Must not be called unless it did.
    fn read(&mut self) -> std::io::Result<CrosstermEvent>;
    /// Throw away events already parsed but not yet read. Called once when a
    /// reader starts, so what was typed at a previous owner of the terminal
    /// does not replay into this one.
    fn discard_pending(&mut self) {}
}

/// The real terminal, via crossterm's blocking API.
///
/// Deliberately *not* `crossterm::event::EventStream`. The stream parks an OS
/// thread of its own inside `poll_internal(None)` holding crossterm's global
/// reader lock (`crossterm-0.29.0/src/event/stream.rs:44-60`), and dropping the
/// stream only *signals* that thread — nothing tells the caller when it has
/// actually let go of the terminal. `poll`/`read` take and release the lock per
/// call on a thread we own, so [`EventLoop::stop_input`] can await its exit and
/// know the terminal is free.
struct CrosstermSource;

impl InputSource for CrosstermSource {
    fn poll(&mut self, timeout: Duration) -> std::io::Result<bool> {
        crossterm::event::poll(timeout)
    }

    fn read(&mut self) -> std::io::Result<CrosstermEvent> {
        crossterm::event::read()
    }

    /// crossterm parses a whole tty burst at once but hands events out one per
    /// `read` (`event/source/unix/mio.rs:68-70`, `read.rs:100-123`), in a
    /// process-global reader (`event.rs:149`). Events left there when a reader
    /// stopped — the tail of a burst typed as an attach began, say — would
    /// otherwise be the first thing the next reader delivers, minutes later.
    /// `tcflush` on detach cannot reach them; they are already in user space.
    fn discard_pending(&mut self) {
        while matches!(crossterm::event::poll(Duration::ZERO), Ok(true)) {
            if crossterm::event::read().is_err() {
                break;
            }
        }
    }
}

/// The input reader thread, as seen from the event loop.
///
/// Held from spawn until the thread is known to have exited — including after a
/// [`EventLoop::stop_input`] that timed out, so the wedged thread it left behind
/// is still accounted for and the next reader waits for it (see
/// [`EventLoop::start_input_reader_with`]).
struct InputReader {
    /// Set to ask the thread to exit at its next poll.
    stop: Arc<AtomicBool>,
    /// Resolves (with `Err`, the sender having dropped) when the thread exits.
    done: oneshot::Receiver<()>,
}

impl InputReader {
    fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// The reader thread's body: wait out a wedged predecessor, then poll `source`
/// until `stop` is set, forwarding events on `tx`. Takes `source` by value so it
/// is dropped — the terminal released — before the caller signals completion.
fn run_input_reader(
    mut source: impl InputSource,
    predecessor: Option<oneshot::Receiver<()>>,
    stop: &AtomicBool,
    tx: &mpsc::Sender<AppEvent>,
) {
    if let Some(previous) = predecessor {
        debug!("Input reader waiting for its wedged predecessor to exit");
        let _ = previous.blocking_recv();
    }
    source.discard_pending();
    while !stop.load(Ordering::Acquire) {
        match source.poll(INPUT_POLL_INTERVAL) {
            Ok(true) => match source.read() {
                Ok(event) => {
                    // An event read after the stop was asked for was typed at
                    // whoever takes the terminal next; we can't hand it back, so
                    // drop it rather than send it to a TUI that is about to hide.
                    // (Any events behind it in crossterm's queue are cleared by
                    // the next reader's `discard_pending`.)
                    if stop.load(Ordering::Acquire) {
                        debug!("Input reader dropping an event read after stop");
                        break;
                    }
                    let Some(app_event) = input_event(event) else {
                        continue;
                    };
                    if tx.blocking_send(app_event).is_err() {
                        break;
                    }
                }
                Err(e) => debug!("Error reading terminal event: {e}"),
            },
            Ok(false) => {}
            Err(e) => {
                // No tty, most likely. Keep honouring `stop`, but don't spin on
                // the error.
                debug!("Error polling terminal events: {e}");
                std::thread::sleep(INPUT_POLL_INTERVAL);
            }
        }
    }
    debug!("Input reader thread exited");
}

/// Map a terminal event to the app event it becomes, or `None` to drop it.
fn input_event(event: CrosstermEvent) -> Option<AppEvent> {
    let input = match event {
        CrosstermEvent::Key(key) => InputEvent::Key(key),
        CrosstermEvent::Mouse(mouse) => InputEvent::Mouse(mouse),
        CrosstermEvent::Resize(w, h) => InputEvent::Resize(w, h),
        CrosstermEvent::Paste(text) => InputEvent::Paste(text),
        _ => return None,
    };
    Some(AppEvent::Input(input))
}

/// Event loop handle
pub struct EventLoop {
    /// Sender for events
    tx: mpsc::Sender<AppEvent>,
    /// Receiver for events
    rx: mpsc::Receiver<AppEvent>,
    /// The terminal reader while one exists: running, or asked to stop but not
    /// yet gone. `None` while an attach (or the editor) owns the terminal.
    input_reader: Option<InputReader>,
    /// How long [`Self::stop_input`] waits for the thread; see
    /// [`INPUT_STOP_GRACE`]. A field so tests don't pay the real grace.
    pub(crate) stop_grace: Duration,
    /// Current tick rate
    tick_rate: Option<Duration>,
}

impl EventLoop {
    /// Create a new event loop
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            tx,
            rx,
            input_reader: None,
            stop_grace: INPUT_STOP_GRACE,
            tick_rate: None,
        }
    }

    /// Get a sender for posting events
    pub fn sender(&self) -> mpsc::Sender<AppEvent> {
        self.tx.clone()
    }

    /// Start the event loop
    ///
    /// This spawns background tasks for:
    /// - Terminal input
    /// - Render ticks
    pub fn start(&mut self, tick_rate: Duration) {
        self.tick_rate = Some(tick_rate);
        self.start_input_reader();

        // Render tick task
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick_rate);

            loop {
                interval.tick().await;
                if tx.send(AppEvent::Tick).await.is_err() {
                    break;
                }
            }
        });
    }

    /// Start the terminal reader on the real terminal.
    fn start_input_reader(&mut self) {
        self.start_input_reader_with(CrosstermSource);
    }

    /// Start the terminal reader over `source`.
    ///
    /// A no-op while one is running: two readers would split every burst
    /// between them. If the previous reader was asked to stop but is still
    /// wedged in the terminal (see [`INPUT_STOP_GRACE`]), the new thread is
    /// started but does not touch the terminal until that one has exited — so
    /// the invariant holds even on the path where `stop_input` gave up.
    pub(crate) fn start_input_reader_with(&mut self, source: impl InputSource) {
        let predecessor = match self.input_reader.take() {
            Some(reader) if !reader.stop_requested() => {
                debug!("Input reader already running; not starting another");
                self.input_reader = Some(reader);
                return;
            }
            Some(wedged) => Some(wedged.done),
            None => None,
        };

        let tx = self.tx.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (done_tx, done) = oneshot::channel::<()>();
        let stop_flag = stop.clone();

        let spawned = std::thread::Builder::new()
            .name("cc-term-input".into())
            .spawn(move || {
                // `source` is moved into the loop and dropped when it returns, so
                // by the time `done_tx` drops — resolving `done` — the source has
                // let go of the terminal. A local `_done` guard would get that
                // backwards: locals drop before the closure's captures do.
                // On a panic the unwind drops `done_tx` too.
                run_input_reader(source, predecessor, &stop_flag, &tx);
                drop(done_tx);
            });

        match spawned {
            Ok(_) => self.input_reader = Some(InputReader { stop, done }),
            Err(e) => {
                // A TUI with no input reader is a TUI the operator cannot leave;
                // exiting is the lesser harm. Practically unreachable (thread
                // creation failing means the process is out of pids).
                warn!("failed to spawn the terminal input reader ({e}); quitting");
                let _ = self.tx.try_send(AppEvent::Quit);
            }
        }
    }

    /// Stop the terminal reader and wait until it has let go of the terminal.
    ///
    /// Anything that reads the terminal itself — the attach pump, the editor —
    /// must not start until this resolves: two live readers split each burst
    /// between them, and a split escape sequence is how a mouse report arrives
    /// as the keystrokes `<`, `3`, `5`, … Resolves within about a poll interval
    /// normally. A thread wedged in the terminal (see [`INPUT_STOP_GRACE`])
    /// cannot be interrupted: after the grace this warns and returns, that
    /// thread will swallow the next burst before exiting, and it stays on the
    /// books so the next reader waits for it rather than overlapping it.
    pub async fn stop_input(&mut self) {
        let Some(reader) = self.input_reader.as_mut() else {
            return;
        };
        reader.stop.store(true, Ordering::Release);
        match tokio::time::timeout(self.stop_grace, &mut reader.done).await {
            Ok(_) => {
                debug!("Input reader stopped");
                self.input_reader = None;
            }
            Err(_) => warn!(
                "input reader did not release the terminal within {:?}; it is blocked in a \
                 terminal read and will swallow the next burst",
                self.stop_grace
            ),
        }
    }

    /// Restart the terminal reader after an attach (or the editor) returns.
    pub fn restart_input(&mut self) {
        // Drain any stale events from the channel
        while self.rx.try_recv().is_ok() {}

        self.start_input_reader();
        debug!("Input reader restarted");
    }

    /// Receive the next event
    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await
    }

    /// Try to receive an event without blocking
    pub fn try_next(&mut self) -> Option<AppEvent> {
        self.rx.try_recv().ok()
    }

    /// Post a state update
    pub async fn post_update(&self, update: StateUpdate) {
        let _ = self.tx.send(AppEvent::StateUpdate(update)).await;
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KeyBindings {
        KeyBindings::default()
    }

    #[test]
    fn telemetry_feature_skips_noise_and_service_instrumented_commands() {
        // Navigation/scroll/mechanics record nothing.
        for cmd in [
            UserCommand::NavigateUp,
            UserCommand::NavigateDown,
            UserCommand::ScrollUp,
            UserCommand::PageDown,
            UserCommand::Confirm,
            UserCommand::Cancel,
            UserCommand::Backspace,
            UserCommand::TextInput('x'),
            UserCommand::Quit,
        ] {
            assert_eq!(cmd.telemetry_feature(), None, "{cmd:?} should be silent");
        }

        // Commands recorded by the service layer must not double-count here.
        for cmd in [
            UserCommand::NewSession,
            UserCommand::DeleteSession,
            UserCommand::RestartSession,
            // Reset reaches `CommanderService::restart_session_fresh`, which
            // records `session.restart_fresh` itself.
            UserCommand::ResetSession,
            UserCommand::NewProject,
            UserCommand::OpenReviewDiff,
            // These reach an already-instrumented service method under the same
            // feature name, so the TUI chokepoint must stay silent to avoid ~2x
            // inflation of these counts relative to other frontends.
            UserCommand::RenameSession,
            UserCommand::RemoveProject,
            UserCommand::CascadeResume,
            UserCommand::PushStack,
        ] {
            assert_eq!(
                cmd.telemetry_feature(),
                None,
                "{cmd:?} is service-instrumented; TUI must stay silent"
            );
        }

        // UI-level features are named.
        assert_eq!(
            UserCommand::OpenCommander.telemetry_feature(),
            Some("commander.open")
        );
        assert_eq!(
            UserCommand::OpenInfo.telemetry_feature(),
            Some("ui.open_info")
        );
        // The two voice modes are separate features on purpose: they share a
        // microphone but answer different questions (how often is the assistant
        // spoken to, how often is a pane dictated into), so folding them into
        // one name would make neither answerable.
        assert_eq!(
            UserCommand::ToggleVoiceInput.telemetry_feature(),
            Some("stt.toggle_voice")
        );
        assert_eq!(
            UserCommand::ToggleDictation.telemetry_feature(),
            Some("stt.toggle_dictation")
        );
    }

    #[test]
    fn workspace_keys_map_to_workspace_commands() {
        let w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(w, &kb()),
            Some(UserCommand::NextWorkspace)
        ));
        let shift_w = KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT);
        assert!(matches!(
            UserCommand::from_key(shift_w, &kb()),
            Some(UserCommand::WorkspacePicker)
        ));
        for (action, expected) in [
            (BindableAction::PreviousWorkspace, "PreviousWorkspace"),
            (BindableAction::NewWorkspace, "NewWorkspace"),
            (
                BindableAction::MoveProjectToWorkspace,
                "MoveProjectToWorkspace",
            ),
        ] {
            assert_eq!(format!("{:?}", UserCommand::from(action)), expected);
        }
    }

    #[test]
    fn workspace_commands_record_ui_features() {
        assert_eq!(
            UserCommand::NextWorkspace.telemetry_feature(),
            Some("ui.switch_workspace")
        );
        assert_eq!(
            UserCommand::PreviousWorkspace.telemetry_feature(),
            Some("ui.switch_workspace")
        );
        assert_eq!(
            UserCommand::WorkspacePicker.telemetry_feature(),
            Some("ui.workspace_picker")
        );
        // Distinct from the service's `workspace.*` names, so no double count.
        assert_eq!(
            UserCommand::NewWorkspace.telemetry_feature(),
            Some("ui.new_workspace")
        );
        assert_eq!(
            UserCommand::MoveProjectToWorkspace.telemetry_feature(),
            Some("ui.move_project_to_workspace")
        );
    }

    #[test]
    fn toggle_dictation_maps_from_bindable_action() {
        // Dictation reaches `handle_command` by both routes a bound action can:
        // its own key (Alt-T by default) and the palette's BindableAction
        // conversion. The conversion is the one a missing match arm would break
        // silently, since it has a catch-all-shaped `From` impl.
        assert!(matches!(
            UserCommand::from(BindableAction::ToggleDictation),
            UserCommand::ToggleDictation
        ));
    }

    #[test]
    fn test_open_info_key_maps_to_open_info() {
        let b = kb();
        let key = KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::OpenInfo)
        ));
    }

    #[test]
    fn test_key_to_command() {
        let b = kb();

        // Navigation
        let key = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::NavigateDown)
        ));

        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::NavigateUp)
        ));

        // Column navigation: h/l and ←/→ move between board columns.
        for code in [KeyCode::Char('h'), KeyCode::Left] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            assert!(
                matches!(
                    UserCommand::from_key(key, &b),
                    Some(UserCommand::NavigateLeft)
                ),
                "{code:?} should map to NavigateLeft"
            );
        }
        for code in [KeyCode::Char('l'), KeyCode::Right] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            assert!(
                matches!(
                    UserCommand::from_key(key, &b),
                    Some(UserCommand::NavigateRight)
                ),
                "{code:?} should map to NavigateRight"
            );
        }

        // Quit
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::Quit)
        ));

        // Text input — 'a' is not bound to any action, so falls through to TextInput
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::TextInput('a'))
        ));
    }

    #[test]
    fn test_pane_resize_keys() {
        let b = kb();

        // `<` / `>` are shifted characters, and terminals disagree on whether
        // they also report the SHIFT modifier — both forms must bind.
        for mods in [KeyModifiers::SHIFT, KeyModifiers::NONE] {
            assert!(
                matches!(
                    UserCommand::from_key(KeyEvent::new(KeyCode::Char('<'), mods), &b),
                    Some(UserCommand::ShrinkLeftPane)
                ),
                "`<` must shrink the left pane (mods={mods:?})"
            );
            assert!(
                matches!(
                    UserCommand::from_key(KeyEvent::new(KeyCode::Char('>'), mods), &b),
                    Some(UserCommand::GrowLeftPane)
                ),
                "`>` must grow the left pane (mods={mods:?})"
            );
        }
    }

    #[test]
    fn test_tab_toggles_the_right_pane() {
        let b = kb();
        assert!(matches!(
            UserCommand::from_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &b),
            Some(UserCommand::TogglePane)
        ));
        assert!(matches!(
            UserCommand::from_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &b),
            Some(UserCommand::TogglePaneReverse)
        ));
    }

    #[test]
    fn test_ctrl_c_quits() {
        let b = kb();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::Quit)
        ));
    }

    #[test]
    fn test_ctrl_p_navigates_up() {
        let b = kb();
        let key = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::NavigateUp)
        ));
    }

    #[test]
    fn test_ctrl_n_navigates_down() {
        let b = kb();
        let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::NavigateDown)
        ));
    }

    #[test]
    fn test_arrow_keys() {
        let b = kb();

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(up, &b),
            Some(UserCommand::NavigateUp)
        ));

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(down, &b),
            Some(UserCommand::NavigateDown)
        ));
    }

    #[test]
    fn test_enter_selects() {
        let b = kb();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::Select)
        ));
    }

    #[test]
    fn test_session_management_keys() {
        let b = kb();
        let cases: Vec<(KeyCode, KeyModifiers, UserCommand)> = vec![
            (
                KeyCode::Char('s'),
                KeyModifiers::NONE,
                UserCommand::SelectShell,
            ),
            (
                KeyCode::Char('n'),
                KeyModifiers::NONE,
                UserCommand::NewSession,
            ),
            (
                KeyCode::Char('N'),
                KeyModifiers::SHIFT,
                UserCommand::NewProject,
            ),
            (
                KeyCode::Char('d'),
                KeyModifiers::NONE,
                UserCommand::DeleteSession,
            ),
            (
                KeyCode::Char('R'),
                KeyModifiers::SHIFT,
                UserCommand::RestartSession,
            ),
            (
                KeyCode::Char('D'),
                KeyModifiers::SHIFT,
                UserCommand::RemoveProject,
            ),
            (
                KeyCode::Char('.'),
                KeyModifiers::NONE,
                UserCommand::OpenInEditor,
            ),
            (
                KeyCode::Char('.'),
                KeyModifiers::CONTROL,
                UserCommand::OpenInEditor,
            ),
            (
                KeyCode::Char('o'),
                KeyModifiers::NONE,
                UserCommand::OpenPullRequest,
            ),
            (
                KeyCode::Char('r'),
                KeyModifiers::NONE,
                UserCommand::OpenReviewDiff,
            ),
            (
                KeyCode::Char('r'),
                KeyModifiers::ALT,
                UserCommand::OpenReviewDiff,
            ),
            (
                KeyCode::Char('S'),
                KeyModifiers::SHIFT,
                UserCommand::ScanDirectory,
            ),
        ];

        for (code, modifiers, expected) in cases {
            let key = KeyEvent::new(code, modifiers);
            let result = UserCommand::from_key(key, &b);
            assert!(
                result.is_some(),
                "Expected Some for {:?}+{:?}",
                code,
                modifiers
            );
            assert_eq!(
                std::mem::discriminant(&result.unwrap()),
                std::mem::discriminant(&expected),
                "Mismatch for {:?}+{:?}",
                code,
                modifiers
            );
        }
    }

    #[test]
    fn test_scan_directory_key() {
        let b = kb();
        let key = KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::ScanDirectory)
        ));
    }

    #[test]
    fn test_scroll_keys() {
        let b = kb();

        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(matches!(
            UserCommand::from_key(ctrl_u, &b),
            Some(UserCommand::PageUp)
        ));

        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(matches!(
            UserCommand::from_key(ctrl_d, &b),
            Some(UserCommand::PageDown)
        ));

        // PgUp/PgDn page the list / board column; Ctrl-u/Ctrl-d (above) jump
        // to the column's first/last card instead.
        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(pgup, &b),
            Some(UserCommand::ListPageUp)
        ));

        let pgdown = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(pgdown, &b),
            Some(UserCommand::ListPageDown)
        ));
    }

    #[test]
    fn test_help_key() {
        let b = kb();

        let q_none = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(q_none, &b),
            Some(UserCommand::ShowHelp)
        ));

        let q_shift = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert!(matches!(
            UserCommand::from_key(q_shift, &b),
            Some(UserCommand::ShowHelp)
        ));
    }

    #[test]
    fn test_escape_cancels() {
        let b = kb();
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::Cancel)
        ));
    }

    #[test]
    fn test_backspace_key() {
        let b = kb();
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::Backspace)
        ));
    }

    #[test]
    fn test_key_release_ignored() {
        use crossterm::event::{KeyEventKind, KeyEventState};
        let b = kb();
        let key = KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::empty(),
        };
        assert!(UserCommand::from_key(key, &b).is_none());
    }

    #[test]
    fn test_key_repeat_ignored() {
        use crossterm::event::{KeyEventKind, KeyEventState};
        let b = kb();
        let key = KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::empty(),
        };
        assert!(UserCommand::from_key(key, &b).is_none());
    }

    #[test]
    fn test_refresh_pr_status_maps_from_palette_action() {
        // Palette-only command: reachable only via the BindableAction → UserCommand
        // conversion the palette uses, never from a key (it has no binding).
        assert!(matches!(
            UserCommand::from(BindableAction::RefreshPrStatus),
            UserCommand::RefreshPrStatus
        ));
    }

    #[test]
    fn test_remote_server_actions_map_from_palette_and_record_telemetry() {
        // Palette-only commands: reachable via the BindableAction conversion,
        // and both record a feature at the handle_command chokepoint.
        assert!(matches!(
            UserCommand::from(BindableAction::AddRemoteServer),
            UserCommand::AddRemoteServer
        ));
        assert!(matches!(
            UserCommand::from(BindableAction::RemoveRemoteServer),
            UserCommand::RemoveRemoteServer
        ));
        assert_eq!(
            UserCommand::AddRemoteServer.telemetry_feature(),
            Some("server.add_remote")
        );
        assert_eq!(
            UserCommand::RemoveRemoteServer.telemetry_feature(),
            Some("server.remove_remote")
        );
    }

    #[test]
    fn test_clone_repository_maps_from_palette_and_records_a_ui_feature() {
        // Palette-only command: reachable via the BindableAction conversion,
        // never from a key (it has no default binding).
        assert!(matches!(
            UserCommand::from(BindableAction::CloneRepository),
            UserCommand::CloneRepository
        ));
        // The service records the *domain* feature `clone_project` inside
        // `start_clone`; the chokepoint records opening the picker, which is a
        // distinct UI event and must not reuse that name (it would double-count
        // against the other frontends).
        let feature = UserCommand::CloneRepository.telemetry_feature();
        assert_eq!(feature, Some("ui.clone_repository"));
        assert_ne!(feature, Some("clone_project"));
    }

    #[test]
    fn test_unbound_char_falls_through_to_text_input() {
        let b = kb();
        // z is unbound by default, falls through to TextInput
        let key = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::TextInput('z'))
        ));
    }

    #[test]
    fn test_unknown_key_returns_none() {
        let b = kb();
        let key = KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE);
        assert!(UserCommand::from_key(key, &b).is_none());
    }

    #[test]
    fn test_text_input_uppercase() {
        let b = kb();
        // 'A' with SHIFT is not bound to any action, falls through to TextInput
        let key = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::TextInput('A'))
        ));
    }

    #[test]
    fn test_reset_session_action_maps_to_command() {
        // The palette dispatches by BindableAction, so an unbound action still
        // has to convert.
        assert!(matches!(
            UserCommand::from(BindableAction::ResetSession),
            UserCommand::ResetSession
        ));
    }

    #[test]
    fn test_restart_kind_wording_distinguishes_reset() {
        // A reset and a restart are different promises: one preserves the
        // conversation, one discards it. The toast must not blur them.
        assert_eq!(RestartKind::Restart.success_toast(), "Session restarted");
        assert_eq!(RestartKind::Restart.error_prefix(), "Failed to restart");
        assert!(RestartKind::Reset.success_toast().contains("reset"));
        assert_ne!(
            RestartKind::Reset.success_toast(),
            RestartKind::Restart.success_toast()
        );
        assert_eq!(RestartKind::Reset.error_prefix(), "Failed to reset");
    }
}

/// The input reader's lifecycle: the part of the event loop that shares the
/// terminal with the attach pump, pinned without a tty via a scripted
/// [`InputSource`].
#[cfg(test)]
mod input_reader_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Condvar, Mutex};

    use tokio::sync::Notify;

    /// Counts live readers so a test can assert two never overlap. `live` goes
    /// up when a thread first polls and down when its source is dropped, i.e.
    /// when the thread has exited; `peak` remembers the highest `live`, and
    /// `entered` wakes a test waiting for the thread to reach the terminal —
    /// a signal, not a sleep, so a slow scheduler can't fail the test.
    #[derive(Default)]
    struct Overlap {
        live: AtomicUsize,
        peak: AtomicUsize,
        entered: Notify,
    }

    impl Overlap {
        fn enter(&self) {
            let now = self.live.fetch_add(1, Ordering::AcqRel) + 1;
            self.peak.fetch_max(now, Ordering::AcqRel);
            self.entered.notify_one();
        }
        fn leave(&self) {
            self.live.fetch_sub(1, Ordering::AcqRel);
        }
        async fn wait_entered(&self) {
            tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
                .await
                .expect("the reader thread should reach its first poll");
        }
    }

    /// Polls report "nothing" after a short sleep; `read` is never reached.
    struct IdleSource {
        overlap: Arc<Overlap>,
        entered: bool,
    }

    impl IdleSource {
        fn new(overlap: &Arc<Overlap>) -> Self {
            Self {
                overlap: overlap.clone(),
                entered: false,
            }
        }
    }

    impl InputSource for IdleSource {
        fn poll(&mut self, timeout: Duration) -> std::io::Result<bool> {
            if !self.entered {
                self.entered = true;
                self.overlap.enter();
            }
            std::thread::sleep(timeout / 5);
            Ok(false)
        }
        fn read(&mut self) -> std::io::Result<CrosstermEvent> {
            unreachable!("poll never reports an event")
        }
    }

    impl Drop for IdleSource {
        fn drop(&mut self) {
            if self.entered {
                self.overlap.leave();
            }
        }
    }

    /// A gate a thread blocks on until the test opens it.
    #[derive(Default)]
    struct Gate(Mutex<bool>, Condvar);

    impl Gate {
        fn wait(&self) {
            let mut open = self.0.lock().unwrap();
            while !*open {
                open = self.1.wait(open).unwrap();
            }
        }
        fn open(&self) {
            *self.0.lock().unwrap() = true;
            self.1.notify_all();
        }
    }

    /// Blocks inside `poll` until released — the production wedge: crossterm's
    /// `poll` loops a blocking `read()` on fd 0 after a partial escape sequence
    /// until the rest arrives. Counts as a live reader like `IdleSource`.
    struct WedgedSource {
        gate: Arc<Gate>,
        overlap: Arc<Overlap>,
        entered: bool,
    }

    impl InputSource for WedgedSource {
        fn poll(&mut self, _timeout: Duration) -> std::io::Result<bool> {
            if !self.entered {
                self.entered = true;
                self.overlap.enter();
            }
            self.gate.wait();
            Ok(false)
        }
        fn read(&mut self) -> std::io::Result<CrosstermEvent> {
            unreachable!("poll never reports an event")
        }
    }

    impl Drop for WedgedSource {
        fn drop(&mut self) {
            if self.entered {
                self.overlap.leave();
            }
        }
    }

    fn key(c: char) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    /// Hands out scripted events one per poll, then idles. `pending` is what a
    /// previous owner left parsed-but-unread; a reader must discard it first.
    struct ScriptedSource {
        pending: std::collections::VecDeque<CrosstermEvent>,
        events: std::collections::VecDeque<CrosstermEvent>,
    }

    impl InputSource for ScriptedSource {
        fn poll(&mut self, timeout: Duration) -> std::io::Result<bool> {
            if self.pending.is_empty() && self.events.is_empty() {
                std::thread::sleep(timeout / 5);
                return Ok(false);
            }
            Ok(true)
        }
        fn read(&mut self) -> std::io::Result<CrosstermEvent> {
            Ok(self
                .pending
                .pop_front()
                .or_else(|| self.events.pop_front())
                .expect("poll reported an event"))
        }
        fn discard_pending(&mut self) {
            self.pending.clear();
        }
    }

    async fn next_key(ev: &mut EventLoop) -> KeyCode {
        match tokio::time::timeout(Duration::from_secs(2), ev.next()).await {
            Ok(Some(AppEvent::Input(InputEvent::Key(k)))) => k.code,
            other => panic!("expected a key event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stop_input_resolves_only_after_the_reader_has_left_the_terminal() {
        let overlap = Arc::new(Overlap::default());
        let mut ev = EventLoop::new();
        ev.start_input_reader_with(IdleSource::new(&overlap));
        overlap.wait_entered().await;
        assert_eq!(overlap.live.load(Ordering::Acquire), 1);

        ev.stop_input().await;

        // No sleep: the guarantee is that *resolution* means the thread is gone.
        assert_eq!(
            overlap.live.load(Ordering::Acquire),
            0,
            "stop_input returned while the reader thread still held the terminal"
        );
    }

    /// The regression: stopping used to only *signal* the reader, and callers
    /// slept 100ms hoping it had noticed. Restarting straight after an awaited
    /// stop must never put two readers on the terminal at once.
    #[tokio::test]
    async fn readers_never_overlap_across_repeated_stop_and_restart() {
        let overlap = Arc::new(Overlap::default());
        let mut ev = EventLoop::new();

        for _ in 0..5 {
            ev.start_input_reader_with(IdleSource::new(&overlap));
            overlap.wait_entered().await;
            ev.stop_input().await;
        }

        assert_eq!(overlap.peak.load(Ordering::Acquire), 1);
        assert_eq!(overlap.live.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn a_second_start_while_one_is_running_is_ignored() {
        let overlap = Arc::new(Overlap::default());
        let mut ev = EventLoop::new();
        ev.start_input_reader_with(IdleSource::new(&overlap));
        overlap.wait_entered().await;
        ev.start_input_reader_with(IdleSource::new(&overlap));

        ev.stop_input().await;
        assert_eq!(overlap.peak.load(Ordering::Acquire), 1);
        assert_eq!(overlap.live.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn events_read_before_stop_are_delivered() {
        let mut ev = EventLoop::new();
        ev.start_input_reader_with(ScriptedSource {
            pending: Default::default(),
            events: [key('j'), key('k')].into(),
        });

        assert_eq!(next_key(&mut ev).await, KeyCode::Char('j'));
        assert_eq!(next_key(&mut ev).await, KeyCode::Char('k'));
        ev.stop_input().await;
    }

    /// What a previous owner of the terminal left parsed in the source must not
    /// be the first thing a new reader delivers.
    #[tokio::test]
    async fn a_new_reader_discards_events_left_by_the_previous_owner() {
        let mut ev = EventLoop::new();
        ev.start_input_reader_with(ScriptedSource {
            pending: [key('q'), key('q')].into(),
            events: [key('j')].into(),
        });

        assert_eq!(next_key(&mut ev).await, KeyCode::Char('j'));
        ev.stop_input().await;
    }

    /// A thread wedged in the terminal can't be interrupted; `stop_input` must
    /// bound its wait rather than hang the frontend behind it — and must keep
    /// the wedged reader on the books rather than forget it.
    #[tokio::test]
    async fn stop_input_gives_up_on_a_wedged_reader_but_keeps_track_of_it() {
        let gate = Arc::new(Gate::default());
        let overlap = Arc::new(Overlap::default());
        let mut ev = EventLoop::new();
        ev.stop_grace = Duration::from_millis(50);
        ev.start_input_reader_with(WedgedSource {
            gate: gate.clone(),
            overlap: overlap.clone(),
            entered: false,
        });
        overlap.wait_entered().await;

        let started = Instant::now();
        ev.stop_input().await;
        assert!(
            started.elapsed() >= ev.stop_grace,
            "expected to wait out the grace period"
        );
        assert_eq!(overlap.live.load(Ordering::Acquire), 1, "still wedged");
        assert!(
            ev.input_reader.as_ref().is_some_and(|r| r.stop_requested()),
            "the wedged reader must stay accounted for"
        );

        gate.open();
        // A second stop now observes the exit and forgets the reader. It is
        // expected to complete, so give it the real grace: the shortened one
        // exists to time out the wedged stop, and a loaded runner can take
        // longer than 50ms to schedule the reader's exit.
        ev.stop_grace = INPUT_STOP_GRACE;
        ev.stop_input().await;
        assert_eq!(overlap.live.load(Ordering::Acquire), 0);
        assert!(ev.input_reader.is_none());
    }

    /// The path `stop_input`'s timeout leaves behind: a restart after a wedged
    /// stop must not put a second reader on the terminal while the first is
    /// still there. The new thread starts, but waits for the old one to exit.
    #[tokio::test]
    async fn a_reader_started_after_a_wedged_stop_waits_for_the_wedged_one() {
        let gate = Arc::new(Gate::default());
        let overlap = Arc::new(Overlap::default());
        let mut ev = EventLoop::new();
        ev.stop_grace = Duration::from_millis(50);
        ev.start_input_reader_with(WedgedSource {
            gate: gate.clone(),
            overlap: overlap.clone(),
            entered: false,
        });
        overlap.wait_entered().await;
        ev.stop_input().await; // times out; the wedge holds

        let idle = Arc::new(Overlap::default());
        ev.start_input_reader_with(IdleSource::new(&idle));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            idle.live.load(Ordering::Acquire),
            0,
            "the new reader must not touch the terminal while the wedged one holds it"
        );

        gate.open();
        idle.wait_entered().await;
        assert_eq!(overlap.live.load(Ordering::Acquire), 0, "wedged one gone");
        assert_eq!(idle.live.load(Ordering::Acquire), 1);

        // A normal stop, so the real grace (see the test above).
        ev.stop_grace = INPUT_STOP_GRACE;
        ev.stop_input().await;
        assert_eq!(idle.live.load(Ordering::Acquire), 0);
    }

    #[test]
    fn focus_and_unknown_events_are_dropped() {
        assert!(input_event(CrosstermEvent::FocusGained).is_none());
        assert!(input_event(CrosstermEvent::FocusLost).is_none());
        assert!(matches!(
            input_event(CrosstermEvent::Resize(80, 24)),
            Some(AppEvent::Input(InputEvent::Resize(80, 24)))
        ));
    }
}
