//! Main TUI application
//!
//! Event-driven application that coordinates:
//! - Terminal rendering with ratatui
//! - User input handling
//! - Background state updates

use std::collections::HashMap;
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
use tracing::{debug, info, warn};

use super::event::{AppEvent, EventLoop, InputEvent, StateUpdate, UserCommand};
use super::path_completer::PathCompleter;
use super::theme::Theme;
use super::widgets::{
    InfoContent, InfoProjectData, InfoSessionData, InfoView, InfoViewState, Preview, PreviewState,
    TreeList, TreeListState,
};
use crate::config::{BindableAction, Config, ConfigStore, StateStore};
use crate::error::{Result, TuiError};
use crate::git::{
    AiSummary, BlockReason, DiffInfo, EnrichedPrInfo, PrCheckResult, PullOutcome,
    check_pr_for_branch, diff_hash, fetch_branch_summary, fetch_enriched_pr, is_gh_available,
    run_project_pull,
};
use crate::session::{
    AgentState, ProjectId, SessionId, SessionListItem, SessionManager, SessionStatus,
    WorktreeSession,
};
use crate::tmux::AgentStateDetector;

mod actions;
mod background;
mod event_loop;
mod input;
mod modals;
mod render;
mod selection;
mod settings;
mod state;

#[cfg(test)]
mod tests;

/// Direction for mouse scroll events
enum ScrollDirection {
    Up,
    Down,
}

/// Minimum left pane width as a percentage of the content area
const MIN_LEFT_PANE_PCT: u16 = 15;
/// Maximum left pane width as a percentage of the content area
const MAX_LEFT_PANE_PCT: u16 = 60;
/// Default left pane width as a percentage of the content area
const DEFAULT_LEFT_PANE_PCT: u16 = 30;

/// Which pane is currently focused
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusedPane {
    #[default]
    SessionList,
    RightPane,
}

/// Which view is shown in the right pane
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RightPaneView {
    #[default]
    Preview,
    Info,
    Shell,
}

/// Which list view mode is active
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    ProjectGrouped,
    SectionGrouped,
}

/// A single entry in the pre-computed stack chain for the Info pane.
#[derive(Debug, Clone)]
pub struct StackChainEntry {
    pub title: String,
    pub status: SessionStatus,
    pub is_current: bool,
}

/// Modal dialog state
#[derive(Debug, Clone)]
pub enum Modal {
    /// No modal open
    None,
    /// Text input modal
    Input {
        title: String,
        prompt: String,
        value: String,
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
        value: String,
        on_submit: InputAction,
        completer: PathCompleter,
        /// First visible row of the completions list.
        scroll: usize,
    },
    /// Loading spinner modal (non-interactive)
    Loading { title: String, message: String },
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
        query: String,
        matches: Vec<QuickSwitchItem>,
        selected_idx: usize,
        /// Index of the first visible row — keeps `selected_idx` inside
        /// the visible window when the list is longer than can fit.
        scroll: usize,
    },
    /// Checkout-existing-branch modal. Shows an input field plus a
    /// filterable/scrollable list of branches (local + remote) and
    /// creates a worktree session from the selected branch on submit.
    CheckoutBranch {
        /// Project the session will belong to
        project_id: ProjectId,
        /// Current input text (filter + paste target)
        query: String,
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
}

