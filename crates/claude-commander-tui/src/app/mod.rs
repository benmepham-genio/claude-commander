//! Main TUI application
//!
//! Event-driven application that coordinates:
//! - Terminal rendering with ratatui
//! - User input handling
//! - Background state updates

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};
use tracing::{debug, error, info, warn};
use tui_input::Input;

use super::event::{AppEvent, EventLoop, InputEvent, RestartKind, StateUpdate, UserCommand};
use super::path_completer::PathCompleter;
use super::theme::Theme;
use super::widgets::board::{
    BoardButtonRegion, BoardHitRegion, BoardRects, BoardState, BoardWidget,
};
use super::widgets::{
    InfoContent, InfoProjectData, InfoSessionData, InfoView, Preview, PreviewState, TreeList,
    TreeListState, status_glyph,
};
use crate::error::{Result, TuiError};
use claude_commander_core::api::{CommanderService, DiffSide};
use claude_commander_core::backend::{
    AttachConnection, AttachKind, BResult, BackendCapabilities, BackendError, BackendHandle,
    BackendId, BackendView, CommanderBackend, ConnectionState, LOCAL_BACKEND_ID, LocalBackend,
    PlaceholderBackend, RemoteBackendFactory, SessionRef,
};
use claude_commander_core::config::ViewMode;
use claude_commander_core::config::{
    BindableAction, Config, ConfigStore, RemoteServerConfig, StateStore,
};
use claude_commander_core::git::{
    AiSummary, BlockReason, DiffInfo, EnrichedPrInfo, diff_hash, fetch_branch_summary,
    fetch_enriched_pr, is_gh_available,
};
use claude_commander_core::session::{
    AgentState, Board, BoardPos, ProjectId, SessionId, SessionListItem, SessionStatus,
};
use claude_commander_core::tmux::PaneInfo;

mod actions;
mod background;
mod colour_picker;
mod conversation;
mod event_loop;
mod input;
mod modals;
mod render;
mod review;
mod selection;
mod settings;
mod state;
mod switcher;
mod workspace_settings;
mod workspaces;

#[cfg(test)]
mod tests;

/// Modal geometry, re-exported for the crate's render snapshots so they frame
/// widgets with the same rects the app ships rather than a copy that can drift.
/// Test-only, so a normal build doesn't carry an unused re-export.
#[cfg(test)]
pub(crate) use modals::{centered_rect, confirm_modal_area};
#[cfg(test)]
pub(crate) use render::pane_tabs;
pub use review::DiffReviewState;
pub(crate) use review::ImageEntry;
pub use review::ReviewPrepared;

/// Direction for mouse scroll events
enum ScrollDirection {
    Up,
    Down,
}

/// iTerm2 advertises Kitty-graphics support (since 3.5) so the stdio probe can
/// land on `Kitty`, but iTerm2 renders that protocol unreliably — the escapes
/// are emitted and nothing is drawn. Its native inline-image protocol (OSC
/// 1337) is solid, so when the probe chose Kitty *and* the terminal identifies
/// as iTerm2, fall back to iTerm2. Returns the protocol to force, or `None` to
/// keep `detected`. Pure (env passed in) so it is unit-testable without a tty.
fn iterm2_kitty_override(
    detected: ratatui_image::picker::ProtocolType,
    term_program: Option<&str>,
    lc_terminal: Option<&str>,
) -> Option<ratatui_image::picker::ProtocolType> {
    use ratatui_image::picker::ProtocolType;
    if detected != ProtocolType::Kitty {
        return None;
    }
    let is_iterm2 = term_program.is_some_and(|t| t.contains("iTerm"))
        || lc_terminal.is_some_and(|t| t.contains("iTerm"));
    is_iterm2.then_some(ProtocolType::Iterm2)
}

/// Whether an ended attached session should be auto-restarted fresh.
///
/// Shell sessions (suffix `-sh`) and the project-less commander are never
/// auto-restarted: the commander is absent from `state.sessions`, so a
/// restart-by-name would fail — it is revived lazily by
/// [`commander::ensure_session`](claude_commander_core::commander::ensure_session) on next
/// open. A crash-loop guard stops after 3 consecutive ends.
fn should_auto_restart_ended(session_name: &str, consecutive_ends: u8) -> bool {
    !session_name.ends_with("-sh")
        && session_name != claude_commander_core::commander::COMMANDER_TMUX_NAME
        && consecutive_ends < 3
}

/// Fetch a backend's workspace snapshot + agent states and forward them as a
/// [`StateUpdate::BackendChanged`] over `tx`. Shared by the per-backend
/// change-feed task and the post-mutation forced refresh
/// ([`App::spawn_backend_view_refresh`]) so both deliver identical updates off
/// the event loop. A failed snapshot fetch is skipped (the cached view keeps its
/// last-good contents) without killing a long-lived feed loop; a failed
/// agent-states fetch falls back to empty. Returns `false` only when the event
/// channel has closed, so a feed loop knows to stop.
async fn fetch_and_send_backend_change(
    backend_id: usize,
    backend: Arc<dyn CommanderBackend>,
    tx: tokio::sync::mpsc::Sender<AppEvent>,
) -> bool {
    let snapshot = match backend.snapshot().await {
        Ok(s) => s,
        Err(e) => {
            debug!("Snapshot refresh for backend {backend_id} failed: {e}");
            return true;
        }
    };
    let states = backend.agent_states(false).await.unwrap_or_else(|_| {
        claude_commander_core::api::AgentStatesSnapshot {
            states: Default::default(),
            commander_running: false,
        }
    });
    tx.send(AppEvent::StateUpdate(StateUpdate::BackendChanged {
        backend_id,
        snapshot: Box::new(snapshot),
        states: Box::new(states),
    }))
    .await
    .is_ok()
}

/// Derive a backend's connection health from a freshly-folded workspace
/// snapshot, or `None` when the snapshot must not touch it.
///
/// Only the **local** backend's connection is snapshot-derived: `server.tmux_ok
/// == false` means local sessions can't be driven, so its header shows
/// `Degraded` and stays there until tmux recovers (a snapshot still fetches fine
/// without tmux, reporting `tmux_ok=false`, so folding one must not silently
/// flip the header back to `Connected` and un-gate local commands). A **remote**
/// backend's health is owned exclusively by its connection-watch task
/// ([`StateUpdate::BackendConnection`]), so a snapshot fold returns `None` and
/// leaves `view.connection` untouched — otherwise a slow in-flight fetch
/// completing after the poller went `Degraded` would pin the header `Connected`
/// for the rest of the outage.
fn connection_from_snapshot(
    is_local: bool,
    snapshot: &claude_commander_core::api::Snapshot,
) -> Option<ConnectionState> {
    if !is_local {
        return None;
    }
    Some(if snapshot.server.tmux_ok {
        ConnectionState::Connected
    } else {
        ConnectionState::Degraded {
            reason: "tmux unavailable".to_string(),
        }
    })
}

/// The status-toast text to surface when the attach loop ends on `result`, or
/// `None` for a clean end (user detached / session ended, handled elsewhere)
/// that needs no toast. A mid-attach transport error must not silently drop the
/// operator back to the tree — the remote attach path promises a toast — so it
/// maps to a message here. Pure so the mapping is unit-testable without the
/// (headless-untestable) attach loop.
pub(super) fn attach_end_toast(
    result: &claude_commander_core::tmux::AttachResult,
) -> Option<String> {
    match result {
        claude_commander_core::tmux::AttachResult::Error(e) => Some(format!("Attach failed: {e}")),
        _ => None,
    }
}

/// What startup should do given the local tmux probe result and whether any
/// remote servers are configured.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TmuxStartup {
    /// tmux is available — start normally.
    Proceed,
    /// tmux is unavailable but remotes are configured — run remote-only,
    /// marking the local backend Degraded with this reason.
    DegradeLocal(String),
    /// tmux is unavailable and no remotes are configured — nothing to drive.
    Abort(String),
}

/// Decide how [`App::run`] reacts to the local tmux probe. `tmux_err` is the
/// probe's error message (`None` when tmux is present). Kept as a pure fn so the
/// branch logic is unit-testable without spawning the (headless-untestable) TUI.
pub(super) fn tmux_startup_decision(tmux_err: Option<String>, has_remotes: bool) -> TmuxStartup {
    match (tmux_err, has_remotes) {
        (None, _) => TmuxStartup::Proceed,
        (Some(reason), true) => TmuxStartup::DegradeLocal(reason),
        (Some(reason), false) => TmuxStartup::Abort(reason),
    }
}

/// What the TUI attaches to once it tears down for an attach. Set by the
/// select / shell / commander handlers and consumed by the attach loop in
/// [`App::run`], which drives it over [`claude_commander_core::tmux::run_attach`].
#[derive(Clone, Debug)]
pub enum AttachTarget {
    /// A session's pane, attached through the owning backend. `Ctrl+\` toggles
    /// between [`AttachKind::Agent`] and [`AttachKind::Shell`] on the same id.
    Session {
        session: SessionRef,
        kind: AttachKind,
    },
    /// A name-only tmux session with no `SessionId` — the commander session or
    /// a project shell — attached via the local backend by name. Local-only:
    /// these are absent from the workspace snapshot.
    LocalName(String),
}

/// Minimum left pane width as a percentage of the content area
pub(crate) const MIN_LEFT_PANE_PCT: u16 = 15;
/// Maximum left pane width as a percentage of the content area
pub(crate) const MAX_LEFT_PANE_PCT: u16 = 60;
/// Default left pane width as a percentage of the content area
pub(crate) const DEFAULT_LEFT_PANE_PCT: u16 = 30;

/// Which view the right-hand pane shows in the *list* view modes.
///
/// The board is a full-screen takeover with no right pane, so this is only
/// consulted when [`ViewMode::is_board`] is false. `Info` renders the same
/// content as the `i` Info modal — which remains the only way to reach it from
/// the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RightPaneView {
    /// Live capture of the session's agent pane.
    #[default]
    Preview,
    /// Session metadata, diffstat, PR details and AI summary.
    Info,
    /// Live capture of the session's (or project's) shell pane.
    Shell,
}

impl RightPaneView {
    /// Advance the tab cycle. A project has no agent pane, so its cycle is the
    /// two-tab `Shell ↔ Info`; a session cycles all three.
    pub fn cycled(self, on_project: bool, forward: bool) -> Self {
        if on_project {
            return match self.effective(true) {
                Self::Info => Self::Shell,
                _ => Self::Info,
            };
        }
        match (self, forward) {
            (Self::Preview, true) => Self::Info,
            (Self::Info, true) => Self::Shell,
            (Self::Shell, true) => Self::Preview,
            (Self::Preview, false) => Self::Shell,
            (Self::Info, false) => Self::Preview,
            (Self::Shell, false) => Self::Info,
        }
    }

    /// Tab labels and the active index, for the pane header.
    ///
    /// Returned as a slice + index so the header renderer stays ignorant of
    /// which selection kind produced it.
    pub fn tabs(self, on_project: bool) -> (&'static [&'static str], usize) {
        if on_project {
            (
                &["Shell", "Info"],
                match self.effective(true) {
                    Self::Info => 1,
                    _ => 0,
                },
            )
        } else {
            (
                &["Preview", "Info", "Shell"],
                match self {
                    Self::Preview => 0,
                    Self::Info => 1,
                    Self::Shell => 2,
                },
            )
        }
    }

    /// The view actually rendered for the current selection: a project has no
    /// agent pane, so `Preview` collapses to `Shell` there.
    pub fn effective(self, on_project: bool) -> Self {
        match (self, on_project) {
            (Self::Preview, true) => Self::Shell,
            _ => self,
        }
    }
}

/// A single entry in the pre-computed stack chain for the Info modal.
#[derive(Debug, Clone)]
pub struct StackChainEntry {
    pub title: String,
    pub status: SessionStatus,
    pub is_current: bool,
}

/// Modal dialog state
/// Caret glyph shown at the cursor position in single-line text inputs,
/// matching the review comment box.
pub(super) const INPUT_CARET: char = '▏';

/// Render a [`tui_input::Input`]'s value with the caret glyph spliced in at the
/// cursor (appended when the cursor is at the end). Single-line inputs across
/// the app render through this so the caret tracks the cursor instead of
/// sitting at the end of the text.
pub(super) fn input_with_caret(input: &Input) -> String {
    let chars: Vec<char> = input.value().chars().collect();
    let mut out = String::with_capacity(input.value().len() + INPUT_CARET.len_utf8());
    for (i, ch) in chars.iter().enumerate() {
        if i == input.cursor() {
            out.push(INPUT_CARET);
        }
        out.push(*ch);
    }
    if input.cursor() >= chars.len() {
        out.push(INPUT_CARET);
    }
    out
}

/// Forward an editing key to a single-line `tui-input` field, returning `true`
/// when the text value changed (so callers can recompute dependent state such
/// as fuzzy filters or path completions). Cursor-only moves and keys `tui-input`
/// doesn't recognise (Enter, Esc, Tab, Up/Down, …) return `false` and are left
/// for the caller to handle.
pub(super) fn edit_text_input(input: &mut Input, key: crossterm::event::KeyEvent) -> bool {
    use tui_input::backend::crossterm::to_input_request;
    match to_input_request(&crossterm::event::Event::Key(key)) {
        Some(req) => input.handle(req).is_some_and(|c| c.value),
        None => false,
    }
}

/// Insert a string at the cursor of a `tui-input` field (it has no bulk insert).
pub(super) fn insert_into_input(input: &mut Input, s: &str) {
    for c in s.chars() {
        input.handle(tui_input::InputRequest::InsertChar(c));
    }
}

#[derive(Debug, Clone)]
pub enum Modal {
    /// No modal open
    None,
    /// Text input modal
    Input {
        title: String,
        prompt: String,
        value: Input,
        on_submit: InputAction,
        /// When `Some`, the dialog renders a dynamic hint indicating whether
        /// the sanitized session name corresponds to an already-existing
        /// branch (local or remote). Populated by `handle_new_session` /
        /// `handle_new_stacked_session`; other Input flows (Rename) leave
        /// this as `None`.
        ///
        /// Entries are the local-name form returned by `load_branch_entries`
        /// (i.e. remote-only `origin/<x>` is stored as just `<x>` to match
        /// `finalize_session`'s resolution logic).
        existing_branches: Option<Vec<String>>,
        /// When `Some`, the dialog renders a filterable project picker beneath
        /// the name field; the highlighted project becomes the session's target.
        /// Only the plain New Session flow populates this — stacked sessions are
        /// locked to the parent's project, and non-creating flows leave it `None`.
        project_picker: Option<ProjectPicker>,
        /// When `Some`, the dialog renders a program picker beneath the name
        /// field and the chosen entry's command launches the session. `None`
        /// for Input flows that don't create sessions (Rename, AddProject…).
        program_picker: Option<ProgramPicker>,
        /// When `Some`, the dialog renders a server picker; the highlighted
        /// backend is where the session is created (re-scoping the project and
        /// program/section options). Only the New Session flow populates it, and
        /// only when more than one backend is configured.
        server_picker: Option<ServerPicker>,
        /// When `Some` (and it holds more than just the catch-all), the dialog
        /// renders a section picker; the highlighted row becomes the new
        /// session's section (catch-all row = no override / auto-assign).
        section_picker: Option<SectionPicker>,
        /// Which field currently has focus. Tab cycles through the fields that
        /// are present (see `InputFocus::next`).
        focus: InputFocus,
        /// Whether the focused picker row's dropdown is expanded.
        /// `expanded && focus == Project` means the project dropdown is open
        /// (and likewise for the program picker). Focus cannot move while a
        /// dropdown is open (Tab/arrows are captured by the dropdown), so the
        /// flag stays consistent with `focus` without needing an explicit reset.
        expanded: bool,
        /// Render the value as bullets instead of plaintext (secret entry,
        /// e.g. a remote server's bearer token). Editing behaves normally;
        /// only the display is masked.
        mask: bool,
    },
    /// Confirmation modal
    Confirm {
        title: String,
        message: String,
        on_confirm: ConfirmAction,
    },
    /// Path input modal with a live-filtered subdirectory list.
    ///
    /// The list is populated on open and re-filtered on every keystroke.
    /// Arrow keys move `completer.selected_idx`; `scroll` keeps the
    /// highlighted row inside the visible window (same pattern as
    /// `Modal::QuickSwitch`).
    PathInput {
        title: String,
        prompt: String,
        value: Input,
        on_submit: InputAction,
        completer: PathCompleter,
        /// First visible row of the completions list.
        scroll: usize,
    },
    /// Loading spinner modal (non-interactive). `hint`, when set, renders as a
    /// dimmed line beneath the spinner (e.g. how to turn the operation off).
    Loading {
        title: String,
        message: String,
        hint: Option<String>,
    },
    /// Help modal. `scroll` is the first visible line of `build_help_lines`.
    /// Clamped against the rendered content height in `render_help_modal`.
    Help { scroll: u16 },
    /// Error modal
    Error { message: String },
    /// Settings modal
    Settings(SettingsState),
    /// Quick-switch palette modal — searches sessions and/or commands.
    QuickSwitch {
        /// Entry mode. `Unified` mixes sessions and commands; `CommandOnly`
        /// was opened via Shift+leader and only shows commands regardless of
        /// query. A leading `>` in a `Unified` query *effectively* filters
        /// to commands without changing this field, so backspacing past the
        /// `>` naturally restores the unified view.
        mode: PaletteMode,
        query: Input,
        matches: Vec<QuickSwitchItem>,
        selected_idx: usize,
        /// Index of the first visible row — keeps `selected_idx` inside
        /// the visible window when the list is longer than can fit.
        scroll: usize,
        /// The review view this palette was opened *over* (`SessionOnly` mode,
        /// from `handle_review_key`), suspended rather than closed: Esc puts it
        /// back exactly as it was, and it keeps rendering underneath meanwhile.
        ///
        /// It rides inside the modal rather than sitting in [`AppUiState`] so
        /// its lifetime is the palette's by construction — any other path that
        /// replaces the modal (activating a row, a state update swapping it)
        /// drops the review with it, and no invariant has to be maintained to
        /// stop a suspended view outliving the palette that suspended it.
        review: Option<Box<DiffReviewState>>,
    },
    /// Checkout-existing-branch modal. Shows an input field plus a
    /// filterable/scrollable list of branches (local + remote) and
    /// creates a worktree session from the selected branch on submit.
    CheckoutBranch {
        /// Project the session will belong to
        project_id: ProjectId,
        /// Current input text (filter + paste target)
        query: Input,
        /// All branches loaded from the repo (source for filtering)
        all_branches: Vec<BranchEntry>,
        /// Filtered view of branches matching `query`
        filtered: Vec<BranchEntry>,
        /// Index into `filtered` of the currently highlighted branch
        selected_idx: usize,
        /// Scroll offset into `filtered` (first visible row)
        scroll: usize,
        /// True while `git fetch origin` is running in the background
        fetching: bool,
    },
    /// Full-screen review-diff-and-comment view.
    ReviewDiff(Box<DiffReviewState>),
    /// Full-screen conversation overlay (view onto the headless `claude`
    /// session). View-only state; the session itself lives on `App`, so closing
    /// this leaves the conversation running.
    Conversation { input: Input, scroll: u16 },
    /// Session Info overlay (metadata, diffstat, PR details, AI summary) for the
    /// selected session. `scroll` is the first visible line of the composed
    /// `InfoView` content, clamped against its height each frame (like `Help`).
    Info { scroll: u16 },
}

