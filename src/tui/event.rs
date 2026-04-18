//! Event handling for the TUI
//!
//! Provides an async event stream that combines:
//! - Terminal input events (keyboard, mouse)
//! - Application state updates
//! - Render ticks

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::config::keybindings::{BindableAction, KeyBindings};
use crate::git::{DiffInfo, EnrichedPrInfo};
use crate::session::{AgentState, ProjectId, SessionId};

use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use futures::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use tracing::debug;

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
    /// Project added
    ProjectAdded { project_id: ProjectId },
    /// Session added
    SessionAdded { session_id: SessionId },
    /// Session removed
    SessionRemoved { session_id: SessionId },
    /// Error occurred
    Error { message: String },
    /// PR status results ready from background check
    PrStatusReady {
        results: Vec<(SessionId, Option<crate::git::PrInfo>)>,
    },
    /// Session creation completed successfully
    SessionCreated { session_id: SessionId },
    /// Session creation failed
    SessionCreateFailed {
        session_id: SessionId,
        message: String,
    },
    /// State file was modified by another instance
    ExternalChange,
    /// Enriched PR info ready from background fetch
    EnrichedPrReady {
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
    /// Agent states updated from background polling
    AgentStatesUpdated {
        states: HashMap<SessionId, AgentState>,
    },
    /// Background `git fetch origin` kicked off by the Checkout modal
    /// has finished — the modal should refresh its branch list if still open.
    CheckoutFetchComplete {
        project_id: ProjectId,
        /// Fresh branch list produced after the fetch completed.
        branches: Vec<(String, bool)>,
    },
    /// Preview/diff/shell data ready from background fetch
    PreviewReady {
        /// Which session this data is for (None if project-level)
        session_id: Option<SessionId>,
        /// Which project this data is for
        project_id: Option<ProjectId>,
        /// Preview pane content (tmux capture)
        preview_content: String,
        /// Diff information
        diff_info: Arc<DiffInfo>,
        /// Shell pane content
        shell_content: String,
    },
}

/// User commands triggered by input
#[derive(Debug, Clone)]
pub enum UserCommand {
    /// Navigate up in the list
    NavigateUp,
    /// Navigate down in the list
    NavigateDown,
    /// Select/attach to current item
    Select,
    /// Open shell in worktree
    SelectShell,
    /// Create new session
    NewSession,
    /// Create new project
    NewProject,
    /// Checkout an existing branch into a new worktree session
    CheckoutBranch,
    /// Delete/kill current session
    DeleteSession,
    /// Rename the currently selected session (UI title only)
    RenameSession,
    /// Restart current session (kill tmux and recreate)
    RestartSession,
    /// Remove an entire project
    RemoveProject,
    /// Open worktree in editor/IDE
    OpenInEditor,
    /// Open the selected session's PR URL in a web browser
    OpenPullRequest,
    /// Toggle between preview/diff panes
    TogglePane,
    /// Toggle between preview/diff panes (reverse)
    TogglePaneReverse,
    /// Shrink left pane (move divider left)
    ShrinkLeftPane,
    /// Grow left pane (move divider right)
    GrowLeftPane,
    /// Show help
    ShowHelp,
    /// Show settings modal
    ShowSettings,
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
    /// Scroll preview up
    ScrollUp,
    /// Scroll preview down
    ScrollDown,
    /// Page up in preview
    PageUp,
    /// Page down in preview
    PageDown,
    /// Open quick-switch session search modal
    QuickSwitch,
    /// Generate AI summary for the current session (Info pane only)
    GenerateSummary,
    /// Scan a directory for git repos and add them as projects
    ScanDirectory,
    /// Open the "Move to section" modal for the selected session.
    MoveToSection,
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
}