/// A session match in the quick-switch modal
#[derive(Debug, Clone)]
pub struct QuickSwitchMatch {
    pub session_id: SessionId,
    pub title: String,
    pub branch: String,
    pub project_name: String,
    pub status: SessionStatus,
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
    /// Section picker for a specific session. The palette is populated with
    /// one entry per configured `[[sections]]` plus an "Auto" entry; selecting
    /// an entry sets (or clears) the session's `section_override`.
    SectionPicker {
        session_id: SessionId,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    General,
    Keybindings,
    Theme,
    Sections,
}

impl SettingsTab {
    const ALL: [SettingsTab; 4] = [
        Self::General,
        Self::Keybindings,
        Self::Theme,
        Self::Sections,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Keybindings => "Keybindings",
            Self::Theme => "Theme",
            Self::Sections => "Sections",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::General => Self::Keybindings,
            Self::Keybindings => Self::Theme,
            Self::Theme => Self::Sections,
            Self::Sections => Self::General,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::General => Self::Sections,
            Self::Keybindings => Self::General,
            Self::Theme => Self::Keybindings,
            Self::Sections => Self::Theme,
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
    RenamingSection { value: String },
    EditingPredicate { value: String },
    CreatingSection { value: String },
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
}

/// A single row in the settings list
#[derive(Debug, Clone)]
pub struct SettingsRow {
    pub label: String,
    pub value: String,
    pub field_key: String,
    /// Optional color for displaying a swatch next to the value (Theme tab only)
    pub color_swatch: Option<Color>,
}

/// Editing state within the settings modal
#[derive(Debug, Clone)]
pub enum SettingsEditing {
    /// Editing a text value
    TextInput { value: String },
    /// Capturing a key for keybinding
    KeyCapture {
        action_name: String,
        keys: Vec<String>,
    },
    /// Picking from a list of options (used for theme presets)
    OptionPicker {
        options: Vec<String>,
        selected: usize,
    },
}

/// Action to perform when input modal is submitted
#[derive(Debug, Clone)]
pub enum InputAction {
    CreateSession {
        project_id: ProjectId,
    },
    CreateStackedSession {
        project_id: ProjectId,
        parent_session_id: SessionId,
        parent_branch: String,
    },
    AddProject,
    ScanDirectory,
    RenameSession {
        session_id: SessionId,
    },
}

/// Action to perform when confirm modal is confirmed
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    DeleteSession { session_id: SessionId },
    DeleteMergedPrSessions { session_ids: Vec<SessionId> },
    RestartSession { session_id: SessionId },
    RemoveProject { project_id: ProjectId },
}

/// Application UI state
pub struct AppUiState {
    /// Session list state
    pub list_state: TreeListState,
    /// Preview pane state
    pub preview_state: PreviewState,
    /// Info pane state
    pub info_state: InfoViewState,
    /// Enriched PR info for the currently selected session
    pub enriched_pr: Option<(SessionId, EnrichedPrInfo)>,
    /// Cached AI summaries keyed by session ID
    pub ai_summaries: std::collections::HashMap<SessionId, AiSummary>,
    /// Currently focused pane
    pub focused_pane: FocusedPane,
    /// Which view is shown in the right pane
    pub right_pane_view: RightPaneView,
    /// Current modal
    pub modal: Modal,
    /// Session list items (flattened hierarchy)
    pub list_items: Vec<SessionListItem>,
    /// Preview content
    pub preview_content: String,
    /// Shell pane state
    pub shell_state: PreviewState,
    /// Shell content
    pub shell_content: String,
    /// Diff info
    pub diff_info: Arc<DiffInfo>,
    /// Status message (with expiry time)
    pub status_message: Option<(String, Instant)>,
    /// Should quit
    pub should_quit: bool,
    /// Last known terminal size (updated each render frame)
    pub terminal_size: Rect,
    /// Currently selected session (for preview/diff)
    pub selected_session_id: Option<SessionId>,
    /// Currently selected project
    pub selected_project_id: Option<ProjectId>,
    /// Attach command to run after exiting TUI
    pub attach_command: Option<String>,
    /// Editor command + path to open after exiting TUI
    pub editor_command: Option<(String, PathBuf)>,
    /// When attached via shell toggle (Ctrl+\), stores the session name to switch back to.
    /// Contains (current_session_name, paired_session_name) so we can toggle between them.
    pub shell_toggle_pair: Option<(String, String)>,
    /// Needs right pane clear (set on view switch, consumed on render)
    pub clear_right_pane: bool,
    /// Left pane width as a percentage (adjustable at runtime via < / >)
    pub left_pane_pct: u16,
    /// When the last PR status check was performed
    pub last_pr_check: Option<Instant>,
    /// Whether the `gh` CLI is available
    pub gh_available: bool,
    /// When the last background preview fetch was spawned (None = not in flight)
    pub preview_update_spawned_at: Option<Instant>,
    /// Tick counter for animations (incremented each render tick)
    pub tick_count: u64,
    /// Throbber/spinner state for loading modals
    pub throbber_state: throbber_widgets_tui::ThrobberState,
    /// Current agent states for Running Claude sessions (ephemeral, from background poller)
    pub agent_states: HashMap<SessionId, AgentState>,
    /// Cached mirror of `AppState::cascade_paused_at.is_some()` — used by
    /// `is_command_available` to gate the `CascadeResume` / `CascadeAbandon`
    /// palette entries without an async read on every keystroke. Refreshed
    /// alongside `list_items`.
    pub cascade_paused: bool,
    /// Section names that are currently collapsed in the list view.
    pub collapsed_sections: std::collections::HashSet<String>,
    /// Last left-mouse click on a session-list row: (list index, timestamp).
    /// Used to detect double-click on the same row within `DOUBLE_CLICK_WINDOW`.
    pub last_left_click: Option<(usize, Instant)>,
    /// Current list view mode (project-grouped vs section-grouped).
    pub view_mode: ViewMode,
    /// Pre-computed stack chain for the selected session (empty if not stacked).
    pub stack_chain: Vec<StackChainEntry>,
    /// When each project last completed a background pull attempt
    /// (success or block). Drives the per-project interval scheduler.
    pub last_project_pull: HashMap<ProjectId, Instant>,
    /// Projects whose most recent pull was held back, with the reason.
    /// Cleared when a subsequent attempt advances or finds nothing to do.
    pub project_pull_blocked: HashMap<ProjectId, BlockReason>,
    /// Projects with a pull task currently in flight, so we don't double-spawn.
    pub project_pull_in_flight: std::collections::HashSet<ProjectId>,
    /// When the app launched. Used to give the background pull task a short
    /// grace period before its first fire after startup.
    pub started_at: Instant,
    /// When the project-pull scheduler last swept the project list. A cheap
    /// global throttle so the per-tick check doesn't acquire the state lock
    /// and clone the project list on every render frame.
    pub last_project_pull_sweep: Option<Instant>,
}

impl Default for AppUiState {
    fn default() -> Self {
        Self {
            list_state: TreeListState::new(),
            preview_state: PreviewState::new(),
            info_state: InfoViewState::new(),
            enriched_pr: None,
            ai_summaries: std::collections::HashMap::new(),
            shell_state: PreviewState::new(),
            shell_content: String::new(),
            focused_pane: FocusedPane::default(),
            right_pane_view: RightPaneView::default(),
            modal: Modal::None,
            list_items: Vec::new(),
            preview_content: String::new(),
            diff_info: Arc::new(DiffInfo::empty()),
            status_message: None, // (message, expiry)

            should_quit: false,
            selected_session_id: None,
            selected_project_id: None,
            attach_command: None,
            editor_command: None,
            shell_toggle_pair: None,
            clear_right_pane: false,
            left_pane_pct: DEFAULT_LEFT_PANE_PCT,
            last_pr_check: None,
            gh_available: false,
            preview_update_spawned_at: None,
            terminal_size: Rect::default(),
            tick_count: 0,
            throbber_state: throbber_widgets_tui::ThrobberState::default(),
            agent_states: HashMap::new(),
            cascade_paused: false,
            collapsed_sections: std::collections::HashSet::new(),
            last_left_click: None,
            view_mode: ViewMode::default(),
            stack_chain: Vec::new(),
            last_project_pull: HashMap::new(),
            project_pull_blocked: HashMap::new(),
            project_pull_in_flight: std::collections::HashSet::new(),
            started_at: Instant::now(),
            last_project_pull_sweep: None,
        }
    }
}

impl AppUiState {
    /// Whether a given command is currently invokable.
    ///
    /// These rules mirror the early-return guards scattered across
    /// `App::handle_command` and friends, so the palette only lists
    /// commands that would actually *do* something if selected. Pure with
    /// respect to `self` — safe to unit-test by constructing a default
    /// `AppUiState` and mutating a few fields.
    pub fn is_command_available(&self, action: BindableAction) -> bool {
        let has_session = self.selected_session_id.is_some();
        let has_project = self.selected_project_id.is_some();
        match action {
            // Session-scoped actions require a selected session
            BindableAction::Select
            | BindableAction::SelectShell
            | BindableAction::DeleteSession
            | BindableAction::RenameSession
            | BindableAction::RestartSession
            | BindableAction::OpenInEditor
            | BindableAction::OpenPullRequest
            | BindableAction::MoveToSection => has_session,
            // Cascade merge is only meaningful from a session that's part of
            // a stack. We accept any selected session here; the handler is
            // cheap to no-op if the stack chain turns out to be length 1.
            BindableAction::CascadeMergeMain | BindableAction::PushStack => has_session,
            // Cascade resume / abandon are only meaningful when a cascade is paused.
            BindableAction::CascadeResume | BindableAction::CascadeAbandon => self.cascade_paused,
            // Removing a project is only meaningful from a project row (no session selected)
            BindableAction::RemoveProject => has_project && !has_session,
            // GenerateSummary only does something when the Info pane is active
            BindableAction::GenerateSummary => {
                has_session && self.right_pane_view == RightPaneView::Info
            }
            // All other actions are always available
            _ => true,
        }
    }