/// A session match in the quick-switch modal
#[derive(Debug, Clone)]
pub struct QuickSwitchMatch {
    pub session_id: SessionId,
    pub title: String,
    pub branch: String,
    pub project_name: String,
    pub status: SessionStatus,
    /// Live agent sub-state, from the owning backend's poller. Carried so the
    /// palette row can draw the *same* glyph the tree draws for the session
    /// (`session_status_glyph`) — `status` alone renders every running session
    /// as an idle `●`, whatever its agent is doing.
    pub agent_state: Option<AgentState>,
    /// Whether the session has unread output, for the same reason.
    pub unread: bool,
    /// Most-recent attach time, used to rank the palette by recency when the
    /// query is empty (newest first, mirroring the pinned "Recent" block).
    /// `None` for sessions never attached, which sort to the bottom.
    pub last_attached_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The session's workspace (its project's tag; `None` = Main). Picking a
    /// session outside the active workspace switches to this one first.
    pub workspace: Option<String>,
    /// The workspace's label when it is *not* the active one (and workspace UI
    /// is showing), drawn as a dim tag; `None` for active-workspace rows, which
    /// also rank first.
    pub other_workspace: Option<String>,
}

/// How the quick-switch palette was opened.
///
/// `Unified` is the default (plain leader key) — sessions and commands are
/// both shown. `CommandOnly` is entered via Shift+leader and restricts the
/// list to commands regardless of query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    Unified,
    CommandOnly,
    /// Sessions only, no command rows — the switcher opened from inside the
    /// full-screen review view (`Ctrl+Space`, or the leader key when the review
    /// doesn't bind it). Commands are left out because every one of them would
    /// have to tear the review down to run; a `>` prefix does *not* upgrade this
    /// mode to `CommandOnly` for the same reason.
    SessionOnly,
    /// Section picker for a specific session. The palette is populated with
    /// one entry per configured `[[sections]]` plus an "Auto" entry; selecting
    /// an entry sets (or clears) the session's `section_override`.
    SectionPicker {
        session_id: SessionId,
    },
    /// Remote-server picker for the remove-server flow: one entry per
    /// configured `[[remote_servers]]`; selecting one opens a confirm modal.
    RemoteServerPicker,
    /// GitHub repo picker for the clone flow. Rows come from
    /// [`AppUiState::repo_picker`] (fetched asynchronously); selecting one opens
    /// the destination-name prompt. Unlike the other picker modes this one also
    /// acts on an *unmatched* query, treating it as a clone URL — that is the
    /// "clone something not in the list" path.
    GithubRepoPicker,
    /// Stack-base picker for a specific session: one row per eligible session
    /// in the same project, plus a row for the project's main branch.
    /// Selecting a row opens a confirm modal that retargets the session's base.
    BasePicker {
        session_id: SessionId,
    },
    /// Program (agent) picker for a specific session. The palette lists the
    /// owning backend's configured programs; selecting one opens a confirm modal
    /// that changes the session's program and relaunches it.
    ProgramPicker {
        session_id: SessionId,
    },
    /// Workspace picker (`W`): one row per merged workspace; selecting one
    /// switches to it. An unmatched query creates a workspace of that name.
    WorkspacePicker,
    /// Target picker for moving `project_id` to another workspace. An
    /// unmatched query creates a workspace of that name and moves it there.
    MoveProjectPicker {
        project_id: ProjectId,
    },
}

/// A row in the quick-switch palette — either an open session, a
/// keybound command, or a section-move target.
#[derive(Debug, Clone)]
pub enum QuickSwitchItem {
    Session(QuickSwitchMatch),
    Command(CommandEntry),
    /// Selecting this row pins `session_id` to `target` (Some = section name,
    /// None = "Auto" / clear override).
    SectionMove {
        session_id: SessionId,
        target: Option<String>,
        /// Pre-formatted display label.
        label: String,
    },
    /// Selecting this row opens the remove-confirmation for the named remote
    /// server (remove-server palette mode).
    RemoteServerRemove {
        name: String,
        /// Pre-formatted display label (`name (url)`).
        label: String,
    },
    /// Selecting this row opens the destination-name prompt for cloning
    /// `full_name` (GitHub repo-picker palette mode).
    GithubRepo {
        /// `owner/name`, the form `gh repo clone` takes.
        full_name: String,
        /// The repo's own directory name, the destination-name default.
        dir_name: String,
        /// Pre-formatted display label (slug, visibility, and an "already a
        /// project" note when this repo is registered on the target backend).
        label: String,
    },
    /// Selecting this row opens a confirm modal retargeting `session_id`'s stack
    /// base onto `target` (`None` = the project's main branch).
    BaseChange {
        session_id: SessionId,
        target: Option<SessionId>,
        /// The branch the session would end up based on, for the confirm text.
        base_branch: String,
        /// Pre-formatted display label.
        label: String,
    },
    /// Selecting this row opens a confirm modal to change `session_id`'s program
    /// to `program` and relaunch it (program-picker palette mode).
    ProgramChange {
        session_id: SessionId,
        /// The launch command to switch to.
        program: String,
        /// Pre-formatted display label.
        label: String,
    },
    /// Selecting this row switches to workspace `name` (`None` = Main).
    Workspace {
        name: Option<String>,
        /// Pre-formatted display label (with waiting count / current marker).
        label: String,
    },
    /// Selecting this row moves `project_id` into `target` (`None` = Main).
    ProjectWorkspace {
        project_id: ProjectId,
        target: Option<String>,
        /// Pre-formatted display label.
        label: String,
    },
}

/// A command row in the quick-switch palette.
#[derive(Debug, Clone)]
pub struct CommandEntry {
    /// The action to dispatch when the user presses Enter on this row.
    pub action: BindableAction,
    /// Human-readable label (from `BindableAction::description`).
    pub label: &'static str,
    /// Pre-formatted key-binding string (from `KeyBindings::keys_display`).
    /// Empty when the action has no binding — the palette intentionally
    /// still lists these so it can function as the primary command surface
    /// as hotkeys are trimmed over time.
    pub keys: String,
}

/// Lifecycle of the clone picker's repo listing.
///
/// A distinct `Failed` state rather than an empty list because the two mean
/// different things and the picker renders them differently: a host without
/// `gh` (or an unauthenticated one) fails with
/// [`BackendError::Unavailable`](claude_commander_core::backend::BackendError), while an
/// authenticated account with no repos genuinely answers an empty list. Either
/// way the picker's URL path stays usable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RepoFetch {
    /// A listing is in flight (`gh api --paginate` can take many seconds).
    #[default]
    Loading,
    /// The listing arrived; `repos` is authoritative (possibly empty).
    Ready,
    /// The listing failed; the message is the backend's, shown as a state.
    Failed(String),
}

/// State behind [`PaletteMode::GithubRepoPicker`].
///
/// Held on `AppUiState` (rather than inside the modal) for the same reason
/// `program_picker_choices` is: the fetch resolves asynchronously and the
/// refilter rebuilds the palette rows from it on every keystroke. Only read
/// while that palette mode is open, so a stale value between opens is inert.
#[derive(Debug, Clone)]
pub struct RepoPicker {
    /// Backend the listing came from and the clone will run on. Frozen when the
    /// picker opens so a selection change mid-flow can't retarget it — a clone
    /// runs where the sessions run, and the two hosts have different disks.
    pub backend: BackendId,
    /// Repos fetched from `backend`; the source the fuzzy filter narrows.
    pub repos: Vec<claude_commander_protocol::github::GithubRepo>,
    /// Where the listing is up to.
    pub fetch: RepoFetch,
    /// Bumped on each (re)fetch so a superseded response is dropped on arrival.
    pub generation: u64,
}

impl Default for RepoPicker {
    fn default() -> Self {
        Self {
            backend: LOCAL_BACKEND_ID,
            repos: Vec::new(),
            fetch: RepoFetch::default(),
            generation: 0,
        }
    }
}

/// A single branch entry in the checkout modal list
#[derive(Debug, Clone)]
pub struct BranchEntry {
    /// Local branch name used for checkout (e.g. `"feature-auth"`).
    /// For remote-only branches this is the remote ref without the
    /// `origin/` prefix.
    pub local_name: String,
    /// Label shown in the UI — for remote-only branches this is the
    /// full `origin/<name>` form; for local branches it's the same as
    /// `local_name`.
    pub display_name: String,
    /// True when this branch only exists remotely (no local tracking branch).
    pub is_remote: bool,
}

/// Which tab is active in the settings modal
///
/// Purely in-memory UI state — the modal opens on `General` every time and the
/// active tab is never written to `config.toml` or `tui.json`, so the variants
/// can be renamed freely. [`Voice`](Self::Voice) was called `Conversation` until
/// it grew speech-to-text and dictation alongside the spoken replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    General,
    Voice,
    Keybindings,
    Theme,
    Sections,
    Workspaces,
    Programs,
    Server,
}

impl SettingsTab {
    const ALL: [SettingsTab; 8] = [
        Self::General,
        Self::Voice,
        Self::Keybindings,
        Self::Theme,
        Self::Sections,
        Self::Workspaces,
        Self::Programs,
        Self::Server,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Voice => "Voice",
            Self::Keybindings => "Keybindings",
            Self::Theme => "Theme",
            Self::Sections => "Sections",
            Self::Workspaces => "Workspaces",
            Self::Programs => "Programs",
            Self::Server => "Server",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::General => Self::Voice,
            Self::Voice => Self::Keybindings,
            Self::Keybindings => Self::Theme,
            Self::Theme => Self::Sections,
            Self::Sections => Self::Workspaces,
            Self::Workspaces => Self::Programs,
            Self::Programs => Self::Server,
            Self::Server => Self::General,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::General => Self::Server,
            Self::Voice => Self::General,
            Self::Keybindings => Self::Voice,
            Self::Theme => Self::Keybindings,
            Self::Sections => Self::Theme,
            Self::Workspaces => Self::Sections,
            Self::Programs => Self::Workspaces,
            Self::Server => Self::Programs,
        }
    }
}

/// Which pane is focused in the Sections tab
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SectionsFocus {
    #[default]
    List,
    Predicates,
}

/// State for the Sections tab within the settings modal
#[derive(Debug, Clone)]
pub struct SectionsState {
    pub selected_section: usize,
    pub focus: SectionsFocus,
    pub pred_selected: usize,
    pub editing: Option<SectionsEditing>,
}

/// Editing state for the Sections tab
#[derive(Debug, Clone)]
pub enum SectionsEditing {
    RenamingSection { value: Input },
    EditingPredicate { value: Input },
    CreatingSection { value: Input },
}

impl Default for SectionsState {
    fn default() -> Self {
        Self {
            selected_section: 0,
            focus: SectionsFocus::List,
            pred_selected: 0,
            editing: None,
        }
    }
}

/// Which pane is focused in the Workspaces tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspacesFocus {
    #[default]
    List,
    /// The right pane: the theme row, then the workspace's projects.
    Detail,
}

/// State for the Workspaces tab. The list it edits is the *merged* one across
/// every backend (index 0 is Main), read fresh from the snapshots each frame,
/// so this holds only cursor and editing state.
#[derive(Debug, Clone, Default)]
pub struct WorkspacesState {
    /// Index into the merged workspace list.
    pub selected: usize,
    pub focus: WorkspacesFocus,
    /// Detail-pane row: 0 = theme, 1.. = the workspace's projects.
    pub detail_selected: usize,
    pub editing: Option<WorkspacesEditing>,
}

/// Editing state for the Workspaces tab.
#[derive(Debug, Clone)]
pub enum WorkspacesEditing {
    Creating {
        value: Input,
    },
    Renaming {
        value: Input,
    },
    /// `m` on a project: pick its target in the left list (`target` indexes
    /// the merged list), Enter to move, Esc to cancel.
    MovingProject {
        project_id: ProjectId,
        target: usize,
    },
}

/// Which pane is focused in the Programs tab
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProgramsFocus {
    #[default]
    List,
    Fields,
}

/// State for the Programs tab within the settings modal.
///
/// The tab edits a working copy (`entries`) rather than `config.programs`
/// directly, because the `target` backend may be a *remote* server whose program
/// list lives on the server, not in the local config. `entries` is the single
/// source the editor reads and mutates; each committed edit is pushed to the
/// `target` backend (local → local config + persist; remote → `set_programs`
/// over the API). For a remote target the list is fetched asynchronously, so the
/// tab shows a `loading` / `load_error` state until it arrives.
#[derive(Debug, Clone)]
pub struct ProgramsState {
    /// Which backend's program list is being edited (local or a remote server).
    pub target: BackendId,
    /// The working copy of `target`'s program list.
    pub entries: Vec<claude_commander_core::config::ProgramEntry>,
    /// Index into `entries`.
    pub selected: usize,
    pub focus: ProgramsFocus,
    /// 0 = label, 1 = command.
    pub field_selected: usize,
    pub editing: Option<ProgramsEditing>,
    /// A remote fetch of `entries` is in flight; editing is disabled until it
    /// resolves.
    pub loading: bool,
    /// A remote fetch failed; `entries` is empty and editing is disabled.
    pub load_error: Option<String>,
    /// A background save (remote PUT) failed; shown as a non-blocking warning
    /// line. `entries` is unaffected and editing stays enabled. Cleared on the
    /// next committed edit.
    pub save_error: Option<String>,
    /// Bumped on each (re)load so a stale async response for a superseded target
    /// can be discarded on arrival.
    pub load_gen: u64,
}

impl Default for ProgramsState {
    fn default() -> Self {
        Self {
            target: LOCAL_BACKEND_ID,
            entries: Vec::new(),
            selected: 0,
            focus: ProgramsFocus::default(),
            field_selected: 0,
            editing: None,
            loading: false,
            load_error: None,
            save_error: None,
            load_gen: 0,
        }
    }
}