impl From<BindableAction> for UserCommand {
    fn from(action: BindableAction) -> Self {
        match action {
            BindableAction::NavigateUp => Self::NavigateUp,
            BindableAction::NavigateDown => Self::NavigateDown,
            BindableAction::Select => Self::Select,
            BindableAction::SelectShell => Self::SelectShell,
            BindableAction::NewSession => Self::NewSession,
            BindableAction::NewProject => Self::NewProject,
            BindableAction::CheckoutBranch => Self::CheckoutBranch,
            BindableAction::DeleteSession => Self::DeleteSession,
            BindableAction::RenameSession => Self::RenameSession,
            BindableAction::RestartSession => Self::RestartSession,
            BindableAction::RemoveProject => Self::RemoveProject,
            BindableAction::OpenInEditor => Self::OpenInEditor,
            BindableAction::OpenPullRequest => Self::OpenPullRequest,
            BindableAction::TogglePane => Self::TogglePane,
            BindableAction::TogglePaneReverse => Self::TogglePaneReverse,
            BindableAction::ShrinkLeftPane => Self::ShrinkLeftPane,
            BindableAction::GrowLeftPane => Self::GrowLeftPane,
            BindableAction::ShowHelp => Self::ShowHelp,
            BindableAction::ShowSettings => Self::ShowSettings,
            BindableAction::Quit => Self::Quit,
            BindableAction::ScrollUp => Self::ScrollUp,
            BindableAction::ScrollDown => Self::ScrollDown,
            BindableAction::PageUp => Self::PageUp,
            BindableAction::PageDown => Self::PageDown,
            BindableAction::GenerateSummary => Self::GenerateSummary,
            BindableAction::ScanDirectory => Self::ScanDirectory,
            BindableAction::MoveToSection => Self::MoveToSection,
        }
    }
}

/// Event loop handle
pub struct EventLoop {
    /// Sender for events
    tx: mpsc::Sender<AppEvent>,
    /// Receiver for events
    rx: mpsc::Receiver<AppEvent>,
    /// Generation counter for input reader (used to stop old readers)
    input_generation: Arc<AtomicU64>,
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
            input_generation: Arc::new(AtomicU64::new(0)),
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

    /// Start the input reader task
    fn start_input_reader(&self) {
        let tx = self.tx.clone();
        let generation = self.input_generation.load(Ordering::SeqCst);
        let generation_ref = self.input_generation.clone();

        tokio::spawn(async move {
            let mut reader = EventStream::new();

            loop {
                // Check if we should stop (generation changed = stop signal)
                if generation_ref.load(Ordering::SeqCst) != generation {
                    debug!("Input reader stopping (generation changed)");
                    break;
                }

                // Use short timeout to check generation frequently
                let event =
                    tokio::time::timeout(Duration::from_millis(50), reader.next().fuse()).await;

                match event {
                    Ok(Some(Ok(event))) => {
                        // Re-check generation before sending (might have changed during read)
                        if generation_ref.load(Ordering::SeqCst) != generation {
                            debug!("Input reader stopping (generation changed during read)");
                            break;
                        }

                        let app_event = match event {
                            CrosstermEvent::Key(key) => AppEvent::Input(InputEvent::Key(key)),
                            CrosstermEvent::Mouse(mouse) => {
                                AppEvent::Input(InputEvent::Mouse(mouse))
                            }
                            CrosstermEvent::Resize(w, h) => {
                                AppEvent::Input(InputEvent::Resize(w, h))
                            }
                            CrosstermEvent::Paste(text) => AppEvent::Input(InputEvent::Paste(text)),
                            _ => continue,
                        };

                        if tx.send(app_event).await.is_err() {
                            break;
                        }
                    }
                    Ok(Some(Err(e))) => {
                        debug!("Error reading terminal event: {}", e);
                        continue;
                    }
                    Ok(None) => break,
                    Err(_) => continue, // Timeout, loop back to check generation
                }
            }
            debug!("Input reader task exited");
        });
    }

    /// Stop the input reader before tmux attach
    ///
    /// Increments generation to signal current reader to stop, then waits briefly
    /// for it to actually stop so it won't compete for stdin during attach.
    pub fn stop_input(&mut self) {
        self.input_generation.fetch_add(1, Ordering::SeqCst);
        debug!("Input reader stop signaled");
    }

    /// Restart the input reader after returning from tmux attach
    pub fn restart_input(&mut self) {
        // Drain any stale events from the channel
        while self.rx.try_recv().is_ok() {}

        // Start a fresh input reader (generation was already incremented by stop_input)
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

        let key = KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::ShrinkLeftPane)
        ));

        let key = KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::GrowLeftPane)
        ));

        // Some terminals report without SHIFT
        let key = KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::ShrinkLeftPane)
        ));

        let key = KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(key, &b),
            Some(UserCommand::GrowLeftPane)
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
    fn test_tab_toggles_pane() {
        let b = kb();

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(tab, &b),
            Some(UserCommand::TogglePane)
        ));

        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(matches!(
            UserCommand::from_key(backtab, &b),
            Some(UserCommand::TogglePaneReverse)
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
                KeyCode::Char('r'),
                KeyModifiers::NONE,
                UserCommand::RenameSession,
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

        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(pgup, &b),
            Some(UserCommand::PageUp)
        ));

        let pgdown = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        assert!(matches!(
            UserCommand::from_key(pgdown, &b),
            Some(UserCommand::PageDown)
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
}