    /// Build the palette's command rows for a given filter query.
    ///
    /// Commands with no effective keybinding are still included — the
    /// palette is intended to be the canonical access surface as hotkeys
    /// get trimmed over time. `NavigateUp` and `NavigateDown` are excluded
    /// because they only make sense as palette-internal list navigation.
    pub fn gather_command_entries(
        &self,
        kb: &crate::config::KeyBindings,
        filter_query: &str,
    ) -> Vec<CommandEntry> {
        let mut scored: Vec<(i64, CommandEntry)> = Vec::new();
        for &action in BindableAction::ALL {
            if matches!(
                action,
                BindableAction::NavigateUp | BindableAction::NavigateDown
            ) {
                continue;
            }
            if !self.is_command_available(action) {
                continue;
            }
            let label = action.description();
            let Some(score) = crate::fuzzy::fuzzy_score(label, filter_query) else {
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
pub struct App {
    /// Local config cache — refreshed from config_store on tick when file changes
    config: Config,
    /// Shared config store (hot-reloaded from disk)
    config_store: Arc<ConfigStore>,
    /// Concurrent-safe persistent state store
    store: Arc<StateStore>,
    /// Session manager
    session_manager: SessionManager,
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
}

impl App {
    /// Create a new application
    pub fn new(config_store: Arc<ConfigStore>, store: Arc<StateStore>) -> Self {
        let config = config_store.read().clone();
        let theme = Theme::default();
        let session_manager = SessionManager::new(
            config_store.clone(),
            store.clone(),
            theme.tmux_status_style(),
        );

        let base = config
            .theme
            .preset
            .as_deref()
            .and_then(Theme::from_preset)
            .unwrap_or_default();
        let theme = base.with_overrides(&config.theme);
        let debounce = Duration::from_millis(config.session_number_debounce_ms);

        Self {
            config,
            config_store,
            store,
            session_manager,
            ui_state: AppUiState::default(),
            event_loop: EventLoop::new(),
            theme,
            suppress_keys_until: Instant::now(),
            digit_accumulator: super::digit_accumulator::DigitAccumulator::new(debounce),
        }
    }

    /// Run the application
    pub async fn run(&mut self) -> Result<()> {
        // Check tmux is available
        self.session_manager.check_tmux().await?;

        // One-time setup
        self.cleanup_stale_creating_sessions().await;
        self.cleanup_stale_merging_sessions().await;
        self.sync_session_states().await;
        self.reconcile_section_assignments().await;

        // Check gh availability and do initial PR check
        if self.config.pr_check_interval_secs > 0 {
            self.ui_state.gh_available = is_gh_available().await;
            if self.ui_state.gh_available {
                self.spawn_pr_status_check();
            }
        }

        let tick_rate = Duration::from_millis(1000 / self.config.ui_refresh_fps as u64);
        self.event_loop.start(tick_rate);

        // Start background state sync for cross-instance changes
        if self.config.state_sync_interval_ms > 0 {
            let store = self.store.clone();
            let tx = self.event_loop.sender();
            let interval_ms = self.config.state_sync_interval_ms;
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
                loop {
                    interval.tick().await;
                    match store.reload_if_changed().await {
                        Ok(true) => {
                            let _ = tx
                                .send(AppEvent::StateUpdate(StateUpdate::ExternalChange))
                                .await;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            debug!("State sync check failed: {}", e);
                        }
                    }
                }
            });
        }

        // Start background agent state polling
        if self.config.agent_state_poll_interval_ms > 0 {
            let store = self.store.clone();
            let tx = self.event_loop.sender();
            let interval_ms = self.config.agent_state_poll_interval_ms;
            let tmux = self.session_manager.tmux.clone();
            tokio::spawn(async move {
                let cache_ttl = Duration::from_millis(interval_ms.saturating_sub(500).max(500));
                let mut detector = AgentStateDetector::new(tmux, cache_ttl);
                let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
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
                    if sessions.is_empty() {
                        continue;
                    }
                    let states: HashMap<SessionId, AgentState> =
                        detector.detect_all(&sessions).await;
                    if !states.is_empty() {
                        let _ = tx
                            .send(AppEvent::StateUpdate(StateUpdate::AgentStatesUpdated {
                                states,
                            }))
                            .await;
                    }
                }
            });
        }

        // Set initial view mode based on whether sections are configured
        if !self.config.sections.is_empty() {
            self.ui_state.view_mode = ViewMode::SectionGrouped;
        }

        // Restore last selection from persisted state
        self.refresh_list_items().await;
        self.restore_selection().await;

        loop {
            // Setup terminal for TUI
            let mut terminal = self.setup_terminal()?;
            self.refresh_list_items().await;

            // Run main loop until quit or attach
            info!("Entering main loop");
            let result = self.main_loop(&mut terminal).await;
            info!("Main loop exited with result: {:?}", result.is_ok());

            // Restore terminal before attach or exit
            info!("Restoring terminal");
            self.restore_terminal(&mut terminal)?;
            info!("Terminal restored successfully");

            // Reset should_quit for next iteration
            self.ui_state.should_quit = false;

            if let Some((editor, path)) = self.ui_state.editor_command.take() {
                // Run editor as a foreground process, then return to TUI
                self.event_loop.stop_input();
                tokio::time::sleep(Duration::from_millis(100)).await;

                info!("Launching editor: {} {}", editor, path.display());
                let status = std::process::Command::new(&editor).arg(&path).status();

                if let Err(e) = status {
                    self.ui_state.modal = Modal::Error {
                        message: format!("Failed to launch '{}': {}", editor, e),
                    };
                }

                self.event_loop.restart_input();
            } else {
                match self.ui_state.attach_command.take() {
                    Some(cmd) => {
                        // Stop the input reader BEFORE attaching so it doesn't compete for stdin
                        self.event_loop.stop_input();
                        // Brief delay to let the reader task actually stop
                        tokio::time::sleep(Duration::from_millis(100)).await;

                        // Flush any pending input (e.g. the Enter key that triggered this attach)
                        crate::tmux::flush_stdin();

                        // Attach to session via async PTY bridge (supports Ctrl+Q detach, Ctrl+\ shell toggle)
                        info!("Executing attach command: {}", cmd);
                        let session_name = cmd.split_whitespace().last().unwrap_or("").to_string();
                        if !session_name.is_empty() {
                            let mut current_session = session_name.clone();
                            let mut consecutive_ends: u8 = 0;

                            loop {
                                let editor_triggers =
                                    crate::config::keybindings::editor_trigger_bytes(
                                        &self.config.keybindings,
                                    );
                                // Shell sessions are named with a trailing
                                // "-sh" (see resolve_shell_toggle_pair). Only
                                // intercept Ctrl+Z for non-shell (Claude)
                                // sessions, where SIGTSTP would freeze the
                                // pane with no shell to recover from.
                                let intercept_ctrl_z = !current_session.ends_with("-sh");

                                // Stamp last_attached_at so the in-tmux
                                // switcher can sort Alt+Tab-style by MRU.
                                let to_stamp = current_session.clone();
                                if let Err(e) = self
                                    .store
                                    .mutate(move |state| {
                                        if let Some(session) = state
                                            .sessions
                                            .values_mut()
                                            .find(|s| s.matches_tmux_name(&to_stamp))
                                        {
                                            session.mark_attached();
                                        }
                                    })
                                    .await
                                {
                                    warn!("Failed to stamp last_attached_at: {}", e);
                                }

                                let outcome = match crate::tmux::attach_to_session(
                                    &current_session,
                                    editor_triggers,
                                    intercept_ctrl_z,
                                )
                                .await
                                {
                                    Ok(o) => o,
                                    Err(e) => {
                                        warn!("Failed to attach to session: {}", e);
                                        self.ui_state.modal = Modal::Error {
                                            message: format!("Failed to attach: {}", e),
                                        };
                                        self.ui_state.shell_toggle_pair = None;
                                        break;
                                    }
                                };

                                // The in-session switcher may have run `tmux switch-client`
                                // mid-attach, so the session we exited from isn't
                                // necessarily the one we entered with. Trust the outcome.
                                let switched_via_popup = outcome.final_session != current_session;
                                current_session = outcome.final_session;
                                if switched_via_popup {
                                    // Picking a new session in the popup invalidates the
                                    // shell-toggle pair (which is tied to a specific
                                    // Claude/shell duo).
                                    self.ui_state.shell_toggle_pair = None;
                                }

                                match outcome.result {
                                    crate::tmux::AttachResult::SwitchToShell => {
                                        info!(
                                            "Shell toggle requested from session: {}",
                                            current_session
                                        );

                                        // Determine the paired session to switch to
                                        let next_session = match &self.ui_state.shell_toggle_pair {
                                            Some((_, paired)) => paired.clone(),
                                            None => {
                                                // First toggle — resolve the shell session
                                                match self
                                                    .resolve_shell_toggle_pair(&current_session)
                                                    .await
                                                {
                                                    Ok(paired) => paired,
                                                    Err(e) => {
                                                        warn!(
                                                            "Failed to resolve shell session: {}",
                                                            e
                                                        );
                                                        self.ui_state.modal = Modal::Error {
                                                            message: format!(
                                                                "Cannot switch to shell: {}",
                                                                e
                                                            ),
                                                        };
                                                        break;
                                                    }
                                                }
                                            }
                                        };

                                        // Update the toggle pair so next Ctrl+\ switches back
                                        self.ui_state.shell_toggle_pair =
                                            Some((next_session.clone(), current_session.clone()));
                                        current_session = next_session;

                                        // Flush between switches
                                        crate::tmux::flush_stdin();
                                        consecutive_ends = 0;
                                        info!("Switching to session: {}", current_session);
                                        continue;
                                    }
                                    crate::tmux::AttachResult::OpenEditor => {
                                        info!(
                                            "OpenEditor requested from session: {}",
                                            current_session
                                        );
                                        // Run the editor for the session's worktree, keep
                                        // the tmux session alive, and then re-attach.
                                        self.open_editor_for_tmux_session(&current_session).await;
                                        crate::tmux::flush_stdin();
                                        consecutive_ends = 0;
                                        continue;
                                    }
                                    crate::tmux::AttachResult::SessionEnded => {
                                        info!("Session ended, attempting fresh restart");
                                        let is_claude_session = !current_session.ends_with("-sh");
                                        if is_claude_session && consecutive_ends < 3 {
                                            consecutive_ends += 1;
                                            match self
                                                .session_manager
                                                .restart_session_fresh_by_tmux_name(
                                                    &current_session,
                                                )
                                                .await
                                            {
                                                Ok(()) => {
                                                    info!(
                                                        "Auto-restarted session fresh (attempt {})",
                                                        consecutive_ends
                                                    );
                                                    crate::tmux::flush_stdin();
                                                    continue;
                                                }
                                                Err(e) => {
                                                    warn!("Failed to auto-restart session: {}", e);
                                                    self.ui_state.shell_toggle_pair = None;
                                                    break;
                                                }
                                            }
                                        } else {
                                            if consecutive_ends >= 3 {
                                                warn!(
                                                    "Session ended {} consecutive times, \
                                                     giving up",
                                                    consecutive_ends
                                                );
                                            }
                                            self.ui_state.shell_toggle_pair = None;
                                            break;
                                        }
                                    }
                                    result => {
                                        info!("Attach ended: {:?}", result);
                                        self.ui_state.shell_toggle_pair = None;
                                        break;
                                    }
                                }
                            }
                        }

                        // Flush stdin again after detach to discard any stale input
                        crate::tmux::flush_stdin();

                        // Restart the input reader after detach
                        info!("Returned from attach, restarting input reader");
                        self.event_loop.restart_input();
                        // Loop continues, TUI resumes with state preserved
                    }
                    None => {
                        // Save selection before quitting
                        self.save_selection().await;
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