/// Editing state for the Programs tab
#[derive(Debug, Clone)]
pub enum ProgramsEditing {
    /// Renaming the selected entry's label (`r` in list focus).
    RenamingLabel { value: Input },
    /// Editing the selected field (label or command) in Fields focus.
    EditingField { value: Input },
    /// Step 1 of `n`: naming the just-added entry. The entry already exists in
    /// `entries` (so it shows in the list immediately); this only edits its
    /// label. An empty/duplicate label keeps the auto-generated default.
    CreatingLabel { value: Input },
    /// Step 2 of `n`: setting the just-added entry's command (seeded from its
    /// label). The entry is already in `entries`; this only edits its command.
    CreatingCommand { value: Input },
}

/// State for the settings modal
#[derive(Debug, Clone)]
pub struct SettingsState {
    pub tab: SettingsTab,
    pub selected_row: usize,
    pub editing: Option<SettingsEditing>,
    /// Cached row data for the current tab
    pub rows: Vec<SettingsRow>,
    /// State for the Sections tab (lazily initialised on first tab switch)
    pub sections_state: SectionsState,
    /// State for the Workspaces tab
    pub workspaces_state: WorkspacesState,
    /// State for the Programs tab (lazily initialised on first tab switch)
    pub programs_state: ProgramsState,
    /// Which theme the Theme tab edits. Opening settings defaults it to the
    /// active workspace once there are two or more; the Workspaces tab's
    /// Theme row sets it to the selected workspace.
    pub theme_scope: ThemeScope,
    /// Active search filter for the Keybindings tab. `Some` while the search
    /// box is focused (typing filters the shortcut list live); `None` when the
    /// list is browsed normally.
    pub search: Option<Input>,
}

/// Which theme Settings → Theme edits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ThemeScope {
    /// `[theme]`, which every workspace without its own preset inherits.
    #[default]
    Usual,
    /// One workspace's `[workspace_themes."<name>"]` (`None` = Main).
    Workspace(Option<String>),
}

/// Kind of a settings row, carrying its typed value.
///
/// Each variant holds its own value — a display/edit string for `Text`, a live
/// `bool` for `Toggle` — so the row model is fully typed rather than stuffing
/// everything into a string. String conversion only happens at the
/// preferences-file boundary (serde) and the text-input edit path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsRowKind {
    /// Free-text field edited via a text-input box.
    Text(String),
    /// Two-state boolean rendered as a checkbox and flipped in place.
    Toggle(bool),
    /// A non-selectable section heading (Keybindings tab grouping). Carries no
    /// value and is skipped by navigation.
    Header,
}

/// A single row in the settings list
#[derive(Debug, Clone)]
pub struct SettingsRow {
    pub label: String,
    pub field_key: String,
    pub kind: SettingsRowKind,
    /// Optional color for displaying a swatch next to the value (Theme tab only)
    pub color_swatch: Option<Color>,
    /// Where an inherited value comes from (`"usual"`, `"preset"`): the row
    /// has no value of its own, so it is drawn dim with this note. `None` for
    /// a value set in the scope being edited (Theme tab only).
    pub inherited_from: Option<&'static str>,
}

impl SettingsRow {
    /// A free-text settings row.
    pub fn text(
        label: impl Into<String>,
        value: impl Into<String>,
        field_key: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            field_key: field_key.into(),
            kind: SettingsRowKind::Text(value.into()),
            color_swatch: None,
            inherited_from: None,
        }
    }

    /// A two-state boolean toggle row.
    pub fn toggle(label: impl Into<String>, on: bool, field_key: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            field_key: field_key.into(),
            kind: SettingsRowKind::Toggle(on),
            color_swatch: None,
            inherited_from: None,
        }
    }

    /// A non-selectable section heading.
    pub fn header(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            field_key: String::new(),
            kind: SettingsRowKind::Header,
            color_swatch: None,
            inherited_from: None,
        }
    }

    /// Whether this row can be highlighted/edited. Section headers cannot.
    pub fn is_selectable(&self) -> bool {
        !matches!(self.kind, SettingsRowKind::Header)
    }

    /// A text row that also displays a color swatch (Theme tab).
    pub fn swatch(
        label: impl Into<String>,
        value: impl Into<String>,
        field_key: impl Into<String>,
        color: Color,
    ) -> Self {
        Self {
            label: label.into(),
            field_key: field_key.into(),
            kind: SettingsRowKind::Text(value.into()),
            color_swatch: Some(color),
            inherited_from: None,
        }
    }

    /// The display/edit string for a `Text` row; `""` for a `Toggle` row.
    pub fn text_value(&self) -> &str {
        match &self.kind {
            SettingsRowKind::Text(v) => v,
            SettingsRowKind::Toggle(_) | SettingsRowKind::Header => "",
        }
    }

    /// For a Toggle row, the flipped boolean; `None` for other kinds.
    pub fn toggled(&self) -> Option<bool> {
        match self.kind {
            SettingsRowKind::Toggle(on) => Some(!on),
            SettingsRowKind::Text(_) | SettingsRowKind::Header => None,
        }
    }
}

/// Editing state within the settings modal
#[derive(Debug, Clone)]
pub enum SettingsEditing {
    /// Editing a text value
    TextInput { value: Input },
    /// Capturing a key for keybinding
    KeyCapture {
        action_name: String,
        keys: Vec<String>,
    },
    /// Picking from a list of options (theme presets, speak scope, microphone).
    OptionPicker {
        options: Vec<PickerOption>,
        selected: usize,
    },
    /// A Theme-tab colour row: the swatch grid and hex row.
    Colour {
        picker: Box<colour_picker::ColourPicker>,
    },
}

/// One entry in an [`SettingsEditing::OptionPicker`]: the `label` shown in the
/// dropdown and the `value` committed to config when it's chosen. They differ
/// only when the display text isn't the stored value — e.g. a microphone's
/// friendly name (label) versus its stable device id (value). For simple enum
/// pickers use [`PickerOption::plain`], where the two are the same.
#[derive(Debug, Clone)]
pub struct PickerOption {
    pub label: String,
    pub value: String,
}

impl PickerOption {
    /// A picker entry whose committed value is the same as its display label.
    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            label: text.clone(),
            value: text,
        }
    }
}

/// Which field of the New Session input modal currently has focus. Tab cycles
/// through the fields that are present; the modal owns the value so the
/// name/project/program sections stay mutually exclusive by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFocus {
    /// The session-name text field (default on open).
    Name,
    /// The server/backend picker (only present when >1 backend is configured).
    Server,
    /// The filterable project picker.
    Project,
    /// The program/agent picker.
    Program,
    /// The section picker (only present when the backend has configured sections).
    Section,
}

/// Which of the New Session dialog's picker fields are present, so the focus
/// ring can skip the absent ones. Name is always present and so isn't tracked.
#[derive(Debug, Clone, Copy)]
pub struct FieldsPresent {
    pub server: bool,
    pub project: bool,
    pub program: bool,
    pub section: bool,
}

impl InputFocus {
    /// Ordered ring of the fields that exist; Name is always present. The order
    /// mirrors the dialog's top-to-bottom layout: Name → Server → Project →
    /// Program → Section.
    fn ring(fields: FieldsPresent) -> Vec<InputFocus> {
        let mut ring = vec![InputFocus::Name];
        if fields.server {
            ring.push(InputFocus::Server);
        }
        if fields.project {
            ring.push(InputFocus::Project);
        }
        if fields.program {
            ring.push(InputFocus::Program);
        }
        if fields.section {
            ring.push(InputFocus::Section);
        }
        ring
    }

    /// The next focus when Tab is pressed, skipping fields that aren't present.
    pub fn next(self, fields: FieldsPresent) -> InputFocus {
        let ring = Self::ring(fields);
        let idx = ring.iter().position(|f| *f == self).unwrap_or(0);
        ring[(idx + 1) % ring.len()]
    }

    /// The previous focus when Shift+Tab is pressed, skipping absent fields.
    pub fn prev(self, fields: FieldsPresent) -> InputFocus {
        let ring = Self::ring(fields);
        let idx = ring.iter().position(|f| *f == self).unwrap_or(0);
        ring[(idx + ring.len() - 1) % ring.len()]
    }
}

/// Maximum project rows shown at once in the New Session project picker; the
/// list scrolls to keep the highlighted row visible. Shared by the input
/// handler (scroll bookkeeping) and the renderer.
pub(super) const MAX_PROJECT_ROWS: usize = 6;

/// Program-picker state embedded in the New Session input modal: the
/// selectable harnesses and the highlighted index.
#[derive(Debug, Clone)]
pub struct ProgramPicker {
    /// Selectable harnesses, from `Config::program_choices`.
    pub choices: Vec<claude_commander_core::config::ProgramEntry>,
    /// Index into `choices` of the highlighted entry.
    pub selected: usize,
}

impl ProgramPicker {
    /// The launch command of the highlighted entry, if any.
    pub fn selected_command(&self) -> Option<String> {
        self.choices.get(self.selected).map(|e| e.command.clone())
    }

    /// Move the highlight up one entry (saturating at the top).
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the highlight down one entry (saturating at the bottom).
    pub fn select_down(&mut self) {
        if self.selected + 1 < self.choices.len() {
            self.selected += 1;
        }
    }
}

/// Server-picker state embedded in the New Session input modal: the selectable
/// backends and the highlighted index. Only present when more than one backend
/// is configured (a single-backend setup hides the field, matching the
/// suppressed server headers in the session list).
#[derive(Debug, Clone)]
pub struct ServerPicker {
    /// Selectable backends as `(id, display name)`, in `App::backends` order.
    pub choices: Vec<(BackendId, String)>,
    /// Index into `choices` of the highlighted entry.
    pub selected: usize,
    /// Index of the last *applied* selection. A confirm that doesn't move off
    /// this index is a no-op (no picker rebuild), so re-confirming the current
    /// server never resets the project selection.
    pub committed: usize,
}

impl ServerPicker {
    /// Build a picker over `choices`, highlighting the entry for `default`
    /// (falling back to the first entry).
    pub fn new(choices: Vec<(BackendId, String)>, default: BackendId) -> Self {
        let selected = choices
            .iter()
            .position(|(id, _)| *id == default)
            .unwrap_or(0);
        Self {
            choices,
            selected,
            committed: selected,
        }
    }

    /// The backend id of the highlighted entry, if any.
    pub fn selected_backend(&self) -> Option<BackendId> {
        self.choices.get(self.selected).map(|(id, _)| *id)
    }

    /// Move the highlight up one entry (saturating at the top).
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the highlight down one entry (saturating at the bottom).
    pub fn select_down(&mut self) {
        if self.selected + 1 < self.choices.len() {
            self.selected += 1;
        }
    }
}

/// Section-picker state embedded in the New Session input modal. `choices[0]` is
/// always the catch-all [`claude_commander_core::session::IN_PROGRESS`] ("In Progress"); the rest
/// are the backend's configured section names. The catch-all row maps to
/// `section = None` (no override → auto-assign by rules).
#[derive(Debug, Clone)]
pub struct SectionPicker {
    /// The catch-all first, then the configured section names.
    pub choices: Vec<String>,
    /// Index into `choices` of the highlighted entry.
    pub selected: usize,
}

impl SectionPicker {
    /// Build a picker over the catch-all plus the configured `sections`,
    /// pre-selecting the row matching `default` (the catch-all when `default` is
    /// `None`, or when it names a section that isn't configured).
    pub fn new(sections: Vec<String>, default: Option<&str>) -> Self {
        let mut choices = vec![claude_commander_core::session::IN_PROGRESS.to_string()];
        choices.extend(sections);
        // Match a configured section from row 1 on, so a default naming a section
        // that happens to be spelled like the catch-all ("In Progress") still
        // selects the real configured row rather than resolving to row 0.
        let selected = default
            .and_then(|name| {
                choices
                    .iter()
                    .skip(1)
                    .position(|c| c == name)
                    .map(|i| i + 1)
            })
            .unwrap_or(0);
        Self { choices, selected }
    }

    /// The chosen section: `None` for the catch-all row (row 0), else the name.
    pub fn selected_section(&self) -> Option<String> {
        if self.selected == 0 {
            None
        } else {
            self.choices.get(self.selected).cloned()
        }
    }

    /// Move the highlight up one entry (saturating at the top).
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the highlight down one entry (saturating at the bottom).
    pub fn select_down(&mut self) {
        if self.selected + 1 < self.choices.len() {
            self.selected += 1;
        }
    }
}

/// A project that can be chosen as the target of a new session.
#[derive(Debug, Clone)]
pub struct ProjectChoice {
    pub id: ProjectId,
    pub name: String,
    pub repo_path: PathBuf,
}

/// Filterable project-picker state embedded in the New Session input modal.
/// `choices` holds every project (sorted by name); `filtered` is the subset
/// matching `filter`, and `selected` indexes into `filtered`.
#[derive(Debug, Clone)]
pub struct ProjectPicker {
    pub choices: Vec<ProjectChoice>,
    pub filter: String,
    pub filtered: Vec<usize>,
    pub selected: usize,
    /// First visible row (index into `filtered`) — keeps the highlight on
    /// screen as the list scrolls, maintained via `adjust_list_scroll`.
    pub scroll: usize,
    /// Memoized branch lists per repo path, so switching the highlight only
    /// lists a given project's branches once per dialog session.
    pub branch_cache: HashMap<PathBuf, Option<Vec<String>>>,
    /// Whether the existing-branch collision hint is computed as the highlight
    /// moves. The hint runs a local gix scan on the project's `repo_path`,
    /// which is only meaningful for a local backend — a remote backend's
    /// `repo_path` lives on the server, so its picker disables the hint rather
    /// than making a network call at keystroke time.
    pub branch_hint_enabled: bool,
}

impl ProjectPicker {
    /// Build a picker over `choices`, highlighting `default` (falling back to
    /// the first entry). The filter starts empty, so all choices are visible.
    pub fn new(choices: Vec<ProjectChoice>, default: ProjectId) -> Self {
        let filtered = (0..choices.len()).collect();
        let selected = choices.iter().position(|c| c.id == default).unwrap_or(0);
        let mut picker = Self {
            choices,
            filter: String::new(),
            filtered,
            selected,
            scroll: 0,
            branch_cache: HashMap::new(),
            branch_hint_enabled: true,
        };
        picker.adjust_scroll();
        picker
    }

    /// Keep `scroll` positioned so the highlighted row stays visible, reusing
    /// the same window helper as the other scrolling lists.
    fn adjust_scroll(&mut self) {
        self.scroll = actions::adjust_list_scroll(self.selected, self.scroll, MAX_PROJECT_ROWS);
    }

    /// The highlighted project, if any.
    pub fn selected_choice(&self) -> Option<&ProjectChoice> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.choices.get(i))
    }

    /// The id of the highlighted project, if any.
    pub fn selected_id(&self) -> Option<ProjectId> {
        self.selected_choice().map(|c| c.id)
    }

    /// The repo path of the highlighted project, if any.
    pub fn selected_repo_path(&self) -> Option<PathBuf> {
        self.selected_choice().map(|c| c.repo_path.clone())
    }

    /// Move the highlight up one entry (saturating at the top).
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.adjust_scroll();
    }

    /// Move the highlight down one entry (saturating at the bottom).
    pub fn select_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
        self.adjust_scroll();
    }

    /// Recompute `filtered` from `filter`, re-anchoring the highlight onto the
    /// previously-selected project when it survives (else clamp to the top).
    /// An empty filter shows every project in name order; otherwise entries are
    /// ranked best-fuzzy-match first via `claude_commander_viewmodel::fuzzy_score`.
    pub fn apply_filter(&mut self) {
        let prev_id = self.selected_id();
        if self.filter.is_empty() {
            self.filtered = (0..self.choices.len()).collect();
        } else {
            let mut scored: Vec<(usize, i64)> = self
                .choices
                .iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    claude_commander_viewmodel::fuzzy_score(&c.name, &self.filter).map(|s| (i, s))
                })
                .collect();
            // Highest score first; ties keep the original (name-sorted) order.
            scored.sort_by_key(|b| std::cmp::Reverse(b.1));
            self.filtered = scored.into_iter().map(|(i, _)| i).collect();
        }
        self.selected = prev_id
            .and_then(|id| self.filtered.iter().position(|&i| self.choices[i].id == id))
            .unwrap_or(0);
        self.adjust_scroll();
    }
}

/// Action to perform when input modal is submitted
#[derive(Debug, Clone)]
pub enum InputAction {
    CreateSession {
        project_id: ProjectId,
        /// Section the tree cursor was in when the modal opened; the new
        /// session is placed there (`None` = default "In Progress").
        section: Option<String>,
    },
    CreateStackedSession {
        project_id: ProjectId,
        parent_session_id: SessionId,
    },
    AddProject,
    ScanDirectory,
    RenameSession {
        session_id: SessionId,
    },
    /// Step 1 of the add-remote-server flow: the server's display name.
    AddRemoteServerName,
    /// Step 2: the server's base URL.
    AddRemoteServerUrl {
        name: String,
    },
    /// Step 3: the bearer token (masked; empty = no token). Submission kicks
    /// off the async connection probe.
    AddRemoteServerToken {
        name: String,
        url: String,
    },
    /// Step 2 of the clone flow: the destination directory name, prefilled with
    /// the name derived from `source` and editable. An empty submission means
    /// "derive it" (`CloneRequest::dest_name = None`), which is a valid choice
    /// rather than an error.
    /// Name for a new workspace (`NewWorkspace`); created on every backend,
    /// then switched to.
    NewWorkspace,
    CloneDestName {
        /// Backend the clone runs on — the one the picker was opened against,
        /// carried explicitly so it can't drift with the tree selection.
        backend: BackendId,
        source: claude_commander_protocol::github::CloneSource,
    },
}

/// Action to perform when confirm modal is confirmed
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    DeleteSession {
        session_id: SessionId,
    },
    DeleteMergedPrSessions {
        session_ids: Vec<SessionId>,
    },
    RestartSession {
        session_id: SessionId,
    },
    /// Relaunch a session's pane *without* resuming, discarding the agent's
    /// conversation.
    ResetSession {
        session_id: SessionId,
    },
    /// Change a session's program (agent) to `program` and relaunch it fresh.
    ChangeProgram {
        session_id: SessionId,
        program: String,
    },
    RemoveProject {
        project_id: ProjectId,
    },
    /// Retarget a session's stack base (picked via the palette).
    SetSessionBase {
        session_id: SessionId,
        target: Option<SessionId>,
    },
    /// Save a remote server whose connection probe failed ("save anyway").
    AddRemoteServerAnyway {
        server: RemoteServerConfig,
    },
    /// Remove a configured remote server (picked via the palette).
    RemoveRemoteServer {
        name: String,
    },
    /// A clone found its destination already occupied by a git checkout, and the
    /// user accepted the offer to register that checkout as a project instead.
    RegisterExistingClone {
        /// Backend that reported the occupied destination — the path is on *its*
        /// disk, so the project must be added there.
        backend: BackendId,
        dest: PathBuf,
    },
}

/// Application UI state
pub struct AppUiState {
    /// Active view. List modes (`ProjectGrouped`/`SectionGrouped`/
    /// `SectionStacks`) render the tree-list; `Board` renders the kanban board.
    /// Cycled with `v` (`ToggleViewMode`).
    pub view_mode: ViewMode,
    /// Flat tree-list rows for the active list view. Rebuilt only in a list
    /// view; retains the last list-view rows while the board is shown (consumers
    /// dispatch on `view_mode`, so the stale rows are never read there).
    pub list_items: Vec<SessionListItem>,
    /// Tree-list navigation state (list modes only).
    pub list_state: TreeListState,
    /// Section headers the user has collapsed (section list modes).
    pub collapsed_sections: std::collections::HashSet<String>,
    /// Kanban board model (sidebar + section columns). Rebuilt from `AppState`
    /// each `refresh_list_items`.
    pub board: Board,
    /// Board navigation state (cursor position + per-column scroll).
    pub board_state: BoardState,
    /// Clickable board regions from the last render frame, mapping screen rects
    /// to board positions for mouse clicks. Rebuilt every frame.
    pub board_hit_regions: Vec<BoardHitRegion>,
    /// Clickable card-button regions from the last render frame, mapping screen
    /// rects to a `(board position, action button)`. Checked before
    /// `board_hit_regions` so a click on a button both selects the card and
    /// fires its action. Rebuilt every frame.
    pub board_button_regions: Vec<BoardButtonRegion>,
    /// Sidebar server-heading hit regions from the last render frame; a click
    /// opens that server's Settings → Programs tab.
    pub board_heading_regions: Vec<(Rect, claude_commander_core::backend::BackendId)>,
    /// Column/sidebar rectangles from the last render frame, for wheel-scroll
    /// targeting. `None` before the first board render.
    pub board_column_rects: Option<BoardRects>,
    /// Active project filter: when `Some`, the board columns show only that
    /// project's cards (the sidebar still lists every project). Set by the
    /// sidebar cursor (see `refresh_list_items`), cleared by Esc in the main
    /// view. `ProjectId`s are globally unique, so this needs no backend qualifier.
    pub board_filter: Option<ProjectId>,
    /// The workspace the user asked to see (`None` = Main). Kept as asked:
    /// the *effective* workspace is this when some backend still has it, else
    /// Main (see `App::active_workspace`), so a startup workspace takes effect
    /// once the snapshot carrying it arrives.
    pub active_workspace: Option<String>,
    /// Enriched PR info for the currently selected session
    pub enriched_pr: Option<(SessionId, EnrichedPrInfo)>,
    /// Session whose enriched-PR fetch came back empty (no PR data, or `gh`
    /// failed). Without this a failed fetch would never cache, so an Info
    /// surface left open would respawn `gh` every few seconds forever. Cleared
    /// by an explicit PR-status refresh, which is the retry path.
    pub enriched_pr_unavailable: Option<SessionId>,
    /// Cached AI summaries keyed by session ID
    pub ai_summaries: std::collections::HashMap<SessionId, AiSummary>,
    /// Current modal
    pub modal: Modal,
    /// Column-major session numbering (session id → 1-based number) for the
    /// board rows. Recomputed in `refresh_list_items` and borrowed by the board
    /// widget so it isn't rebuilt on every paint.
    pub session_numbers: HashMap<SessionId, usize>,
    /// Whether the board's sessions span more than one distinct program.
    /// Recomputed in `refresh_list_items`; gates the `(program)` row suffix.
    pub has_mixed_programs: bool,
    /// Per-project (border, session-title) colours, keyed by project id in the
    /// board's name-sorted order. Recomputed in `refresh_list_items` and
    /// borrowed by the board widget rather than rebuilt per frame.
    pub project_colors: HashMap<ProjectId, (Color, Color)>,
    /// Number of leading `list_items` that make up the pinned recents block
    /// (list views only). The first `recents_len` items form the pinned
    /// "Recent" block (header + shortcut rows + divider) that renders in a
    /// fixed panel above the scrolling list; the rest render in the scrollable
    /// main list. `0` when there is no recents block.
    pub recents_len: usize,
    /// Persisted scroll offset of the main (scrolling) list — the item index,
    /// relative to `list_items[recents_len..]`, shown at the top of the list
    /// area. Kept across frames so the main list remembers its scroll position
    /// while the recents panel stays pinned. ratatui updates it each render.
    pub main_list_offset: usize,
    /// Visible row count of the main (scrolling) list, recorded each render.
    /// The list body has no border, so one row is one item — this is the page
    /// size for `ListPageUp`/`ListPageDown` in the list views.
    pub main_list_height: u16,
    /// Diff info
    pub diff_info: Arc<DiffInfo>,
    /// Which live view the right-hand pane shows (list views only — the board
    /// is full-screen and has no right pane).
    pub right_pane_view: RightPaneView,
    /// Latest agent-pane capture for the selection, fed by `spawn_preview_update`.
    pub preview_content: String,
    /// Latest shell-pane capture for the selection.
    pub shell_content: String,
    /// Scroll/follow state for the Preview tab.
    pub preview_state: PreviewState,
    /// Scroll/follow state for the Shell tab.
    pub shell_state: PreviewState,
    /// Scroll state for the Info tab. Its line count is known up front, so it
    /// is driven by `set_metrics` rather than by scanning captured text.
    pub info_state: PreviewState,
    /// Left (session-list) pane width as a percentage of the content area,
    /// clamped to [`MIN_LEFT_PANE_PCT`]..=[`MAX_LEFT_PANE_PCT`]. Persisted in
    /// `tui.json`.
    pub left_pane_pct: u16,
    /// When the last background preview/shell capture was spawned (None = not
    /// in flight). Guards `spawn_preview_update` against double-spawns.
    pub preview_update_spawned_at: Option<Instant>,
    /// Right-pane rect from the last render frame, for wheel hit-testing.
    /// `None` in board view (no right pane) and before the first frame.
    pub right_pane_rect: Option<Rect>,
    /// Effective right-pane view as of the last frame, so the renderer can reset
    /// the pane's cells when the tab actually being shown changes.
    pub last_pane_view: Option<RightPaneView>,
    /// Status message (with expiry time)
    pub status_message: Option<(String, Instant)>,
    /// Backend ids (raw) that have already shown their one-time version-mismatch
    /// toast this session, so a stale server warns once rather than on every
    /// snapshot fold. Ids are monotonic, so a re-added server gets a fresh id
    /// and re-toasts.
    pub version_warned: HashSet<usize>,
    /// Should quit
    pub should_quit: bool,
    /// Last known terminal size (updated each render frame)
    pub terminal_size: Rect,
    /// Whose theme `App::theme` was last built for: `None` for the usual
    /// `[theme]` (workspace UI hidden), `Some(workspace)` (`Some(None)` = Main)
    /// once there are workspaces. `refresh_list_items` rebuilds the theme when
    /// `App::theme_workspace` moves off it.
    pub theme_workspace: Option<Option<String>>,
    /// Inner rect of the review-diff body pane, recorded each render frame so
    /// mouse events can map a screen position to a diff line. `None` unless the
    /// review view is open.
    pub review_body_rect: Option<Rect>,
    /// Inner rect of the review-diff file-list pane, recorded each render frame
    /// so mouse events can map a screen position to a tree row. `None` unless
    /// the review view is open.
    pub review_file_list_rect: Option<Rect>,
    /// Rows-only area of the open list modal (quick-switch, checkout-branch,
    /// or path-input completions), recorded each render frame so mouse clicks
    /// can be mapped to list indices. `None` when no list modal is open.
    pub modal_list_rect: Option<Rect>,
    /// Clickable action-button regions from the main-view status bar, recorded
    /// each render frame so a left-click can be mapped back to its
    /// `BindableAction`. Rebuilt every frame; empty when no buttons are drawn.
    pub action_buttons: Vec<crate::hotkey::ActionButton>,
    /// Clickable action-button regions from the review-view footer, recorded
    /// each render frame. Review keys are view-local, so each button carries a
    /// synthesized `KeyEvent` fed straight into `handle_review_key`.
    pub review_buttons: Vec<review::ReviewButton>,
    /// Sessions with at least one pending (not-yet-applied) review comment.
    /// Drives the `*` marker in the session list; refreshed on startup and
    /// whenever the review view closes.
    pub sessions_with_comments: HashSet<SessionId>,
    /// Currently selected session (for preview/diff), qualified by the backend
    /// that owns it so actions route to the right machine.
    pub selected_session_id: Option<SessionRef>,
    /// Currently selected project, qualified by its owning backend.
    pub selected_project_id: Option<(BackendId, ProjectId)>,
    /// Whether the backend owning the current selection is connected. Cached in
    /// `update_selection` so the (sync, backend-unaware) `is_command_available`
    /// can gate actions on a live backend. Always `true` for the local backend.
    pub selected_backend_connected: bool,
    /// Capabilities of the backend owning the current selection. Cached in
    /// `update_selection` (same reason as `selected_backend_connected`) so
    /// `is_command_available` can hide local-only actions (e.g. open-in-editor)
    /// for a remote-backed selection. Defaults to the all-on local set.
    pub selected_backend_capabilities: BackendCapabilities,
    /// Whether the `cc-commander` tmux session is currently running. Cached from
    /// the background agent-state poll so the (sync) renderers — the footer chip
    /// — can read it without awaiting tmux.
    pub commander_running: bool,
    /// The in-process HTTP server's outcome, or `None` when this run was never
    /// asked to serve. Set once at startup by the binary (which owns the server
    /// dependency) and read by the status-bar chip and the copy-token command.
    pub embedded_server: Option<crate::EmbeddedServerStatus>,
    /// What to attach to after the TUI tears down (set by select/shell/commander).
    pub attach_request: Option<AttachTarget>,
    /// Session whose review diff should be opened on returning to the TUI —
    /// set when the user pressed Alt-r inside an attached session.
    pub pending_open_review: Option<SessionId>,
    /// Where the in-session switcher wants to attach next, when its pick can't
    /// be reached by `tmux switch-client` in place (it lives on another
    /// backend, or the switch failed). The attach loop consumes this and hops
    /// straight there rather than dropping the user back to the tree.
    pub pending_switcher_target: Option<AttachTarget>,
    /// Editor command + path to open after exiting TUI
    pub editor_command: Option<(String, PathBuf)>,
    /// When attached via shell toggle (Ctrl+\), stores the session name to switch back to.
    /// Contains (current_session_name, paired_session_name) so we can toggle between them.
    pub shell_toggle_pair: Option<(String, String)>,
    /// Force a full in-memory repaint on the next render (via the `Clear`
    /// widget, not `terminal.clear()` — see `render`). Set when leaving a
    /// full-screen modal takeover so its cells don't linger under the board.
    pub force_clear: bool,
    /// Whether the previous frame was a full-screen modal takeover
    /// (ReviewDiff/Conversation). A true→false transition triggers a `Clear`.
    pub prev_fullscreen: bool,
    /// Whether the `gh` CLI is available
    pub gh_available: bool,
    /// When the last enriched-PR fetch was spawned (None = not in flight).
    /// Guards `spawn_info_fetch` against double-spawns: `update_selection` now
    /// runs it every tick (via `refresh_list_items`), so without this an open
    /// Info modal would re-spawn a duplicate `gh` fetch each tick until the
    /// first resolves. Mirrors `preview_update_spawned_at`'s 5s safety window.
    pub enriched_pr_fetch_spawned_at: Option<Instant>,
    /// Whether a review-diff refresh re-compose is currently in flight, so the
    /// idle trigger and a manual refresh don't double-spawn.
    pub review_refresh_in_flight: bool,
    /// A reviewed-marks poll for the open review is outstanding; see
    /// `spawn_reviewed_marks_poll`.
    pub reviewed_poll_in_flight: bool,
    /// Tick counter for animations (incremented each render tick)
    pub tick_count: u64,
    /// Throbber/spinner state for loading modals
    pub throbber_state: throbber_widgets_tui::ThrobberState,
    /// Current agent states for Running Claude sessions (ephemeral, from background poller)
    pub agent_states: BTreeMap<SessionId, AgentState>,
    /// Cached mirror of `AppState::cascade_paused_at.is_some()` — used by
    /// `is_command_available` to gate the `CascadeResume` / `CascadeAbandon`
    /// palette entries without an async read on every keystroke. Refreshed in
    /// `refresh_list_items`.
    pub cascade_paused: bool,
    /// Last left-mouse click on a board row: (board position, timestamp).
    /// Used to detect double-click on the same row within `DOUBLE_CLICK_WINDOW`.
    pub last_left_click: Option<(BoardPos, Instant)>,
    /// Last left-mouse click on a session-list row: (list index, timestamp).
    /// The list-view analogue of `last_left_click` (double-click detection).
    pub last_list_click: Option<(usize, Instant)>,
    /// Screen rect of the scrolling main-list rows (below the heading and the
    /// pinned recents panel), recorded each frame in list view for mouse
    /// hit-testing. `None` in board view.
    pub list_rect: Option<Rect>,
    /// Screen rect of the pinned recents panel, recorded each frame in list view
    /// when a recents block is shown. `None` when there is no recents panel (or
    /// in board view).
    pub recents_rect: Option<Rect>,
    /// Last left-mouse click in the review diff body: (screen row, timestamp).
    /// Used to detect a double-click on the same row within `DOUBLE_CLICK_WINDOW`,
    /// which opens a comment box like a right-click.
    pub review_last_click: Option<(u16, Instant)>,
    /// Last left-mouse click on a list-modal row: (absolute list index,
    /// timestamp). Used to detect double-click on the same row within
    /// `DOUBLE_CLICK_WINDOW`. Cleared on any modal keystroke or paste, since
    /// typing can refilter the list out from under a pending click.
    pub modal_list_last_click: Option<(usize, Instant)>,
    /// Pre-computed stack chain for the selected session (empty if not stacked).
    pub stack_chain: Vec<StackChainEntry>,
    /// Projects whose most recent background pull was held back, with the
    /// reason. Folded out of the workspace snapshot's `project_pull` (which the
    /// service's pull loop maintains) to drive the per-project row badge.
    pub project_pull_blocked: HashMap<ProjectId, BlockReason>,
    /// Sessions with a background `git lfs pull` in flight (worktree created
    /// with smudging skipped). Drives the `⇣ LFS` row marker; an id is added
    /// when the pull is spawned and removed on `LfsPullFinished`.
    /// (The project-pull scheduler state that used to sit alongside this
    /// moved into `CommanderService`'s background tasks.)
    pub lfs_pull_in_flight: std::collections::HashSet<SessionId>,
    /// Program choices for the change-program palette (`ProgramPicker` mode).
    /// Re-seeded from local config each time the palette opens and replaced by
    /// the owning backend's list once `ProgramChoicesLoaded` arrives for a remote
    /// session. The refilter rebuilds the palette rows from this. Only read while
    /// the `ProgramPicker` palette is open, so a stale value between opens is
    /// inert (not cleared on close).
    pub program_picker_choices: Vec<claude_commander_core::config::ProgramEntry>,
    /// The current program of the session the change-program palette targets, so
    /// its row can be flagged. Re-set each time the palette opens; like
    /// `program_picker_choices`, only read while that palette is open.
    pub program_picker_current: String,
    /// Repo listing + fetch state behind the clone picker
    /// ([`PaletteMode::GithubRepoPicker`]). Reset each time the picker opens;
    /// only read while it is open, so a stale value between opens is inert.
    pub repo_picker: RepoPicker,
}

impl Default for AppUiState {
    fn default() -> Self {
        Self {
            view_mode: ViewMode::default(),
            list_items: Vec::new(),
            list_state: TreeListState::default(),
            collapsed_sections: std::collections::HashSet::new(),
            board: Board::default(),
            board_state: BoardState::new(),
            board_hit_regions: Vec::new(),
            board_button_regions: Vec::new(),
            board_heading_regions: Vec::new(),
            board_column_rects: None,
            board_filter: None,
            active_workspace: None,
            enriched_pr: None,
            enriched_pr_unavailable: None,
            ai_summaries: std::collections::HashMap::new(),
            modal: Modal::None,
            session_numbers: HashMap::new(),
            has_mixed_programs: false,
            project_colors: HashMap::new(),
            recents_len: 0,
            main_list_offset: 0,
            main_list_height: 0,
            diff_info: Arc::new(DiffInfo::empty()),
            right_pane_view: RightPaneView::default(),
            preview_content: String::new(),
            shell_content: String::new(),
            preview_state: PreviewState::new(),
            shell_state: PreviewState::new(),
            info_state: PreviewState::anchored_top(),
            left_pane_pct: DEFAULT_LEFT_PANE_PCT,
            preview_update_spawned_at: None,
            right_pane_rect: None,
            last_pane_view: None,
            status_message: None, // (message, expiry)
            version_warned: HashSet::new(),
            review_body_rect: None,
            review_file_list_rect: None,
            modal_list_rect: None,
            action_buttons: Vec::new(),
            review_buttons: Vec::new(),
            sessions_with_comments: HashSet::new(),

            should_quit: false,
            selected_session_id: None,
            selected_project_id: None,
            selected_backend_connected: true,
            selected_backend_capabilities: BackendCapabilities::LOCAL,
            commander_running: false,
            embedded_server: None,
            attach_request: None,
            pending_open_review: None,
            pending_switcher_target: None,
            editor_command: None,
            shell_toggle_pair: None,
            force_clear: false,
            prev_fullscreen: false,
            gh_available: false,
            enriched_pr_fetch_spawned_at: None,
            review_refresh_in_flight: false,
            reviewed_poll_in_flight: false,
            terminal_size: Rect::default(),
            theme_workspace: None,
            tick_count: 0,
            throbber_state: throbber_widgets_tui::ThrobberState::default(),
            agent_states: BTreeMap::new(),
            cascade_paused: false,
            last_left_click: None,
            last_list_click: None,
            list_rect: None,
            recents_rect: None,
            review_last_click: None,
            modal_list_last_click: None,
            stack_chain: Vec::new(),
            project_pull_blocked: HashMap::new(),
            lfs_pull_in_flight: std::collections::HashSet::new(),
            program_picker_choices: Vec::new(),
            program_picker_current: String::new(),
            repo_picker: RepoPicker::default(),
        }
    }
}

impl AppUiState {
    /// Whether the selection is a project row rather than a session. A project
    /// has no agent pane, so the right pane offers it Shell and Info only.
    pub fn is_project_selected(&self) -> bool {
        self.selected_session_id.is_none() && self.selected_project_id.is_some()
    }

    /// Whether the right pane is currently showing its Info tab. False on the
    /// board, which has no right pane.
    pub fn is_info_tab(&self) -> bool {
        !self.view_mode.is_board()
            && self.right_pane_view.effective(self.is_project_selected()) == RightPaneView::Info
    }

    /// Whether a given command is currently invokable.
    ///
    /// These rules mirror the early-return guards scattered across
    /// `App::handle_command` and friends, so the palette only lists
    /// commands that would actually *do* something if selected. Pure with
    /// respect to `self` — safe to unit-test by constructing a default
    /// `AppUiState` and mutating a few fields.
    pub fn is_command_available(&self, action: BindableAction) -> bool {
        // A degraded/connecting remote backend can't service actions against its
        // sessions/projects, so gate those on the selected backend being live.
        // The local backend is always connected, so single-machine setups are
        // unaffected.
        let connected = self.selected_backend_connected;
        let has_session = self.selected_session_id.is_some() && connected;
        let has_project = self.selected_project_id.is_some() && connected;
        match action {
            // Session-scoped actions require a selected session
            BindableAction::Select
            | BindableAction::SelectShell
            | BindableAction::DeleteSession
            | BindableAction::RenameSession
            | BindableAction::RestartSession
            | BindableAction::ResetSession
            | BindableAction::ChangeProgram
            | BindableAction::SetSessionBase
            | BindableAction::ToggleKeepAlive
            | BindableAction::OpenPullRequest
            | BindableAction::OpenReviewDiff
            | BindableAction::OpenInfo
            | BindableAction::MoveToSection => has_session,
            // Moving acts on the selected project (a session row selects its
            // project too) on its owning backend.
            BindableAction::MoveProjectToWorkspace => has_project,
            // Opening the operator's editor only works against a local worktree;
            // a remote backend has no path on this machine.
            BindableAction::OpenInEditor => {
                has_session && self.selected_backend_capabilities.open_editor
            }
            // Cascade merge is only meaningful from a session that's part of
            // a stack. We accept any selected session here; the handler is
            // cheap to no-op if the stack chain turns out to be length 1.
            BindableAction::CascadeMergeMain | BindableAction::PushStack => has_session,
            // Cascade resume / abandon are only meaningful when a cascade is paused.
            BindableAction::CascadeResume | BindableAction::CascadeAbandon => self.cascade_paused,
            // Removing a project is only meaningful from a project row (no session selected)
            BindableAction::RemoveProject => has_project && !has_session,
            // GenerateSummary is only meaningful while an Info surface is
            // showing — the modal or the right pane's Info tab — since that is
            // where the summary (and its `g` hotkey) is displayed.
            BindableAction::GenerateSummary => {
                has_session && (matches!(self.modal, Modal::Info { .. }) || self.is_info_tab())
            }
            // Creating a session or checking out a branch issues a request to the
            // selected project's backend (create-options / branch list). A
            // degraded/connecting remote can't service those and the awaits would
            // stall, so gate on that backend being live. Nothing selected keeps
            // `connected` true (local default), so single-machine setups and the
            // no-selection case are unaffected.
            // Same reasoning for the clone flow: the repo listing and the clone
            // itself both run on the selected backend's host.
            BindableAction::NewSession
            | BindableAction::CheckoutBranch
            | BindableAction::CloneRepository => connected,
            // The right pane only exists in the list views; the board is a
            // full-screen takeover, so these have nothing to act on there.
            BindableAction::TogglePane
            | BindableAction::TogglePaneReverse
            | BindableAction::ShrinkLeftPane
            | BindableAction::GrowLeftPane => !self.view_mode.is_board(),
            // Pairing a client only makes sense when something is actually
            // being served. Without this gate the wildcard below would list it
            // permanently, including in the overwhelmingly common run that
            // serves nothing at all.
            BindableAction::CopyServerToken => self
                .embedded_server
                .as_ref()
                .is_some_and(|s| s.token().is_some()),
            // All other actions are always available
            _ => true,
        }
    }

    /// Build the palette's command rows for a given filter query.
    ///
    /// Commands with no effective keybinding are still included — the
    /// palette is intended to be the canonical access surface as hotkeys
    /// get trimmed over time. Pure list motions (navigate up/down,
    /// group/first/last jumps) are excluded because moving the tree cursor
    /// from inside the palette makes no sense.
    pub fn gather_command_entries(
        &self,
        kb: &claude_commander_core::config::KeyBindings,
        filter_query: &str,
    ) -> Vec<CommandEntry> {
        let mut scored: Vec<(i64, CommandEntry)> = Vec::new();
        for &action in BindableAction::ALL {
            if matches!(
                action,
                BindableAction::NavigateUp
                    | BindableAction::NavigateDown
                    | BindableAction::NextGroup
                    | BindableAction::PreviousGroup
                    | BindableAction::NavigateFirst
                    | BindableAction::NavigateLast
                    | BindableAction::NavigateLeft
                    | BindableAction::NavigateRight
            ) {
                continue;
            }
            if !self.is_command_available(action) {
                continue;
            }
            let label = action.description();
            let Some(score) = claude_commander_viewmodel::fuzzy_score(label, filter_query) else {
                continue;
            };
            scored.push((
                score,
                CommandEntry {
                    action,
                    label,
                    keys: kb.keys_display(action),
                },
            ));
        }
        if !filter_query.is_empty() {
            // Stable sort by score desc preserves enum order among ties.
            scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        }
        scored.into_iter().map(|(_, e)| e).collect()
    }
}

/// Main TUI application
/// Serializes program-list writes to each backend. A remote save is a full-list
/// PUT; committing per edit means holding a reorder key fires several in quick
/// succession, and HTTP gives no ordering guarantee — so an earlier snapshot
/// could land after a later one and leave the server on a stale list. This runs
/// one background task per target that applies snapshots in submission order,
/// coalescing to the latest pending one.
#[derive(Default)]
pub(crate) struct ProgramCommitQueue {
    senders: HashMap<
        BackendId,
        tokio::sync::mpsc::UnboundedSender<Vec<claude_commander_core::api::ProgramInfo>>,
    >,
}

impl ProgramCommitQueue {
    /// Queue `programs` to be written to `backend` (id `id`). Spawns the target's
    /// worker on first use. A failed PUT posts [`StateUpdate::ServerProgramsSaveFailed`].
    fn submit(
        &mut self,
        id: BackendId,
        backend: Arc<dyn CommanderBackend>,
        events: tokio::sync::mpsc::Sender<AppEvent>,
        programs: Vec<claude_commander_core::api::ProgramInfo>,
    ) {
        let sender = self.senders.entry(id).or_insert_with(|| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<
                Vec<claude_commander_core::api::ProgramInfo>,
            >();
            tokio::spawn(async move {
                while let Some(mut latest) = rx.recv().await {
                    // Coalesce any snapshots already queued behind this one.
                    while let Ok(newer) = rx.try_recv() {
                        latest = newer;
                    }
                    if let Err(e) = backend.set_programs(latest).await {
                        let _ = events
                            .send(AppEvent::StateUpdate(
                                crate::event::StateUpdate::ServerProgramsSaveFailed {
                                    backend: id,
                                    message: e.to_string(),
                                },
                            ))
                            .await;
                    }
                }
            });
            tx
        });
        // Send failure only if the worker is gone (it lives for the app's life).
        let _ = sender.send(programs);
    }

    /// Drop the workers for backends that were removed (e.g. via config
    /// hot-reload). Dropping the sender closes the channel, so the worker's
    /// `rx.recv()` returns `None` and the task exits.
    fn forget(&mut self, ids: &[BackendId]) {
        self.senders.retain(|id, _| !ids.contains(id));
    }
}

pub struct App {
    /// Local config cache — refreshed from config_store on tick when file changes
    config: Config,
    /// Frozen snapshot of `commander_enabled`, captured at startup. The
    /// commander's enablement is restart-required: the agent-state poll task
    /// captures it at spawn, so the footer chip reads this same frozen value
    /// rather than the hot-reloaded `config.commander_enabled`. Toggling it at
    /// runtime only surfaces the restart warning; the commander UI doesn't move
    /// until the next launch.
    commander_enabled_at_init: bool,
    /// Monotonic id for the add-server connection probe in flight; results
    /// carrying a stale nonce are dropped (see `spawn_remote_server_probe`).
    probe_nonce: u64,
    /// Unified service layer — owns SessionManager, StateStore, and ConfigStore.
    ///
    /// Retained for owner-side concerns only (spawning the core background
    /// loops, tmux health check, config read/write/reload); all session and
    /// project state flows through `backends`. The local `BackendHandle`
    /// wraps a clone of this same service.
    service: CommanderService,
    /// The backends the TUI drives: the local backend first, then one entry
    /// per configured `[[remote_servers]]`. Each holds a cached
    /// [`BackendView`] the render path reads synchronously, refreshed by a
    /// per-backend change-feed task via
    /// [`StateUpdate::BackendChanged`](crate::event::StateUpdate).
    backends: Vec<BackendHandle>,
    /// Builds a remote backend from its config entry. Injected by the binary so
    /// core never links the remote client crate. Held past construction so the
    /// config hot-reload path can build backends for servers added at runtime.
    remote_factory: RemoteBackendFactory,
    /// Markdown documenting the CLI, rendered by the binary that defines it and
    /// injected here — core has no CLI of its own to describe, and no way to
    /// name the program it's embedded in. Embedded in the commander and
    /// conversation `CLAUDE.md` files so both document real invocations.
    cli_reference: String,
    /// Monotonic allocator for remote [`BackendId`]s. Ids are stable for a
    /// backend's lifetime and never reused, so an aborted feed task's in-flight
    /// message can't land on a freshly-added backend that happened to reuse its
    /// slot. Starts past the startup-assigned ids.
    next_remote_backend_id: usize,
    /// Serializes program-list writes per backend so rapid edits (e.g. holding a
    /// reorder key) can't have their PUTs arrive out of order.
    program_commit: ProgramCommitQueue,
    /// Frontend-owned UI preferences (view mode, last selection, pane width),
    /// persisted to `tui.json` — kept out of `state.json` so backend/session
    /// data and local UI prefs never share a file. See [`crate::prefs`].
    tui_prefs: crate::prefs::TuiPrefsStore,
    /// UI state
    ui_state: AppUiState,
    /// Event loop
    event_loop: EventLoop,
    /// Theme configuration
    theme: Theme,
    /// Suppress key events until this instant (filters stray bytes from
    /// unrecognized escape sequences that crossterm splits into multiple events)
    suppress_keys_until: Instant,
    /// Two-digit session number accumulator with debounce
    digit_accumulator: super::digit_accumulator::DigitAccumulator,
    /// Conversation mode runtime (headless streaming `claude` session + TTS).
    /// Lives here, not in the overlay modal, so it keeps running while closed.
    conversation: conversation::ConversationRuntime,
    /// Cached list of input (capture) devices for the STT Microphone setting,
    /// refreshed when the settings modal / mic picker opens. Lets the settings
    /// row map the persisted device id to its friendly label without enumerating
    /// the audio host on the render path (which `build_settings_rows` sits on).
    stt_input_devices: Vec<claude_commander_core::conversation::recorder::InputDevice>,
    /// Terminal graphics capability for the review image view, probed ONCE
    /// before the input reader starts (see `run`); `None` until then. Kept on
    /// `App` rather than in `DiffReviewState` because the protocol cache below
    /// isn't `Clone`, and `DiffReviewState` is.
    picker: Option<ratatui_image::picker::Picker>,
    /// Decoded review images keyed by (display path, side). Interior-mutable so
    /// the `&self` render path can read them; populated by the background fetch
    /// task via `StateUpdate::ReviewImageLoaded`. Cleared when a review opens.
    review_images: RefCell<HashMap<(String, DiffSide), review::ImageEntry>>,
    /// Monotonic generation, bumped each time a review opens (when
    /// `review_images` is cleared). A background fetch captures the generation
    /// it was spawned under; a late arrival from a since-closed review carries a
    /// stale generation and is dropped, so it can't poison the new review's
    /// cache (e.g. a same-named path in a different session).
    review_image_gen: Cell<u64>,
    /// Display paths whose working-tree content is currently being fetched for
    /// context expansion, so repeated expand triggers don't spawn duplicate
    /// fetches. Loaded results land in `DiffReviewState.file_lines`; a path is
    /// removed here once its fetch reports back (kept on failure as a negative
    /// cache). Cleared when a review opens or its diff is refreshed.
    review_file_loads: RefCell<HashSet<String>>,
    /// Monotonic generation for context-expansion file-content fetches, bumped
    /// when a review opens *and* when its diff is refreshed (both invalidate the
    /// per-file line cache). A fetch captures the generation it was spawned
    /// under; an arrival whose generation no longer matches is dropped, so a
    /// pre-refresh fetch can't install stale content against the new diff's line
    /// numbers.
    review_file_gen: Cell<u64>,
}

impl App {
    /// Create a new application. `frontend` identifies this binary for
    /// telemetry and is forwarded to [`CommanderService::new`]; `cli_reference`
    /// is markdown documenting the binary's own CLI, handed to the commander and
    /// conversation sessions.
    pub fn new(
        config_store: Arc<ConfigStore>,
        store: Arc<StateStore>,
        frontend: claude_commander_core::telemetry::FrontendInfo,
        remote_factory: RemoteBackendFactory,
        cli_reference: String,
    ) -> Self {
        let config = config_store.read().clone();
        // Build the TUI prefs store from the same data dir as the state file
        // (migrating legacy prefs out of `state.json` on first launch) before
        // the store is moved into the service.
        let tui_prefs = crate::prefs::TuiPrefsStore::load(&store.data_dir());
        let service = CommanderService::new(config_store, store, frontend);

        // The local backend wraps a clone of the same service (a bundle of
        // Arcs). During the Phase-C transition both coexist; call sites migrate
        // from `service` onto `backends` incrementally.
        let local: Arc<dyn CommanderBackend> = Arc::new(LocalBackend::new(service.clone()));
        let mut backends = vec![BackendHandle::new(LOCAL_BACKEND_ID, local)];
        // One backend per configured remote server, in declared order, starting
        // at BackendId(1). A server that fails to construct becomes a
        // permanently-degraded placeholder so it still shows in the tree.
        for (i, server) in config.remote_servers.iter().enumerate() {
            backends.push(Self::build_remote_handle(
                BackendId(i + 1),
                server,
                &remote_factory,
            ));
        }

        // The usual theme: no workspace is active until the snapshots arrive,
        // and `refresh_list_items` swaps in the active one's when it is.
        let theme = Theme::from_overrides(&config.theme);
        let debounce = Duration::from_millis(config.session_number_debounce_ms);
        let commander_enabled_at_init = config.commander_enabled;

        // Startup assigned remote ids 1..=len; the next allocation continues
        // past them so re-added servers get fresh, never-reused ids.
        let next_remote_backend_id = backends.len();

        Self {
            config,
            commander_enabled_at_init,
            probe_nonce: 0,
            service,
            backends,
            remote_factory,
            cli_reference,
            next_remote_backend_id,
            program_commit: ProgramCommitQueue::default(),
            tui_prefs,
            ui_state: AppUiState::default(),
            event_loop: EventLoop::new(),
            theme,
            suppress_keys_until: Instant::now(),
            digit_accumulator: super::digit_accumulator::DigitAccumulator::new(debounce),
            conversation: conversation::ConversationRuntime::default(),
            stt_input_devices: Vec::new(),
            picker: None,
            review_images: RefCell::new(HashMap::new()),
            review_image_gen: Cell::new(0),
            review_file_loads: RefCell::new(HashSet::new()),
            review_file_gen: Cell::new(0),
        }
    }

    /// A handle on the service backing this app, for the binary to serve over
    /// HTTP in-process.
    ///
    /// `CommanderService` is a bundle of `Arc`s, so this is a shared handle
    /// rather than a copy — which is the point. An embedded server built on a
    /// *second* service would re-run telemetry init, rebuild the derived comment
    /// and review stores, and give `state.json` a second writer inside one
    /// process; sharing this one means the TUI and its clients see the same
    /// state and the same background loops.
    pub fn service_handle(&self) -> CommanderService {
        self.service.clone()
    }

    /// Record how the in-process HTTP server fared, for the status-bar chip and
    /// the copy-token command. Called by the binary before [`Self::run`], since
    /// only the binary depends on the server crate.
    ///
    /// A failure also raises a toast, because the chip has to stay short enough
    /// not to evict the action buttons — so the chip says *that* it failed and
    /// the toast says why.
    pub fn set_embedded_server(&mut self, status: crate::EmbeddedServerStatus) {
        if let crate::EmbeddedServerStatus::Failed { reason } = &status {
            self.ui_state.status_message = Some((
                format!("Server not started: {reason}"),
                Instant::now() + Duration::from_secs(10),
            ));
        }
        self.ui_state.embedded_server = Some(status);
    }

    /// Construct the [`BackendHandle`] for one configured remote server: the
    /// real backend from the factory, or — if construction fails — a
    /// [`PlaceholderBackend`] whose view is seeded `Degraded` with the reason, so
    /// the server still appears in the tree as a permanently-errored header
    /// rather than crashing startup or silently disappearing.
    fn build_remote_handle(
        id: BackendId,
        server: &claude_commander_core::config::RemoteServerConfig,
        remote_factory: &RemoteBackendFactory,
    ) -> BackendHandle {
        // A loopback server that shares this machine's `state.json` would
        // double-render every session and misroute rows under its header to the
        // local backend. A loopback server with its OWN data dir is legitimate,
        // so this is a warn-only heuristic, not a hard block.
        if is_loopback_url(&server.url) {
            warn!(
                "Remote server '{}' points at a loopback address ({}); if it shares this \
                 machine's state.json, sessions will appear twice — give it a separate data dir",
                server.name, server.url
            );
        }
        match remote_factory(server) {
            Ok(backend) => BackendHandle::new(id, backend),
            Err(e) => {
                warn!(
                    "Remote backend '{}' failed to construct: {} — showing as degraded",
                    server.name, e
                );
                let placeholder: Arc<dyn CommanderBackend> =
                    Arc::new(PlaceholderBackend::new(server.name.clone(), e.to_string()));
                let mut handle = BackendHandle::new(id, placeholder);
                handle.view.connection = ConnectionState::Degraded {
                    reason: e.to_string(),
                };
                handle
            }
        }
    }

    /// Reconcile the live backends against a hot-reloaded `remote_servers` list:
    /// drop handles for removed/changed servers (their `Drop` aborts the polling
    /// tasks), construct handles for added/changed servers, then reorder the Vec
    /// to match config order (local stays first). Selection on a dropped backend
    /// falls back to the local backend.
    fn apply_remote_servers_reload(
        &mut self,
        old: &[claude_commander_core::config::RemoteServerConfig],
        new: &[claude_commander_core::config::RemoteServerConfig],
    ) {
        let recon = reconcile_remote_servers(old, new);

        // Remove dropped/changed backends. Capture their ids first so a
        // selection pointing at one can fall back to local.
        if !recon.removed.is_empty() {
            let removed_ids: Vec<BackendId> = self
                .backends
                .iter()
                .filter(|h| {
                    h.id != LOCAL_BACKEND_ID && recon.removed.contains(&h.backend.descriptor().name)
                })
                .map(|h| h.id)
                .collect();
            // Dropping the handle aborts its feed tasks (see BackendHandle::drop).
            self.backends
                .retain(|h| h.id == LOCAL_BACKEND_ID || !removed_ids.contains(&h.id));
            // Drop any program-commit worker for a removed backend; dropping the
            // sender closes its channel so the worker task exits cleanly.
            self.program_commit.forget(&removed_ids);

            let session_gone = self
                .ui_state
                .selected_session_id
                .is_some_and(|r| removed_ids.contains(&r.backend));
            let project_gone = self
                .ui_state
                .selected_project_id
                .is_some_and(|(b, _)| removed_ids.contains(&b));
            if session_gone {
                self.ui_state.selected_session_id = None;
            }
            if session_gone || project_gone {
                self.ui_state.selected_project_id = None;
                self.ui_state.selected_backend_connected = true;
                self.ui_state.selected_backend_capabilities = BackendCapabilities::LOCAL;
            }
        }

        // Construct and wire added/changed backends.
        for server in &recon.added {
            let id = BackendId(self.next_remote_backend_id);
            self.next_remote_backend_id += 1;
            let mut handle = Self::build_remote_handle(id, server, &self.remote_factory);
            handle.feed_tasks = self.spawn_backend_feeds_for(&handle);
            self.backends.push(handle);
        }

        // Reorder so the tree shows local first, then remotes in config order.
        let position = |name: &str| new.iter().position(|s| s.name == name);
        self.backends.sort_by_key(|h| {
            if h.id == LOCAL_BACKEND_ID {
                (0usize, 0usize)
            } else {
                (
                    1,
                    position(&h.backend.descriptor().name).unwrap_or(usize::MAX),
                )
            }
        });
    }

    // -- Backend accessors (Phase C) --

    /// The backend handle for `id`, if present.
    /// Test-only mutable view access, for seeding backend snapshots directly.
    #[cfg(test)]
    pub(super) fn backend_mut_for_test(&mut self, id: BackendId) -> &mut BackendHandle {
        self.backends.iter_mut().find(|h| h.id == id).unwrap()
    }

    pub(super) fn backend(&self, id: BackendId) -> Option<&BackendHandle> {
        self.backends.iter().find(|h| h.id == id)
    }

    /// The backend trait object for `id`, cloneable into a spawned task. Falls
    /// back to the local backend when the id is unknown (e.g. its server was
    /// just removed) so callers never panic on a stale id.
    pub(super) fn backend_arc(&self, id: BackendId) -> Arc<dyn CommanderBackend> {
        self.backend(id)
            .map(|h| h.backend.clone())
            .unwrap_or_else(|| self.local_arc())
    }

    /// The backend trait object that owns session ref `r`.
    pub(super) fn backend_for(&self, r: SessionRef) -> Arc<dyn CommanderBackend> {
        self.backend_arc(r.backend)
    }

    /// The cached view of the backend that owns `r` (its snapshot + connection),
    /// or the local view if the backend id is unknown.
    pub(super) fn view_for(&self, backend: BackendId) -> &BackendView {
        self.backend(backend)
            .map(|h| &h.view)
            .unwrap_or_else(|| self.local_view())
    }

    /// The local backend's cached view (single-backend convenience this phase).
    pub(super) fn local_view(&self) -> &BackendView {
        &self.backends[0].view
    }

    /// The local backend trait object, cloneable into a spawned task.
    pub(super) fn local_arc(&self) -> Arc<dyn CommanderBackend> {
        self.backends[0].backend.clone()
    }

    /// Record a UI-only telemetry feature against the local backend.
    pub(super) fn record_feature(&self, feature: &'static str) {
        self.backends[0].backend.record_feature(feature);
    }

    /// Fetch a fresh snapshot + agent states into the local backend's cached
    /// view. Call after a user-initiated backend mutation so the very next
    /// `refresh_list_items` reflects it, rather than waiting a change-feed cycle
    /// (the change-feed task still delivers a redundant, idempotent refresh).
    pub(super) async fn refresh_local_view(&mut self) {
        self.refresh_backend_view(LOCAL_BACKEND_ID).await;
    }

    /// Fetch a fresh snapshot + agent states into `id`'s cached view, so the
    /// very next `refresh_list_items` reflects a just-issued mutation without
    /// waiting a change-feed cycle. A failed fetch leaves the cached view (and
    /// its connection state) untouched, so a degraded remote keeps its last
    /// snapshot rather than blanking.
    pub(super) async fn refresh_backend_view(&mut self, id: BackendId) {
        let backend = self.backend_arc(id);
        let snapshot = backend.snapshot().await;
        let states = backend.agent_states(false).await;
        if let Some(handle) = self.backends.iter_mut().find(|h| h.id == id) {
            if let Ok(snapshot) = snapshot {
                handle.view.snapshot = snapshot;
                // Local: derive connection from the snapshot's tmux health;
                // remote: leave it to the connection-watch task (returns None).
                if let Some(conn) =
                    connection_from_snapshot(id == LOCAL_BACKEND_ID, &handle.view.snapshot)
                {
                    handle.view.connection = conn;
                }
            }
            if let Ok(states) = states {
                handle.view.agent_states = states;
            }
        }
    }

    /// Test-only: fold the current store state into the local backend's cached
    /// view. Production populates the view via `bootstrap_backend_views` and the
    /// change-feed task; tests that seed the store directly (bypassing backend
    /// mutations, so no change-feed bump fires) call this before asserting on
    /// the rendered tree.
    #[cfg(test)]
    pub(super) async fn sync_local_view_from_store_for_test(&mut self) {
        // Read back through the backend (which wraps the same store the test
        // seeded) rather than the store directly, so this stays clear of the
        // Phase-C store-access gate.
        if let Ok(snapshot) = self.local_arc().snapshot().await {
            self.backends[0].view.snapshot = snapshot;
            self.backends[0].view.connection =
                claude_commander_core::backend::ConnectionState::Connected;
        }
        // Mirror any test-injected agent states (set on `ui_state.agent_states`
        // directly, since tmux isn't live in tests) into the local backend view,
        // which is what `refresh_list_items` reads per-backend.
        self.backends[0].view.agent_states.states = self.ui_state.agent_states.clone();
    }

    /// Look up a session in a backend's cached snapshot.
    pub(super) fn session(
        &self,
        r: SessionRef,
    ) -> Option<&claude_commander_core::api::SessionInfo> {
        self.backend(r.backend)?
            .view
            .snapshot
            .sessions
            .iter()
            .find(|s| s.session_id == r.id)
    }

    /// Look up a project across every backend's cached snapshot. Project ids are
    /// globally unique, so the first match is the only one — which lets callers
    /// that hold a bare `ProjectId` (the Info surfaces) skip the qualifier.
    pub(super) fn project(
        &self,
        id: ProjectId,
    ) -> Option<&claude_commander_core::api::ProjectInfo> {
        self.backends
            .iter()
            .find_map(|h| h.view.snapshot.projects.iter().find(|p| p.id == id))
    }

    /// The local backend concretely, for the local-only affordances the trait
    /// deliberately omits (name-based attach, shell-toggle resolution, the
    /// commander session). `None` if backend 0 isn't a [`LocalBackend`] — never
    /// today, since the local backend is always present.
    pub(super) fn local_backend(&self) -> Option<&LocalBackend> {
        self.backends[LOCAL_BACKEND_ID.0]
            .backend
            .as_any()
            .downcast_ref::<LocalBackend>()
    }

    /// The backend that owns an [`AttachTarget`]: a session ref's own backend,
    /// or the local backend for a name-only target (the commander / a project
    /// shell, which are local-only affordances).
    pub(super) fn attach_target_backend(&self, target: &AttachTarget) -> BackendId {
        match target {
            AttachTarget::Session { session, .. } => session.backend,
            AttachTarget::LocalName(_) => LOCAL_BACKEND_ID,
        }
    }

    /// The tmux session name an [`AttachTarget`] resolves to, for the in-session
    /// switcher, MRU/viewed tracking, and post-attach focus. `None` when a
    /// session ref isn't in the cached snapshot.
    pub(super) fn attach_target_name(&self, target: &AttachTarget) -> Option<String> {
        match target {
            AttachTarget::LocalName(name) => Some(name.clone()),
            AttachTarget::Session { session, kind } => {
                let base = self.session(*session)?.tmux_session_name.clone();
                Some(match kind {
                    AttachKind::Agent => base,
                    AttachKind::Shell => format!("{base}-sh"),
                })
            }
        }
    }

    /// Map a tmux session name back to a local [`AttachTarget`] — used after the
    /// in-session switcher lands on an arbitrary session by name. A `-sh`
    /// suffix maps to the session's shell pane; a name that matches no session
    /// (the commander, say) stays a [`AttachTarget::LocalName`].
    pub(super) fn attach_target_from_name(&self, name: &str) -> AttachTarget {
        let is_shell = name.ends_with("-sh");
        let base = name.strip_suffix("-sh").unwrap_or(name);
        match self
            .local_view()
            .snapshot
            .sessions
            .iter()
            .find(|s| s.tmux_session_name == base)
        {
            Some(s) => AttachTarget::Session {
                session: SessionRef::local(s.session_id),
                kind: if is_shell {
                    AttachKind::Shell
                } else {
                    AttachKind::Agent
                },
            },
            None => AttachTarget::LocalName(name.to_string()),
        }
    }

    /// Describe the pane an [`AttachTarget`] puts on screen, for the dictation
    /// submit policy: which half of the session it is, and which harness runs
    /// there.
    ///
    /// The harness comes from the session's configured `program` in the owning
    /// backend's cached snapshot, so it is right for a remote session too. A ref
    /// that isn't in the snapshot yields
    /// [`AgentKind::Unknown`](claude_commander_core::agent::AgentKind::Unknown),
    /// which costs only the per-harness submit delay — the `kind` the policy
    /// actually gates on is carried by the target itself.
    ///
    /// A name-only target is the commander or a project shell: a shell with no
    /// session behind it and so no harness to name. Under the `agent` policy
    /// that is correctly never submitted into; under `always` it is submitted
    /// into, which is what that setting asks for. (The commander pane is
    /// arguably an agent — a follow-up, since it has no `SessionInfo` to read a
    /// program from.)
    pub(super) fn pane_info_for(&self, target: &AttachTarget) -> PaneInfo {
        match target {
            AttachTarget::Session { session, kind } => PaneInfo {
                kind: *kind,
                agent: self
                    .session(*session)
                    .map(|s| claude_commander_core::agent::AgentKind::from_program(&s.program))
                    .unwrap_or(claude_commander_core::agent::AgentKind::Unknown),
            },
            AttachTarget::LocalName(_) => PaneInfo {
                kind: AttachKind::Shell,
                agent: claude_commander_core::agent::AgentKind::Unknown,
            },
        }
    }

    /// The session id a tmux session name belongs to (agent or `-sh` shell),
    /// preferring `backend`'s cached snapshot before scanning the rest. The
    /// attached backend takes precedence because tmux session names can collide
    /// across machines. `None` for the commander / an unknown name.
    pub(super) fn session_id_by_tmux_name(
        &self,
        backend: BackendId,
        name: &str,
    ) -> Option<SessionId> {
        let base = name.strip_suffix("-sh").unwrap_or(name);
        let find_in = |view: &BackendView| {
            view.snapshot
                .sessions
                .iter()
                .find(|s| s.tmux_session_name == base)
                .map(|s| s.session_id)
        };
        find_in(self.view_for(backend))
            .or_else(|| self.backends.iter().find_map(|h| find_in(&h.view)))
    }

    /// Open an attach connection for `target`: through the owning backend for a
    /// session pane (which stamps last-attached and revives a dead tmux
    /// session), or via the local name-based path for the commander / project
    /// shell.
    pub(super) async fn open_attach(
        &self,
        target: &AttachTarget,
        cols: u16,
        rows: u16,
    ) -> BResult<Box<dyn AttachConnection>> {
        match target {
            AttachTarget::Session { session, kind } => {
                self.backend(session.backend)
                    .ok_or(BackendError::NotFound)?
                    .backend
                    .attach(session.id, cols, rows, *kind)
                    .await
            }
            AttachTarget::LocalName(name) => {
                self.local_backend()
                    .ok_or(BackendError::NotFound)?
                    .attach_by_tmux_name(name, cols, rows)
                    .await
            }
        }
    }

    /// Resolve the Ctrl+\ shell-toggle partner for `current`. A session pane
    /// simply flips Agent↔Shell on the same id — `backend.attach(id, Shell)`
    /// creates the `-sh` pair on demand, so no name resolution is needed. A
    /// name-only target (a project shell, or a switcher-picked session) defers
    /// to the local backend's name-based resolver.
    pub(super) async fn toggle_shell_target(
        &self,
        current: &AttachTarget,
        current_name: &str,
    ) -> BResult<AttachTarget> {
        match current {
            AttachTarget::Session { session, kind } => Ok(AttachTarget::Session {
                session: *session,
                kind: match kind {
                    AttachKind::Agent => AttachKind::Shell,
                    AttachKind::Shell => AttachKind::Agent,
                },
            }),
            AttachTarget::LocalName(name) => {
                let be = self.local_backend().ok_or(BackendError::NotFound)?;
                // Prefer the live name reported by the attach (post-switcher).
                let base = if current_name.is_empty() {
                    name.as_str()
                } else {
                    current_name
                };
                let paired = be.resolve_shell_toggle_pair(base).await?;
                Ok(self.attach_target_from_name(&paired))
            }
        }
    }

    /// Fetch an initial snapshot + agent states for every backend, so the first
    /// list refresh reads real data rather than the empty `connecting` view.
    /// A backend that errors is left `Connecting` (its change-feed task will
    /// retry) and marked `Degraded` so its header shows the reason.
    async fn bootstrap_backend_views(&mut self) {
        for handle in &mut self.backends {
            // Only the local backend is fetched synchronously: it's in-process
            // and effectively instant. Remote backends must NOT block first
            // draw — an unreachable server would stall startup for its full
            // connect/request timeout per server. Their headers render as
            // Connecting, and the poller's first successful poll bumps the
            // change feed, which delivers the initial snapshot (or flips the
            // header to Degraded) moments later.
            if handle.backend.descriptor().kind
                != claude_commander_core::backend::BackendKind::Local
            {
                continue;
            }
            let snapshot = handle.backend.snapshot().await;
            let states = handle.backend.agent_states(false).await;
            match (snapshot, states) {
                (Ok(snapshot), Ok(states)) => {
                    handle.view.snapshot = snapshot;
                    handle.view.agent_states = states;
                    // Only the local backend reaches this arm (remotes `continue`
                    // above). Its connection derives from the snapshot's tmux
                    // health — the single source of truth also used by every
                    // subsequent fold — so a tmux-down startup shows Degraded here
                    // rather than being overridden separately in `run()`.
                    let is_local = handle.id == LOCAL_BACKEND_ID;
                    if let Some(conn) = connection_from_snapshot(is_local, &handle.view.snapshot) {
                        handle.view.connection = conn;
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    warn!("Initial snapshot for backend {:?} failed: {}", handle.id, e);
                    handle.view.connection =
                        claude_commander_core::backend::ConnectionState::Degraded {
                            reason: e.to_string(),
                        };
                }
            }
        }
    }

    /// Spawn one task per backend that awaits its change feed and, on each bump,
    /// fetches a fresh snapshot + agent states and forwards them as
    /// [`StateUpdate::BackendChanged`] over the event channel. The task exits
    /// when the feed's sender is dropped (backend gone).
    fn spawn_backend_change_feeds(&mut self) {
        for i in 0..self.backends.len() {
            let tasks = self.spawn_backend_feeds_for(&self.backends[i]);
            self.backends[i].feed_tasks = tasks;
        }
    }

    /// Spawn the change-feed and (if the backend exposes one) connection-watch
    /// tasks for a single backend handle, returning their [`JoinHandle`]s so the
    /// handle can own (and, on drop, abort) them. Used at startup for every
    /// backend and on config hot-reload for a newly-added remote backend.
    fn spawn_backend_feeds_for(&self, handle: &BackendHandle) -> Vec<tokio::task::JoinHandle<()>> {
        let mut tasks = Vec::new();

        // Connection-watch task: forward the backend's health changes into the
        // cached view so the server header re-renders live. Backends with a
        // fixed health (local, placeholder) return `None` and spawn nothing.
        if let Some(mut conn) = handle.backend.connection_watch() {
            let backend_id = handle.id.0;
            let tx = self.event_loop.sender();
            tasks.push(tokio::spawn(async move {
                loop {
                    let state = conn.borrow().clone();
                    if tx
                        .send(AppEvent::StateUpdate(StateUpdate::BackendConnection {
                            backend_id,
                            state,
                        }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if conn.changed().await.is_err() {
                        break;
                    }
                }
            }));
        }

        {
            let backend_id = handle.id.0;
            let backend = handle.backend.clone();
            let mut feed = backend.change_feed();
            let tx = self.event_loop.sender();
            tasks.push(tokio::spawn(async move {
                while feed.changed().await {
                    if !fetch_and_send_backend_change(backend_id, backend.clone(), tx.clone()).await
                    {
                        break;
                    }
                }
            }));
        }

        tasks
    }

    /// Force a snapshot + agent-state refresh of backend `id` off the event
    /// loop, delivered via [`StateUpdate::BackendChanged`] (which folds it in and
    /// re-renders). Use after a user-initiated mutation so the tree reflects it
    /// promptly without awaiting two remote GETs on the render path — the
    /// change-feed task would eventually deliver an identical, idempotent
    /// update, but this forces it now rather than a poll cycle later.
    pub(super) fn spawn_backend_view_refresh(&self, id: BackendId) {
        let backend = self.backend_arc(id);
        let tx = self.event_loop.sender();
        tokio::spawn(fetch_and_send_backend_change(id.0, backend, tx));
    }

    /// Run startup program discovery and refresh the cached config with what
    /// it added. The refresh is not optional: the write records the file's new
    /// mtime, so the hot-reload never fires for it, and without it the picker
    /// would not offer the new entries — and the next settings save, which
    /// writes this cached copy back whole, would delete them from disk.
    fn discover_programs_on(&mut self, path: &std::ffi::OsStr) {
        match self.service.discover_programs_on(path) {
            Ok(added) if !added.is_empty() => self.config = self.service.read_config(),
            Ok(_) => {}
            Err(e) => warn!("Program discovery failed: {e}"),
        }
    }

    /// Run the application
    pub async fn run(&mut self) -> Result<()> {
        // tmux drives every *local* session, but a remote-only operator can work
        // entirely against configured servers. So a failed local tmux probe only
        // aborts when there are no remotes; otherwise we degrade the local
        // backend and carry on. The Degraded header isn't set here: it's derived
        // from the local snapshot's `server.tmux_ok` in `bootstrap_backend_views`
        // (and every later fold), the single source of truth — so a tmux-down
        // startup shows Degraded without a second override that folds would
        // fight.
        let tmux_result = self.service.check_tmux().await;
        let has_remotes = !self.config.remote_servers.is_empty();
        match tmux_startup_decision(
            tmux_result.as_ref().err().map(|e| e.to_string()),
            has_remotes,
        ) {
            TmuxStartup::Proceed => {}
            TmuxStartup::DegradeLocal(reason) => {
                warn!("tmux unavailable; running remote-only ({reason})");
            }
            // Guaranteed `Err` here — propagate the original tmux error.
            TmuxStartup::Abort(_) => return tmux_result.map_err(TuiError::Core),
        }

        // One-time setup: reconcile each backend (drop stale Creating sessions,
        // reset transient stack states, sync status against live tmux, re-run
        // section assignment). The local backend runs it in-process; a remote
        // backend's server reconciles itself (default no-op).
        for handle in &self.backends {
            if let Err(e) = handle.backend.startup_reconcile().await {
                warn!(
                    "Startup reconciliation for backend {:?} failed: {}",
                    handle.id, e
                );
            }
        }

        // Populate each backend's cached view with an initial snapshot before
        // the first list refresh, then start a per-backend change-feed task so
        // subsequent store/remote changes fold in off the render path.
        self.bootstrap_backend_views().await;
        self.spawn_backend_change_feeds();

        // Start the idle-hibernation loop now we're in the long-lived TUI
        // runtime (no-op unless enabled in config). Deliberately not started in
        // CommanderService::new so one-shot CLI commands never trigger it.
        self.service.start_hibernation_loop();

        // Cache gh availability for the enriched-PR info fetch (the PR-status
        // loop gates on its own cached probe inside the service).
        if self.config.pr_check_interval_secs > 0 {
            self.ui_state.gh_available = is_gh_available().await;
        }

        // Probe terminal graphics capability ONCE, here — BEFORE the background
        // input reader starts below. `from_query_stdio` writes DA/DSR escape
        // queries and reads the replies from stdin; if the reader were already
        // running it would steal those replies, time out, and (since ratatui
        // 0.30) crash the loop. It manages its own raw mode for the query. On
        // any failure (non-tty, unsupported terminal) we fall back to Unicode
        // half-blocks, which render on any truecolor terminal.
        let mut picker = ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
        let term_program = std::env::var("TERM_PROGRAM").ok();
        let lc_terminal = std::env::var("LC_TERMINAL").ok();
        if let Some(proto) = iterm2_kitty_override(
            picker.protocol_type(),
            term_program.as_deref(),
            lc_terminal.as_deref(),
        ) {
            debug!(?proto, "iTerm2 detected: overriding Kitty probe result");
            picker.set_protocol_type(proto);
        }
        self.picker = Some(picker);

        // Floor at 1: a hand-edited config with fps 0 must not divide by zero.
        let tick_rate = Duration::from_millis(1000 / self.config.ui_refresh_fps.max(1) as u64);
        self.event_loop.start(tick_rate);

        // Warm the syntax-highlight assets in the background so the first review
        // open doesn't pay the one-time syntax-set load on its critical path.
        tokio::spawn(async {
            let _ = tokio::task::spawn_blocking(crate::syntax_highlight::warm_assets).await;
        });

        // Start the service-owned background loops (agent-state polling,
        // PR-status checks, project auto-pull, cross-instance state-sync). They
        // run inside the service and reach the TUI as fresh snapshots via the
        // backend change feed; a remote backend's server runs its own loops, so
        // this local spawn is idempotent and no-ops if already started.
        let _background =
            self.service
                .spawn_background_tasks(claude_commander_core::api::BackgroundOpts {
                    commander_enabled: self.commander_enabled_at_init,
                });

        // List any recognised agent harness installed on this host that the
        // program picker doesn't offer yet. A remote backend's server does the
        // same against its own host.
        self.discover_programs_on(&std::env::var_os("PATH").unwrap_or_default());

        // Restore the last-selected view (before the first list build so the
        // correct structure — list vs board — is populated up front). If the
        // user never chose one, fall back to the section-aware default:
        // SectionStacks when sections are configured, else ProjectGrouped. Any
        // section view falls back to ProjectGrouped at refresh time if sections
        // have since been removed from config.
        let persisted_view = self.tui_prefs.prefs().view_mode;
        self.ui_state.view_mode = match persisted_view {
            Some(view) => view,
            None if !self.config.sections.is_empty() => ViewMode::SectionStacks,
            None => ViewMode::ProjectGrouped,
        };

        // Restore the list views' pane split. Clamped on the way in so a
        // hand-edited or older out-of-range `tui.json` can't render an unusable
        // (or zero-width) pane.
        if let Some(pct) = self.tui_prefs.prefs().left_pane_pct {
            self.ui_state.left_pane_pct = pct.clamp(MIN_LEFT_PANE_PCT, MAX_LEFT_PANE_PCT);
        }

        // Pick the workspace to open on (`startup_workspace`, else the one
        // `tui.json` remembers) before the first build, so the views are
        // scoped from the first frame.
        self.apply_startup_workspace();

        // Restore last selection from persisted state
        self.refresh_list_items().await;
        self.restore_selection().await;

        // Surface any pending review comments left from a previous run.
        self.refresh_comment_indicators();

        // Bring the mic listener up eagerly (when STT is enabled) so a global
        // hotkey can toggle recording even before the conversation is opened.
        // The heavy headless session stays lazy — the listener spawns it on the
        // first transcript. Then start the external IPC trigger(s).
        if self.config.stt.enabled {
            self.ensure_listener_started().await;
            self.spawn_listen_ipc();
        }

        loop {
            // Setup terminal for TUI
            let mut terminal = self.setup_terminal()?;
            self.refresh_list_items().await;

            // Run main loop until quit or attach
            info!("Entering main loop");
            let result = self.main_loop(&mut terminal).await;
            match &result {
                Ok(()) => info!("Main loop exited cleanly"),
                Err(e) => error!("Main loop exited with error: {e:?}"),
            }

            // Restore terminal before propagating, attaching, or exiting — a
            // raw-mode/alternate-screen terminal must never outlive the loop,
            // even on the error path below.
            info!("Restoring terminal");
            self.restore_terminal(&mut terminal)?;
            info!("Terminal restored successfully");

            // A main-loop error is fatal: propagate it so `main` exits
            // non-zero and color-eyre reports it on stderr, rather than
            // silently dropping into the quit path. Restore has already run.
            result?;

            // Reset should_quit for next iteration
            self.ui_state.should_quit = false;

            if let Some((editor, path)) = self.ui_state.editor_command.take() {
                // Run editor as a foreground process, then return to TUI. The
                // editor reads the terminal itself, so wait for our reader to
                // let go of it first.
                self.event_loop.stop_input().await;

                info!("Launching editor: {} {}", editor, path.display());
                let status = std::process::Command::new(&editor).arg(&path).status();

                if let Err(e) = status {
                    self.ui_state.modal = Modal::Error {
                        message: format!("Failed to launch '{}': {}", editor, e),
                    };
                }

                self.event_loop.restart_input();
            } else {
                match self.ui_state.attach_request.take() {
                    Some(request) => {
                        // Stop the input reader BEFORE attaching so it doesn't
                        // compete for the terminal — `stop_input` resolves only
                        // once the reader thread has exited — then flush the key
                        // that triggered this attach.
                        self.event_loop.stop_input().await;
                        claude_commander_core::tmux::flush_stdin();

                        // Pre-warm the conversation runtime so voice input
                        // (Alt-V) works immediately while attached. Idempotent.
                        if self.config.stt.enabled {
                            self.ensure_conversation_started().await;
                        }

                        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                        // Track every session viewed during this attach (via the
                        // switcher or shell toggle) so we can refresh just their
                        // agent state on the way out — see the post-loop block.
                        let mut viewed: HashSet<String> = HashSet::new();
                        let mut current = request;
                        let mut consecutive_ends: u8 = 0;
                        // The tmux name the user ends on (possibly reached via
                        // the in-session switcher), for post-attach focus.
                        let mut final_name: Option<String> = self.attach_target_name(&current);

                        loop {
                            let name = self.attach_target_name(&current);
                            if let Some(n) = &name {
                                viewed.insert(n.clone());
                            }

                            // For a remote agent pane, hand the attach loop a sink
                            // that uploads a captured clipboard image via the
                            // owning backend (the remote agent can't read the
                            // operator's clipboard on Ctrl+V). Only the agent pane
                            // (where Claude runs) and only backends that advertise
                            // `client_side_image_paste` (i.e. remote) get one.
                            let image_paste: Option<
                                Arc<dyn claude_commander_core::tmux::ImagePasteSink>,
                            > = match &current {
                                AttachTarget::Session {
                                    session,
                                    kind: AttachKind::Agent,
                                } => self.backend(session.backend).and_then(|h| {
                                    h.backend.capabilities().client_side_image_paste.then(|| {
                                        claude_commander_core::tmux::BackendImagePaste::sink(
                                            h.backend.clone(),
                                            session.id,
                                        )
                                    })
                                }),
                                _ => None,
                            };

                            // Open the connection first (the backend revives a
                            // dead tmux session and stamps last-attached); a
                            // failure surfaces as an error modal.
                            let conn = match self.open_attach(&current, cols, rows).await {
                                Ok(c) => c,
                                Err(e) => {
                                    warn!("Failed to attach: {e}");
                                    self.ui_state.modal = Modal::Error {
                                        message: format!("Failed to attach: {e}"),
                                    };
                                    break;
                                }
                            };
                            let streams = conn.split();

                            // Only intercept Ctrl+Z / Alt-r / Alt-V for Claude
                            // (non-shell) panes: SIGTSTP would freeze a shell-
                            // less pane, a shell's Ctrl-r must not be shadowed,
                            // and talking *to* the assistant is a thing you do
                            // from an agent pane.
                            //
                            // Dictation is the exception, and deliberately: it
                            // types into whatever is on screen, so a shell pane
                            // is a first-class destination for it. Alt-T and the
                            // listener behind it are therefore gated on `stt`
                            // alone, not on `is_shell` — otherwise the one place
                            // dictation is most obviously useful (typing a long
                            // command) would be the one place it didn't work.
                            let is_shell = matches!(
                                current,
                                AttachTarget::Session {
                                    kind: AttachKind::Shell,
                                    ..
                                }
                            ) || name.as_deref().is_some_and(|n| n.ends_with("-sh"));
                            let intercept_ctrl_z = !is_shell;
                            let editor_triggers =
                                claude_commander_core::config::keybindings::editor_trigger_bytes(
                                    &self.config.keybindings,
                                );
                            let review_triggers = if intercept_ctrl_z {
                                claude_commander_core::config::keybindings::review_trigger_bytes(
                                    &self.config.keybindings,
                                )
                            } else {
                                Vec::new()
                            };
                            let voice_triggers = if intercept_ctrl_z && self.config.stt.enabled {
                                claude_commander_core::config::keybindings::voice_trigger_bytes(
                                    &self.config.keybindings,
                                )
                            } else {
                                Vec::new()
                            };
                            let dictation_triggers = if self.config.stt.enabled {
                                claude_commander_core::config::keybindings::dictation_trigger_bytes(
                                    &self.config.keybindings,
                                )
                            } else {
                                Vec::new()
                            };
                            // One listener serves both triggers, so it comes up
                            // whenever either of them can fire — which, since
                            // dictation works on a shell pane, means whenever
                            // STT is on at all.
                            let voice_listener = self
                                .config
                                .stt
                                .enabled
                                .then(|| self.conversation.listener.clone());

                            let cfg = claude_commander_core::tmux::AttachConfig {
                                editor_triggers,
                                review_triggers,
                                voice_triggers,
                                dictation_triggers,
                                voice_listener,
                                recording: self.conversation.recording.clone(),
                                intercept_ctrl_z,
                                // The TUI draws the switcher itself, over the
                                // pane, so it works on every backend — unlike
                                // the `tmux display-popup` picker this replaced,
                                // which could only ever reach the operator's own
                                // tmux server.
                                switcher_enabled: true,
                                session_name: name.clone(),
                                image_paste,
                                // The frontend's one long-lived injector: this
                                // attach installs its own channel into it and
                                // drops it on the way out, so the transcript
                                // consumer never has to be told an attach began
                                // or ended.
                                injector: Some(self.conversation.injector.clone()),
                            };

                            // Record what the client is about to show, so a
                            // transcript that arrives during this attach is
                            // planned against the right pane. The switcher
                            // updates it again whenever it moves the client in
                            // place.
                            self.conversation
                                .injector
                                .set_pane(self.pane_info_for(&current));

                            let outcome = match self.drive_attach(streams, cfg).await {
                                Ok(o) => o,
                                Err(e) => {
                                    warn!("Attach loop failed: {e}");
                                    self.ui_state.modal = Modal::Error {
                                        message: format!("Failed to attach: {e}"),
                                    };
                                    break;
                                }
                            };

                            // The switcher may have run `tmux switch-client`
                            // mid-attach, landing on a different session than we
                            // entered. Trust the reported final session and
                            // re-map it to a target.
                            let landed = outcome.final_session.clone();
                            if !landed.is_empty() {
                                viewed.insert(landed.clone());
                                if name.as_deref() != Some(landed.as_str()) {
                                    current = self.attach_target_from_name(&landed);
                                }
                                final_name = Some(landed.clone());
                            } else {
                                final_name = self.attach_target_name(&current);
                            }

                            match outcome.result {
                                claude_commander_core::tmux::AttachResult::SwitchToShell => {
                                    current =
                                        match self.toggle_shell_target(&current, &landed).await {
                                            Ok(t) => t,
                                            Err(e) => {
                                                warn!("Failed to resolve shell session: {e}");
                                                self.ui_state.modal = Modal::Error {
                                                    message: format!("Cannot switch to shell: {e}"),
                                                };
                                                break;
                                            }
                                        };
                                    claude_commander_core::tmux::flush_stdin();
                                    consecutive_ends = 0;
                                    continue;
                                }
                                claude_commander_core::tmux::AttachResult::OpenSwitcher => {
                                    // The overlay resolved to something it could
                                    // not do in place. A staged target means the
                                    // pick lives on another backend (or the
                                    // `switch-client` failed), so hop straight
                                    // there instead of dumping the user in the
                                    // tree; no target means a command ran and
                                    // wants the full TUI.
                                    match self.ui_state.pending_switcher_target.take() {
                                        Some(target) => {
                                            current = target;
                                            claude_commander_core::tmux::flush_stdin();
                                            consecutive_ends = 0;
                                            continue;
                                        }
                                        None => break,
                                    }
                                }
                                claude_commander_core::tmux::AttachResult::SwitchToReview => {
                                    // Queue the review view; opened below once
                                    // we're back in the TUI. Resolve against the
                                    // attached backend so a remote attach opens
                                    // the right session's review.
                                    let attached = self.attach_target_backend(&current);
                                    self.ui_state.pending_open_review =
                                        self.session_id_by_tmux_name(attached, &landed);
                                    break;
                                }
                                claude_commander_core::tmux::AttachResult::OpenEditor => {
                                    // Launch the operator's editor on the session
                                    // worktree, then re-attach. Only meaningful for
                                    // a backend that can drive the local editor
                                    // (there's no local worktree for a remote one).
                                    let can_edit = self
                                        .backend(self.attach_target_backend(&current))
                                        .is_some_and(|h| h.backend.capabilities().open_editor);
                                    if can_edit {
                                        self.open_editor_for_tmux_session(&landed).await;
                                    }
                                    claude_commander_core::tmux::flush_stdin();
                                    consecutive_ends = 0;
                                    continue;
                                }
                                claude_commander_core::tmux::AttachResult::SessionEnded => {
                                    let auto = self.attach_target_name(&current).is_some_and(|n| {
                                        should_auto_restart_ended(&n, consecutive_ends)
                                    });
                                    if let (true, AttachTarget::Session { session, .. }) =
                                        (auto, &current)
                                    {
                                        consecutive_ends += 1;
                                        match self
                                            .backend_arc(session.backend)
                                            .restart_session_fresh(session.id)
                                            .await
                                        {
                                            Ok(()) => {
                                                info!(
                                                    "Auto-restarted session fresh (attempt {consecutive_ends})"
                                                );
                                                claude_commander_core::tmux::flush_stdin();
                                                continue;
                                            }
                                            Err(e) => {
                                                warn!("Failed to auto-restart session: {e}");
                                                break;
                                            }
                                        }
                                    } else {
                                        break;
                                    }
                                }
                                other => {
                                    // A transport error (e.g. a remote WS drop)
                                    // must toast rather than silently return to
                                    // the tree; a clean detach just ends quietly.
                                    if let Some(msg) = attach_end_toast(&other) {
                                        warn!("{msg}");
                                        self.ui_state.status_message =
                                            Some((msg, Instant::now() + Duration::from_secs(4)));
                                    } else {
                                        info!("Attach ended: {other:?}");
                                    }
                                    break;
                                }
                            }
                        }

                        // Flush stdin again after detach to discard stale input,
                        // then restart the input reader (also draining any
                        // AgentStatesUpdated queued while attached).
                        claude_commander_core::tmux::flush_stdin();
                        info!("Returned from attach, restarting input reader");
                        self.event_loop.restart_input();

                        // Refresh agent state for just the sessions we viewed, via
                        // the *attached* session's backend, applying the fresh
                        // states directly. We do NOT clear the whole map: that
                        // would blank every spinner until the next poll.
                        // `agent_states(true)` also advances the service loop's
                        // shared baseline to these observed states, so the loop
                        // won't re-flag a just-finished session on its next tick.
                        let attached_backend = self.attach_target_backend(&current);
                        let viewed_ids: HashSet<SessionId> = self
                            .view_for(attached_backend)
                            .snapshot
                            .sessions
                            .iter()
                            .filter(|s| {
                                viewed.iter().any(|n| {
                                    s.tmux_session_name == n.strip_suffix("-sh").unwrap_or(n)
                                })
                            })
                            .map(|s| s.session_id)
                            .collect();
                        if !viewed_ids.is_empty() {
                            // The service loop runs during the attach and may have
                            // flagged a watched session unread when it went idle.
                            // Clear unread for everything we actually saw — the
                            // operator watched those turns finish.
                            let backend = self.backend_arc(attached_backend);
                            for id in &viewed_ids {
                                let _ = backend.mark_read(*id).await;
                            }
                            if let Ok(fresh) = backend.agent_states(true).await {
                                let refreshed: BTreeMap<SessionId, AgentState> = fresh
                                    .states
                                    .into_iter()
                                    .filter(|(id, _)| viewed_ids.contains(id))
                                    .collect();
                                // Fold into the attached backend's cached view —
                                // the tree reads agent state per-backend from there.
                                if let Some(handle) =
                                    self.backends.iter_mut().find(|h| h.id == attached_backend)
                                {
                                    state::apply_viewed_session_refresh(
                                        &mut handle.view.agent_states.states,
                                        refreshed.clone(),
                                    );
                                }
                                // The local rendered map also feeds local-only
                                // consumers (commander chip, review-transition
                                // detection), so keep it in sync for a local attach.
                                if attached_backend == LOCAL_BACKEND_ID {
                                    state::apply_viewed_session_refresh(
                                        &mut self.ui_state.agent_states,
                                        refreshed,
                                    );
                                }
                            }
                            self.refresh_list_items().await;
                        }

                        // Focus the session the user just left so the tree lands
                        // on it (important after the in-session switcher).
                        if let Some(name) = final_name {
                            self.focus_session_in_tree(attached_backend, &name).await;
                        }

                        // Alt-r inside the attached session queued its review
                        // diff — open it now so the next frame shows it.
                        if let Some(sid) = self.ui_state.pending_open_review.take() {
                            let backend = self.backend_of_session(sid);
                            self.ui_state.selected_session_id = Some(SessionRef::new(backend, sid));
                            self.handle_open_review().await;
                        }
                    }
                    None => {
                        // Save selection before quitting
                        self.save_selection().await;
                        // Flush any queued telemetry before exit so the last
                        // session's events aren't lost to the flush interval.
                        self.local_arc().flush_telemetry().await;
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Setup terminal for TUI mode
    fn setup_terminal(&self) -> Result<Terminal<CrosstermBackend<Stdout>>> {
        enable_raw_mode().map_err(|e| TuiError::InitFailed(e.to_string()))?;

        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )
        .map_err(|e| TuiError::InitFailed(e.to_string()))?;

        // Ask the terminal for unambiguous key events (kitty keyboard
        // protocol). Lets us distinguish Shift+Space from plain Space so
        // the command palette shortcut can work. Terminals that don't
        // support the protocol silently ignore the CSI sequence, in which
        // case Shift+Space falls back to a plain-Space event (opens the
        // unified palette) and the user can still reach command-only mode
        // via the `>` prefix.
        let _ = execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,)
        );

        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).map_err(|e| TuiError::InitFailed(e.to_string()))?;

        Ok(terminal)
    }

    /// Restore terminal to normal state
    fn restore_terminal(&self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        info!("Disabling raw mode");
        disable_raw_mode().map_err(|e| TuiError::RestoreFailed(e.to_string()))?;

        // Pop the keyboard enhancement flags we pushed on setup. Best-effort —
        // on terminals that ignored the push this is a no-op.
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);

        info!("Leaving alternate screen");
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableBracketedPaste,
            DisableMouseCapture
        )
        .map_err(|e| TuiError::RestoreFailed(e.to_string()))?;

        info!("Showing cursor");
        terminal
            .show_cursor()
            .map_err(|e| TuiError::RestoreFailed(e.to_string()))?;

        info!("Terminal restore complete");
        Ok(())
    }
}

/// The result of diffing the configured `remote_servers` on a hot-reload,
/// keyed by server *name* (the stable identity — `BackendId` is positional and
/// config order can change). A server whose `url`/`token` changed appears in
/// both `removed` and `added`, so it is torn down and rebuilt.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RemoteServersReconcile {
    /// Servers to construct (new, or changed since the old config).
    pub added: Vec<claude_commander_core::config::RemoteServerConfig>,
    /// Names of servers to tear down (gone, or changed since the old config).
    pub removed: Vec<String>,
}

/// Pure diff of two `remote_servers` lists → the add/remove decisions the TUI
/// applies to its live `Vec<BackendHandle>`. Matching is by name; a name in
/// both lists with an unchanged config is a no-op, while a changed config is a
/// remove-then-add. Order differences alone produce no add/remove (the caller
/// reorders separately).
pub(super) fn reconcile_remote_servers(
    old: &[claude_commander_core::config::RemoteServerConfig],
    new: &[claude_commander_core::config::RemoteServerConfig],
) -> RemoteServersReconcile {
    let mut removed = Vec::new();
    for o in old {
        match new.iter().find(|n| n.name == o.name) {
            Some(n) if n == o => {}            // unchanged: keep
            _ => removed.push(o.name.clone()), // gone or changed
        }
    }
    let mut added = Vec::new();
    for n in new {
        match old.iter().find(|o| o.name == n.name) {
            Some(o) if o == n => {}     // unchanged: keep
            _ => added.push(n.clone()), // new or changed
        }
    }
    RemoteServersReconcile { added, removed }
}

/// Whether `url`'s host is a loopback address (`localhost`, `127.0.0.0/8`, or
/// `::1`). Used to warn about a self-referential remote server — one sharing
/// this machine's state would double-render sessions. A cheap string heuristic
/// (not full URL parsing): strip the scheme, authority separators, and any
/// userinfo/port, then match the host.
pub(super) fn is_loopback_url(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Drop any `user:pass@` userinfo prefix.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    // Bracketed IPv6 (`[::1]:port`) vs. `host:port` — a bare `:` split would
    // mangle an IPv6 literal, so peel the brackets first.
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };
    host.eq_ignore_ascii_case("localhost") || host == "::1" || host.starts_with("127.")
}
