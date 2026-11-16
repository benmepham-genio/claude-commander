//! Configurable keybinding system
//!
//! Provides a data-driven keybinding table that maps key combinations to
//! user commands. Keybindings are loaded from the `[keybindings]` section
//! of `config.toml` and fall back to sensible defaults when omitted.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// BindableAction — the subset of commands users can rebind
// ---------------------------------------------------------------------------

/// Actions that can be bound to key combinations via config.
///
/// This is intentionally separate from `UserCommand` — structural keys like
/// text input and backspace are not rebindable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindableAction {
    NavigateUp,
    NavigateDown,
    NextGroup,
    PreviousGroup,
    NavigateFirst,
    NavigateLast,
    NavigateLeft,
    NavigateRight,
    ListPageUp,
    ListPageDown,
    Select,
    SelectShell,
    NewSession,
    NewStackedSession,
    SetSessionBase,
    CascadeMergeMain,
    CascadeResume,
    CascadeAbandon,
    PushStack,
    NewProject,
    CloneRepository,
    CheckoutBranch,
    DeleteSession,
    DeleteMergedPrSessions,
    EditSession,
    RenameSession,
    RestartSession,
    ResetSession,
    ChangeProgram,
    ToggleKeepAlive,
    RemoveProject,
    OpenInEditor,
    OpenInfo,
    OpenPullRequest,
    RefreshPrStatus,
    OpenCommander,
    ToggleConversationOverlay,
    ToggleVoiceInput,
    ToggleDictation,
    OpenReviewDiff,
    ShowHelp,
    ShowSettings,
    CopyServerToken,
    Quit,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    GenerateSummary,
    ScanDirectory,
    MoveToSection,
    ToggleViewMode,
    ToggleSection,
    NextWorkspace,
    PreviousWorkspace,
    WorkspacePicker,
    NewWorkspace,
    MoveProjectToWorkspace,
    TogglePane,
    TogglePaneReverse,
    ShrinkLeftPane,
    GrowLeftPane,
    AddRemoteServer,
    RemoveRemoteServer,
    EditServerPrograms,
}

impl BindableAction {
    /// All actions in display order, grouped so each [`section`](Self::section)
    /// stays contiguous (both the help screen and the settings Keybindings tab
    /// rely on this ordering to draw section headers).
    pub const ALL: &'static [BindableAction] = &[
        // Navigation
        Self::NavigateUp,
        Self::NavigateDown,
        Self::NextGroup,
        Self::PreviousGroup,
        Self::NavigateFirst,
        Self::NavigateLast,
        Self::NavigateLeft,
        Self::NavigateRight,
        Self::ListPageUp,
        Self::ListPageDown,
        // Sessions
        Self::Select,
        Self::SelectShell,
        Self::NewSession,
        Self::EditSession,
        Self::RenameSession,
        Self::RestartSession,
        Self::ResetSession,
        Self::ChangeProgram,
        Self::ToggleKeepAlive,
        Self::DeleteSession,
        Self::OpenInEditor,
        Self::OpenInfo,
        // Stacked & Cascade
        Self::NewStackedSession,
        Self::SetSessionBase,
        Self::PushStack,
        Self::CascadeMergeMain,
        Self::CascadeResume,
        Self::CascadeAbandon,
        // Projects
        Self::NewProject,
        Self::CloneRepository,
        Self::CheckoutBranch,
        Self::ScanDirectory,
        Self::RemoveProject,
        // Pull Requests
        Self::OpenPullRequest,
        Self::RefreshPrStatus,
        Self::DeleteMergedPrSessions,
        // Remote Servers
        Self::AddRemoteServer,
        Self::RemoveRemoteServer,
        Self::EditServerPrograms,
        // Sections
        Self::MoveToSection,
        Self::ToggleViewMode,
        Self::ToggleSection,
        // Workspaces
        Self::NextWorkspace,
        Self::PreviousWorkspace,
        Self::WorkspacePicker,
        Self::NewWorkspace,
        Self::MoveProjectToWorkspace,
        // Right pane (list views)
        Self::TogglePane,
        Self::TogglePaneReverse,
        Self::ShrinkLeftPane,
        Self::GrowLeftPane,
        // Review & AI
        Self::OpenReviewDiff,
        Self::GenerateSummary,
        Self::OpenCommander,
        Self::ToggleConversationOverlay,
        Self::ToggleVoiceInput,
        Self::ToggleDictation,
        // Scrolling
        Self::ScrollUp,
        Self::ScrollDown,
        Self::PageUp,
        Self::PageDown,
        // Application
        Self::ShowHelp,
        Self::ShowSettings,
        Self::CopyServerToken,
        Self::Quit,
    ];

    /// Snake_case name used as the TOML key.
    pub fn config_name(self) -> &'static str {
        match self {
            Self::NavigateUp => "navigate_up",
            Self::NavigateDown => "navigate_down",
            Self::NextGroup => "next_group",
            Self::PreviousGroup => "previous_group",
            Self::NavigateFirst => "navigate_first",
            Self::NavigateLast => "navigate_last",
            Self::NavigateLeft => "navigate_left",
            Self::NavigateRight => "navigate_right",
            Self::ListPageUp => "list_page_up",
            Self::ListPageDown => "list_page_down",
            Self::Select => "select",
            Self::SelectShell => "select_shell",
            Self::NewSession => "new_session",
            Self::NewStackedSession => "new_stacked_session",
            Self::SetSessionBase => "set_session_base",
            Self::CascadeMergeMain => "cascade_merge_main",
            Self::CascadeResume => "cascade_resume",
            Self::CascadeAbandon => "cascade_abandon",
            Self::PushStack => "push_stack",
            Self::NewProject => "new_project",
            Self::CloneRepository => "clone_repository",
            Self::CheckoutBranch => "checkout_branch",
            Self::DeleteSession => "delete_session",
            Self::DeleteMergedPrSessions => "delete_merged_pr_sessions",
            Self::EditSession => "edit_session",
            Self::RenameSession => "rename_session",
            Self::RestartSession => "restart_session",
            Self::ResetSession => "reset_session",
            Self::ChangeProgram => "change_program",
            Self::ToggleKeepAlive => "toggle_keep_alive",
            Self::RemoveProject => "remove_project",
            Self::OpenInEditor => "open_in_editor",
            Self::OpenInfo => "open_info",
            Self::OpenPullRequest => "open_pull_request",
            Self::RefreshPrStatus => "refresh_pr_status",
            Self::OpenCommander => "open_commander",
            Self::ToggleConversationOverlay => "toggle_conversation_overlay",
            Self::ToggleVoiceInput => "toggle_voice_input",
            Self::ToggleDictation => "toggle_dictation",
            Self::OpenReviewDiff => "open_review_diff",
            Self::ShowHelp => "show_help",
            Self::ShowSettings => "show_settings",
            Self::CopyServerToken => "copy_server_token",
            Self::Quit => "quit",
            Self::ScrollUp => "scroll_up",
            Self::ScrollDown => "scroll_down",
            Self::PageUp => "page_up",
            Self::PageDown => "page_down",
            Self::GenerateSummary => "generate_summary",
            Self::ScanDirectory => "scan_directory",
            Self::MoveToSection => "move_to_section",
            Self::ToggleViewMode => "toggle_view_mode",
            Self::ToggleSection => "toggle_section",
            Self::NextWorkspace => "next_workspace",
            Self::PreviousWorkspace => "previous_workspace",
            Self::WorkspacePicker => "workspace_picker",
            Self::NewWorkspace => "new_workspace",
            Self::MoveProjectToWorkspace => "move_project_to_workspace",
            Self::TogglePane => "toggle_pane",
            Self::TogglePaneReverse => "toggle_pane_reverse",
            Self::ShrinkLeftPane => "shrink_left_pane",
            Self::GrowLeftPane => "grow_left_pane",
            Self::EditServerPrograms => "edit_server_programs",
            Self::AddRemoteServer => "add_remote_server",
            Self::RemoveRemoteServer => "remove_remote_server",
        }
    }

    /// Human-readable label for the help screen.
    pub fn description(self) -> &'static str {
        match self {
            Self::NavigateUp => "Navigate up",
            Self::NavigateDown => "Navigate down",
            Self::NextGroup => "Next column / project-section group",
            Self::PreviousGroup => "Previous column / project-section group",
            Self::NavigateFirst => "Jump to first card/item",
            Self::NavigateLast => "Jump to last card/item",
            Self::NavigateLeft => "Previous column",
            Self::NavigateRight => "Next column",
            Self::ListPageUp => "Move up a screenful in the list",
            Self::ListPageDown => "Move down a screenful in the list",
            Self::Select => "Attach to selected session",
            Self::SelectShell => "Open shell in worktree",
            Self::NewSession => "New worktree session",
            Self::NewStackedSession => "New stacked session on selected branch",
            Self::SetSessionBase => "Set session base (restack)\u{2026}",
            Self::CascadeMergeMain => "Cascade merge main through stack",
            Self::CascadeResume => "Resume paused cascade merge",
            Self::CascadeAbandon => "Abandon paused cascade merge",
            Self::PushStack => "Push stack to remote (base → leaf)",
            Self::NewProject => "New project (add git repo)",
            Self::CloneRepository => "Clone a hosted repository…",
            Self::CheckoutBranch => "Checkout existing branch",
            Self::DeleteSession => "Delete/kill session",
            Self::DeleteMergedPrSessions => "Delete sessions with merged PRs",
            Self::EditSession => "Edit session…",
            Self::RenameSession => "Rename session",
            Self::RestartSession => "Restart session",
            Self::ResetSession => "Reset session (restart without resuming)",
            Self::ChangeProgram => "Change program (agent)…",
            Self::ToggleKeepAlive => "Toggle keep-alive (never auto-hibernate)",
            Self::RemoveProject => "Remove project",
            Self::OpenInEditor => "Open in editor/IDE",
            Self::OpenInfo => "Show session info",
            Self::OpenPullRequest => "Open PR in browser",
            Self::RefreshPrStatus => "Refresh PR status",
            Self::OpenCommander => "Open commander session",
            Self::ToggleConversationOverlay => "Open/close conversation overlay (TTS)",
            Self::ToggleVoiceInput => "Voice input: record / send (STT)",
            Self::ToggleDictation => "Dictate: record / type transcript into pane or field (STT)",
            Self::OpenReviewDiff => "Review diff & comment",
            Self::ShowHelp => "Show help",
            Self::ShowSettings => "Settings",
            Self::CopyServerToken => "Copy server token (pair a client)",
            Self::Quit => "Quit",
            Self::ScrollUp => "Scroll up",
            Self::ScrollDown => "Scroll down",
            Self::PageUp => "First card in column / page up in modals",
            Self::PageDown => "Last card in column / page down in modals",
            Self::GenerateSummary => "Generate AI summary",
            Self::ScanDirectory => "Scan directory for repos",
            Self::MoveToSection => "Move session to section…",
            Self::ToggleViewMode => "Cycle view: project / sections / stacks / board",
            Self::ToggleSection => "Collapse/expand section",
            Self::NextWorkspace => "Next workspace",
            Self::PreviousWorkspace => "Previous workspace",
            Self::WorkspacePicker => "Switch workspace…",
            Self::NewWorkspace => "New workspace…",
            Self::MoveProjectToWorkspace => "Move project to workspace…",
            Self::TogglePane => "Cycle right pane: preview / info / shell",
            Self::TogglePaneReverse => "Cycle right pane (reverse)",
            Self::ShrinkLeftPane => "Narrow the session list",
            Self::GrowLeftPane => "Widen the session list",
            Self::EditServerPrograms => "Edit server's program list…",
            Self::AddRemoteServer => "Add remote server",
            Self::RemoveRemoteServer => "Remove remote server",
        }
    }

    /// Short verb phrase for on-screen action buttons, distinct from the
    /// longer help-screen [`description`](Self::description). Worded so the
    /// primary hotkey char appears naturally (favouring clean inline
    /// bracketing, e.g. `n` → "new session" renders "[n]ew session").
    ///
    /// Exhaustive on purpose (no wildcard): adding a `BindableAction` must
    /// force a button label to be supplied.
    pub fn button_label(self) -> &'static str {
        match self {
            Self::NavigateUp => "up",
            Self::NavigateDown => "down",
            Self::NextGroup => "next column",
            Self::PreviousGroup => "prev column",
            Self::NavigateFirst => "first",
            Self::NavigateLast => "last",
            Self::NavigateLeft => "prev column",
            Self::NavigateRight => "next column",
            Self::ListPageUp => "list page up",
            Self::ListPageDown => "list page down",
            Self::Select => "attach",
            Self::SelectShell => "shell",
            Self::NewSession => "new session",
            Self::NewStackedSession => "stacked",
            Self::SetSessionBase => "base",
            Self::CascadeMergeMain => "merge stack",
            Self::CascadeResume => "resume cascade",
            Self::CascadeAbandon => "abandon cascade",
            Self::PushStack => "push stack",
            Self::NewProject => "new project",
            Self::CloneRepository => "clone repo",
            Self::CheckoutBranch => "checkout",
            Self::DeleteSession => "delete",
            Self::DeleteMergedPrSessions => "delete merged",
            Self::EditSession => "edit",
            Self::RenameSession => "rename",
            Self::RestartSession => "restart",
            Self::ResetSession => "reset",
            Self::ChangeProgram => "program",
            Self::ToggleKeepAlive => "keep alive",
            Self::RemoveProject => "remove project",
            Self::OpenInEditor => "edit",
            Self::OpenInfo => "info",
            Self::OpenPullRequest => "open PR",
            Self::RefreshPrStatus => "refresh PR",
            Self::OpenCommander => "commander",
            Self::ToggleConversationOverlay => "conversation",
            Self::ToggleVoiceInput => "voice",
            Self::ToggleDictation => "dictate",
            Self::OpenReviewDiff => "review",
            Self::ShowHelp => "help",
            Self::ShowSettings => "settings",
            Self::CopyServerToken => "copy token",
            Self::Quit => "quit",
            Self::ScrollUp => "scroll up",
            Self::ScrollDown => "scroll down",
            Self::PageUp => "first card",
            Self::PageDown => "last card",
            Self::GenerateSummary => "summary",
            Self::ScanDirectory => "scan",
            Self::MoveToSection => "move",
            Self::ToggleViewMode => "view",
            Self::ToggleSection => "collapse",
            Self::NextWorkspace => "next workspace",
            Self::PreviousWorkspace => "previous workspace",
            Self::WorkspacePicker => "Workspaces",
            Self::NewWorkspace => "new workspace",
            Self::MoveProjectToWorkspace => "move project",
            Self::TogglePane => "pane",
            Self::TogglePaneReverse => "pane back",
            Self::ShrinkLeftPane => "narrower",
            Self::GrowLeftPane => "wider",
            Self::EditServerPrograms => "server programs",
            Self::AddRemoteServer => "add server",
            Self::RemoveRemoteServer => "remove server",
        }
    }

    /// Section this action is grouped under, for both the help screen and the
    /// settings Keybindings tab. The [`ALL`](Self::ALL) ordering keeps every
    /// section's actions contiguous so [`KeyBindings::sections`] can slice them
    /// into ordered groups.
    pub fn section(self) -> &'static str {
        match self {
            Self::NavigateUp
            | Self::NavigateDown
            | Self::NextGroup
            | Self::PreviousGroup
            | Self::NavigateFirst
            | Self::NavigateLast
            | Self::NavigateLeft
            | Self::NavigateRight
            | Self::ListPageUp
            | Self::ListPageDown => "Navigation",
            Self::Select
            | Self::SelectShell
            | Self::NewSession
            | Self::EditSession
            | Self::RenameSession
            | Self::RestartSession
            | Self::ResetSession
            | Self::ChangeProgram
            | Self::ToggleKeepAlive
            | Self::DeleteSession
            | Self::OpenInEditor
            | Self::OpenInfo => "Sessions",
            Self::NewStackedSession
            | Self::SetSessionBase
            | Self::PushStack
            | Self::CascadeMergeMain
            | Self::CascadeResume
            | Self::CascadeAbandon => "Stacked & Cascade",
            Self::NewProject
            | Self::CloneRepository
            | Self::CheckoutBranch
            | Self::ScanDirectory
            | Self::RemoveProject => "Projects",
            Self::OpenPullRequest | Self::RefreshPrStatus | Self::DeleteMergedPrSessions => {
                "Pull Requests"
            }
            Self::AddRemoteServer | Self::RemoveRemoteServer | Self::EditServerPrograms => {
                "Remote Servers"
            }
            Self::MoveToSection | Self::ToggleViewMode | Self::ToggleSection => "Sections",
            Self::NextWorkspace
            | Self::PreviousWorkspace
            | Self::WorkspacePicker
            | Self::NewWorkspace
            | Self::MoveProjectToWorkspace => "Workspaces",
            Self::TogglePane
            | Self::TogglePaneReverse
            | Self::ShrinkLeftPane
            | Self::GrowLeftPane => "Right Pane",
            Self::OpenReviewDiff
            | Self::GenerateSummary
            | Self::OpenCommander
            | Self::ToggleConversationOverlay
            | Self::ToggleVoiceInput
            | Self::ToggleDictation => "Review & AI",
            Self::ScrollUp | Self::ScrollDown | Self::PageUp | Self::PageDown => "Scrolling",
            Self::ShowHelp | Self::ShowSettings | Self::CopyServerToken | Self::Quit => {
                "Application"
            }
        }
    }
}

impl FromStr for BindableAction {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "navigate_up" => Ok(Self::NavigateUp),
            "navigate_down" => Ok(Self::NavigateDown),
            "next_group" => Ok(Self::NextGroup),
            "previous_group" => Ok(Self::PreviousGroup),
            "navigate_first" => Ok(Self::NavigateFirst),
            "navigate_last" => Ok(Self::NavigateLast),
            "navigate_left" => Ok(Self::NavigateLeft),
            "navigate_right" => Ok(Self::NavigateRight),
            "list_page_up" => Ok(Self::ListPageUp),
            "list_page_down" => Ok(Self::ListPageDown),
            "select" => Ok(Self::Select),
            "select_shell" => Ok(Self::SelectShell),
            "new_session" => Ok(Self::NewSession),
            "new_stacked_session" => Ok(Self::NewStackedSession),
            "set_session_base" => Ok(Self::SetSessionBase),
            "cascade_merge_main" => Ok(Self::CascadeMergeMain),
            "cascade_resume" => Ok(Self::CascadeResume),
            "cascade_abandon" => Ok(Self::CascadeAbandon),
            "push_stack" => Ok(Self::PushStack),
            "new_project" => Ok(Self::NewProject),
            "clone_repository" => Ok(Self::CloneRepository),
            "checkout_branch" => Ok(Self::CheckoutBranch),
            "delete_session" => Ok(Self::DeleteSession),
            "delete_merged_pr_sessions" => Ok(Self::DeleteMergedPrSessions),
            "edit_session" => Ok(Self::EditSession),
            "rename_session" => Ok(Self::RenameSession),
            "restart_session" => Ok(Self::RestartSession),
            "reset_session" => Ok(Self::ResetSession),
            "change_program" => Ok(Self::ChangeProgram),
            "toggle_keep_alive" => Ok(Self::ToggleKeepAlive),
            "remove_project" => Ok(Self::RemoveProject),
            "open_in_editor" => Ok(Self::OpenInEditor),
            "open_info" => Ok(Self::OpenInfo),
            "open_pull_request" => Ok(Self::OpenPullRequest),
            "refresh_pr_status" => Ok(Self::RefreshPrStatus),
            "open_commander" => Ok(Self::OpenCommander),
            "toggle_conversation_overlay" => Ok(Self::ToggleConversationOverlay),
            "toggle_voice_input" => Ok(Self::ToggleVoiceInput),
            "toggle_dictation" => Ok(Self::ToggleDictation),
            "open_review_diff" => Ok(Self::OpenReviewDiff),
            "show_help" => Ok(Self::ShowHelp),
            "show_settings" => Ok(Self::ShowSettings),
            "copy_server_token" => Ok(Self::CopyServerToken),
            "quit" => Ok(Self::Quit),
            "scroll_up" => Ok(Self::ScrollUp),
            "scroll_down" => Ok(Self::ScrollDown),
            "page_up" => Ok(Self::PageUp),
            "page_down" => Ok(Self::PageDown),
            "generate_summary" => Ok(Self::GenerateSummary),
            "scan_directory" => Ok(Self::ScanDirectory),
            "move_to_section" => Ok(Self::MoveToSection),
            "toggle_view_mode" => Ok(Self::ToggleViewMode),
            "toggle_section" => Ok(Self::ToggleSection),
            "next_workspace" => Ok(Self::NextWorkspace),
            "previous_workspace" => Ok(Self::PreviousWorkspace),
            "workspace_picker" => Ok(Self::WorkspacePicker),
            "new_workspace" => Ok(Self::NewWorkspace),
            "move_project_to_workspace" => Ok(Self::MoveProjectToWorkspace),
            "toggle_pane" => Ok(Self::TogglePane),
            "toggle_pane_reverse" => Ok(Self::TogglePaneReverse),
            "shrink_left_pane" => Ok(Self::ShrinkLeftPane),
            "grow_left_pane" => Ok(Self::GrowLeftPane),
            "edit_server_programs" => Ok(Self::EditServerPrograms),
            "add_remote_server" => Ok(Self::AddRemoteServer),
            "remove_remote_server" => Ok(Self::RemoveRemoteServer),
            _ => Err(format!("unknown action: {s}")),
        }
    }
}

// ---------------------------------------------------------------------------
// KeyBinding — a single key combination
// ---------------------------------------------------------------------------

/// A key combination that can be serialized to/from human-readable strings.
///
/// Format: `[Ctrl-][Alt-][Shift-]<key>`
///
/// Examples: `"k"`, `"Ctrl-p"`, `"Shift-N"`, `"Enter"`, `"F1"`, `"Ctrl-Shift-x"`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyBinding {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyBinding {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// Check whether a crossterm `KeyEvent` matches this binding.
    pub fn matches(&self, event: &KeyEvent) -> bool {
        event.code == self.code && event.modifiers == self.modifiers
    }
}

impl fmt::Display for KeyBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            f.write_str("Ctrl-")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            f.write_str("Alt-")?;
        }
        // We only show Shift prefix for non-character keys or lowercase chars.
        // Uppercase letters like 'N' already imply Shift.
        let show_shift = self.modifiers.contains(KeyModifiers::SHIFT)
            && !matches!(self.code, KeyCode::Char(c) if c.is_ascii_uppercase());
        if show_shift {
            f.write_str("Shift-")?;
        }

        match self.code {
            KeyCode::Char(' ') => f.write_str("Space"),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Enter => f.write_str("Enter"),
            KeyCode::Esc => f.write_str("Esc"),
            KeyCode::Tab => f.write_str("Tab"),
            KeyCode::BackTab => f.write_str("BackTab"),
            KeyCode::Backspace => f.write_str("Backspace"),
            KeyCode::Up => f.write_str("Up"),
            KeyCode::Down => f.write_str("Down"),
            KeyCode::Left => f.write_str("Left"),
            KeyCode::Right => f.write_str("Right"),
            KeyCode::PageUp => f.write_str("PageUp"),
            KeyCode::PageDown => f.write_str("PageDown"),
            KeyCode::Home => f.write_str("Home"),
            KeyCode::End => f.write_str("End"),
            KeyCode::Delete => f.write_str("Delete"),
            KeyCode::Insert => f.write_str("Insert"),
            KeyCode::F(n) => write!(f, "F{n}"),
            _ => f.write_str("???"),
        }
    }
}

impl FromStr for KeyBinding {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty key binding string".to_string());
        }

        let mut modifiers = KeyModifiers::NONE;
        let mut rest = s;

        // Parse modifier prefixes (case-insensitive)
        loop {
            let lower = rest.to_ascii_lowercase();
            if let Some(after) = lower.strip_prefix("ctrl-") {
                modifiers |= KeyModifiers::CONTROL;
                rest = &rest[5..]; // skip "Ctrl-"
                let _ = after; // used only for prefix check
            } else if let Some(after) = lower.strip_prefix("alt-") {
                modifiers |= KeyModifiers::ALT;
                rest = &rest[4..];
                let _ = after;
            } else if let Some(after) = lower.strip_prefix("shift-") {
                modifiers |= KeyModifiers::SHIFT;
                rest = &rest[6..];
                let _ = after;
            } else {
                break;
            }
        }

        // Parse the key name
        let code = match rest.to_ascii_lowercase().as_str() {
            "enter" | "return" | "cr" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "backspace" | "bs" => KeyCode::Backspace,
            "space" => KeyCode::Char(' '),
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            // `>= 2` so the bare key "f" falls through to the
            // single-character arm instead of failing to parse.
            f if f.starts_with('f') && (2..=3).contains(&f.len()) => {
                let n: u8 = f[1..]
                    .parse()
                    .map_err(|_| format!("invalid function key: {rest}"))?;
                if !(1..=12).contains(&n) {
                    return Err(format!("function key out of range: F{n}"));
                }
                KeyCode::F(n)
            }
            _ => {
                // Single character
                let chars: Vec<char> = rest.chars().collect();
                if chars.len() != 1 {
                    return Err(format!("unknown key name: {rest}"));
                }
                let c = chars[0];
                // Uppercase implies Shift modifier
                if c.is_ascii_uppercase() {
                    modifiers |= KeyModifiers::SHIFT;
                }
                KeyCode::Char(c)
            }
        };

        Ok(KeyBinding { code, modifiers })
    }
}

impl Serialize for KeyBinding {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for KeyBinding {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        KeyBinding::from_str(&s).map_err(de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// KeyBindings — the full binding table
// ---------------------------------------------------------------------------

/// Complete keybinding configuration.
///
/// Loaded from `[keybindings]` in config.toml. Missing keys fall back to
/// defaults. The internal lookup table is built on construction.
#[derive(Debug, Clone)]
pub struct KeyBindings {
    /// Action → list of bound keys (source of truth, serialized to TOML)
    bindings: HashMap<BindableAction, Vec<KeyBinding>>,
    /// (KeyCode, KeyModifiers) → action (derived lookup table, not serialized)
    lookup: HashMap<(KeyCode, KeyModifiers), BindableAction>,
}

impl KeyBindings {
    /// Build the lookup table from the bindings map.
    fn build_lookup(
        bindings: &HashMap<BindableAction, Vec<KeyBinding>>,
    ) -> HashMap<(KeyCode, KeyModifiers), BindableAction> {
        let mut lookup = HashMap::new();
        for (action, keys) in bindings {
            for key in keys {
                lookup.insert((key.code, key.modifiers), *action);
            }
        }
        lookup
    }

    /// Resolve a crossterm key event to a bindable action.
    ///
    /// Returns `None` if the key is not bound to any action. Structural
    /// keys (text input, backspace in modals) are handled separately.
    pub fn resolve(&self, event: &KeyEvent) -> Option<BindableAction> {
        if event.kind != KeyEventKind::Press {
            return None;
        }
        self.lookup
            .get(&(event.code, event.modifiers))
            .copied()
            .or_else(|| {
                // Uppercase-only terminal events and explicit Shift events
                // resolve the same binding. The regression test
                // `shifted_uppercase_resolves_with_or_without_shift_metadata`
                // also pins exact-match precedence and modifier isolation.
                if let KeyCode::Char(c) = event.code
                    && c.is_ascii_uppercase()
                    && event.modifiers.is_empty()
                {
                    self.lookup.get(&(event.code, KeyModifiers::SHIFT)).copied()
                } else {
                    None
                }
            })
    }

    /// Get the key bindings for a specific action.
    pub fn keys_for(&self, action: BindableAction) -> &[KeyBinding] {
        self.bindings.get(&action).map_or(&[], |v| v.as_slice())
    }

    /// Replace the bindings for `action` and rebuild the lookup table.
    pub fn set_keys_for(&mut self, action: BindableAction, keys: Vec<KeyBinding>) {
        self.bindings.insert(action, keys);
        self.lookup = Self::build_lookup(&self.bindings);
    }

    /// Format the key bindings for an action as a comma-separated string.
    pub fn keys_display(&self, action: BindableAction) -> String {
        self.keys_for(action)
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// All actions grouped by section, in display order.
    pub fn sections(&self) -> Vec<(&'static str, Vec<(BindableAction, String)>)> {
        let mut sections: Vec<(&str, Vec<(BindableAction, String)>)> = Vec::new();
        let mut current_section = "";

        for &action in BindableAction::ALL {
            let section = action.section();
            if section != current_section {
                sections.push((section, Vec::new()));
                current_section = section;
            }
            let keys = self.keys_display(action);
            sections.last_mut().unwrap().1.push((action, keys));
        }

        sections
    }
}

impl Default for KeyBindings {
    fn default() -> Self {
        let mut bindings = HashMap::new();

        let kb = |code: KeyCode, modifiers: KeyModifiers| KeyBinding::new(code, modifiers);
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let shift = KeyModifiers::SHIFT;
        let alt = KeyModifiers::ALT;

        // Navigation
        bindings.insert(
            BindableAction::NavigateUp,
            vec![
                kb(KeyCode::Char('k'), none),
                kb(KeyCode::Up, none),
                kb(KeyCode::Char('p'), ctrl),
            ],
        );
        bindings.insert(
            BindableAction::NavigateDown,
            vec![
                kb(KeyCode::Char('j'), none),
                kb(KeyCode::Down, none),
                kb(KeyCode::Char('n'), ctrl),
            ],
        );
        bindings.insert(
            BindableAction::NextGroup,
            vec![kb(KeyCode::Char(']'), none)],
        );
        bindings.insert(
            BindableAction::PreviousGroup,
            vec![kb(KeyCode::Char('['), none)],
        );
        bindings.insert(BindableAction::NavigateFirst, vec![kb(KeyCode::Home, none)]);
        bindings.insert(BindableAction::NavigateLast, vec![kb(KeyCode::End, none)]);
        bindings.insert(
            BindableAction::NavigateLeft,
            vec![kb(KeyCode::Char('h'), none), kb(KeyCode::Left, none)],
        );
        bindings.insert(
            BindableAction::NavigateRight,
            vec![kb(KeyCode::Char('l'), none), kb(KeyCode::Right, none)],
        );
        // PgUp/PgDn page the session list / board column; `PageUp`/`PageDown`
        // (Ctrl-u/Ctrl-d) jump to the column's first/last card instead.
        bindings.insert(BindableAction::ListPageUp, vec![kb(KeyCode::PageUp, none)]);
        bindings.insert(
            BindableAction::ListPageDown,
            vec![kb(KeyCode::PageDown, none)],
        );
        bindings.insert(BindableAction::Select, vec![kb(KeyCode::Enter, none)]);

        // Session management
        bindings.insert(
            BindableAction::SelectShell,
            vec![kb(KeyCode::Char('s'), none)],
        );
        bindings.insert(
            BindableAction::NewSession,
            vec![kb(KeyCode::Char('n'), none)],
        );
        bindings.insert(
            BindableAction::NewStackedSession,
            vec![kb(KeyCode::Char('t'), none)],
        );
        bindings.insert(
            BindableAction::NewProject,
            vec![kb(KeyCode::Char('N'), shift)],
        );
        bindings.insert(
            BindableAction::CheckoutBranch,
            vec![kb(KeyCode::Char('c'), none)],
        );
        bindings.insert(
            BindableAction::DeleteSession,
            vec![kb(KeyCode::Char('d'), none)],
        );
        bindings.insert(
            BindableAction::EditSession,
            vec![kb(KeyCode::Char('E'), shift)],
        );
        // RenameSession has no default key — it's reachable via the command
        // palette. `r` is given to OpenReviewDiff so it pairs with the
        // attached-session Alt-r review toggle.
        bindings.insert(
            BindableAction::RestartSession,
            vec![kb(KeyCode::Char('R'), shift)],
        );
        // ResetSession has no default key — it's reachable via the command
        // palette and can be bound explicitly in config. It throws away the
        // agent's conversation, so keeping it off Shift-R's neighbouring keys is
        // deliberate: a mistyped restart should not be a destructive one.
        // ToggleKeepAlive has no default key — it's reachable via the command
        // palette and can be bound explicitly in config. Keep-alive is a rarely
        // toggled, opt-in control, so it doesn't claim a top-level hotkey.
        bindings.insert(
            BindableAction::RemoveProject,
            vec![kb(KeyCode::Char('D'), shift)],
        );
        bindings.insert(
            BindableAction::OpenInEditor,
            vec![kb(KeyCode::Char('.'), none), kb(KeyCode::Char('.'), ctrl)],
        );
        bindings.insert(
            BindableAction::OpenPullRequest,
            vec![kb(KeyCode::Char('o'), none)],
        );
        bindings.insert(BindableAction::OpenInfo, vec![kb(KeyCode::Char('i'), none)]);
        bindings.insert(
            BindableAction::OpenCommander,
            vec![kb(KeyCode::Char('C'), shift)],
        );
        bindings.insert(
            BindableAction::ToggleConversationOverlay,
            vec![kb(KeyCode::Char('c'), alt)],
        );
        bindings.insert(
            BindableAction::ToggleVoiceInput,
            vec![kb(KeyCode::Char('v'), alt)],
        );
        // Alt-t is intercepted on a *shell* pane too, unlike Alt-v — dictation
        // types into whatever pane is attached, so it has to be reachable from
        // both. That costs two things the user may want back, and both are
        // rebindable here: readline's `transpose-words` (Alt-t in a shell) and
        // Claude Code's own Alt+T thinking toggle.
        bindings.insert(
            BindableAction::ToggleDictation,
            vec![kb(KeyCode::Char('t'), alt)],
        );
        bindings.insert(
            BindableAction::OpenReviewDiff,
            vec![kb(KeyCode::Char('r'), none), kb(KeyCode::Char('r'), alt)],
        );
        bindings.insert(
            BindableAction::MoveToSection,
            vec![kb(KeyCode::Char('m'), none)],
        );
        bindings.insert(
            BindableAction::ToggleViewMode,
            vec![kb(KeyCode::Char('v'), none)],
        );
        bindings.insert(BindableAction::ToggleSection, vec![]);

        // Workspaces. `w` cycles (wrapping), `W` opens the picker; the rest are
        // palette-only. Deliberately not Ctrl-w (a terminal's delete-word),
        // Ctrl-Tab (rarely delivered by terminals) or Alt-1..9 (tab switching
        // in most terminal emulators).
        bindings.insert(
            BindableAction::NextWorkspace,
            vec![kb(KeyCode::Char('w'), none)],
        );
        bindings.insert(
            BindableAction::WorkspacePicker,
            vec![kb(KeyCode::Char('W'), shift)],
        );
        bindings.insert(BindableAction::PreviousWorkspace, vec![]);
        bindings.insert(BindableAction::NewWorkspace, vec![]);
        bindings.insert(BindableAction::MoveProjectToWorkspace, vec![]);

        // Right pane (list views only)
        bindings.insert(BindableAction::TogglePane, vec![kb(KeyCode::Tab, none)]);
        bindings.insert(
            BindableAction::TogglePaneReverse,
            vec![kb(KeyCode::BackTab, shift)],
        );
        // `<` / `>` need both modifier forms registered: they are shifted
        // characters, and terminals disagree on whether they also report SHIFT.
        bindings.insert(
            BindableAction::ShrinkLeftPane,
            vec![kb(KeyCode::Char('<'), shift), kb(KeyCode::Char('<'), none)],
        );
        bindings.insert(
            BindableAction::GrowLeftPane,
            vec![kb(KeyCode::Char('>'), shift), kb(KeyCode::Char('>'), none)],
        );

        // Scrolling
        bindings.insert(BindableAction::ScrollUp, vec![]);
        bindings.insert(BindableAction::ScrollDown, vec![]);
        bindings.insert(BindableAction::PageUp, vec![kb(KeyCode::Char('u'), ctrl)]);
        bindings.insert(BindableAction::PageDown, vec![kb(KeyCode::Char('d'), ctrl)]);

        bindings.insert(
            BindableAction::ScanDirectory,
            vec![kb(KeyCode::Char('S'), shift)],
        );

        // Info Pane
        bindings.insert(
            BindableAction::GenerateSummary,
            vec![kb(KeyCode::Char('g'), none)],
        );

        // Other
        bindings.insert(
            BindableAction::ShowHelp,
            vec![kb(KeyCode::Char('?'), shift), kb(KeyCode::Char('?'), none)],
        );
        bindings.insert(
            BindableAction::ShowSettings,
            vec![kb(KeyCode::Char(','), none)],
        );
        bindings.insert(
            BindableAction::Quit,
            vec![kb(KeyCode::Char('q'), none), kb(KeyCode::Char('c'), ctrl)],
        );

        let lookup = Self::build_lookup(&bindings);
        Self { bindings, lookup }
    }
}

// ---------------------------------------------------------------------------
// Serde: serialize as { action_name = ["key1", "key2"] }
// ---------------------------------------------------------------------------

impl Serialize for KeyBindings {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(self.bindings.len()))?;
        // Serialize in a stable order
        let mut entries: Vec<_> = self.bindings.iter().collect();
        entries.sort_by_key(|(action, _)| action.config_name());
        for (action, keys) in entries {
            map.serialize_entry(action.config_name(), keys)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for KeyBindings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_map(KeyBindingsVisitor)
    }
}

struct KeyBindingsVisitor;

impl<'de> Visitor<'de> for KeyBindingsVisitor {
    type Value = KeyBindings;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a keybindings map (action_name = [\"key1\", \"key2\"])")
    }

    fn visit_map<A: de::MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        // Start from defaults, then overlay user-specified bindings
        let mut result = KeyBindings::default();

        while let Some(key) = map.next_key::<String>()? {
            match BindableAction::from_str(&key) {
                Ok(action) => {
                    let keys: OneOrMany = map.next_value()?;
                    result.bindings.insert(action, keys.0);
                }
                Err(_) => {
                    // Skip unknown actions (e.g. `pause_session` / `resume_session`
                    // from configs written before the feature was removed).
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }

        // Rebuild lookup after applying overrides
        result.lookup = KeyBindings::build_lookup(&result.bindings);
        Ok(result)
    }
}

/// Helper to accept either a single string or an array of strings for each action.
struct OneOrMany(Vec<KeyBinding>);

impl<'de> Deserialize<'de> for OneOrMany {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_any(OneOrManyVisitor)
    }
}

struct OneOrManyVisitor;

impl<'de> Visitor<'de> for OneOrManyVisitor {
    type Value = OneOrMany;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a key binding string or array of key binding strings")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
        let kb = KeyBinding::from_str(value).map_err(E::custom)?;
        Ok(OneOrMany(vec![kb]))
    }

    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut keys = Vec::new();
        while let Some(s) = seq.next_element::<String>()? {
            keys.push(KeyBinding::from_str(&s).map_err(de::Error::custom)?);
        }
        Ok(OneOrMany(keys))
    }
}

// ---------------------------------------------------------------------------
// Raw-byte encoding for detecting bindings inside a raw tmux attach session
// ---------------------------------------------------------------------------

/// Compute the raw byte sequences that should trigger `action` while forwarding
/// stdin to a tmux PTY.
///
/// Inside a tmux attach we read raw bytes rather than decoded key events, so
/// only bindings with a well-defined encoding are detectable:
///
/// * `Ctrl-<letter>` → a single control byte (e.g. `Ctrl-e` → 0x05).
/// * `Ctrl-<non-letter>` → the standard CSI-u ("fixterms") encoding
///   `ESC [ N ; 5 u` and the xterm modifyOtherKeys encoding
///   `ESC [ 27 ; 5 ; N ~`, where `N` is the ASCII code. These only arrive on
///   stdin if the outer terminal has the matching protocol enabled, so
///   non-letter Ctrl bindings rely on the user's terminal configuration.
///
/// Bare bindings (no modifiers) are intentionally skipped — they are
/// indistinguishable from ordinary keystrokes being typed into the attached
/// program, so intercepting them would prevent the user from typing that key.
fn trigger_bytes_for(bindings: &KeyBindings, action: BindableAction) -> Vec<Vec<u8>> {
    let mut triggers = Vec::new();
    for kb in bindings.keys_for(action) {
        let KeyCode::Char(c) = kb.code else {
            continue;
        };
        if !c.is_ascii() {
            continue;
        }
        match kb.modifiers {
            KeyModifiers::CONTROL if c.is_ascii_alphabetic() => {
                // Ctrl-<letter> → control byte
                let byte = (c.to_ascii_lowercase() as u8) - 0x60;
                triggers.push(vec![byte]);
            }
            KeyModifiers::CONTROL => {
                // Ctrl-<non-letter> → CSI-u or modifyOtherKeys sequence
                let n = c as u32;
                triggers.push(format!("\x1b[{n};5u").into_bytes());
                triggers.push(format!("\x1b[27;5;{n}~").into_bytes());
            }
            KeyModifiers::ALT => {
                // Alt-<char> → ESC prefix + the literal byte (the standard
                // metaSendsEscape encoding terminals emit for Meta/Alt).
                triggers.push(vec![0x1b, c as u8]);
            }
            _ => continue,
        }
    }
    triggers
}

/// Raw stdin byte patterns that open the editor mid-attach (from the
/// [`OpenInEditor`](BindableAction::OpenInEditor) binding). See
/// [`trigger_bytes_for`].
pub fn editor_trigger_bytes(bindings: &KeyBindings) -> Vec<Vec<u8>> {
    trigger_bytes_for(bindings, BindableAction::OpenInEditor)
}

/// Raw stdin byte patterns that switch from an attached session to its review
/// diff (from the [`OpenReviewDiff`](BindableAction::OpenReviewDiff) binding —
/// `Alt-r` by default, encoded as the `ESC r` metaSendsEscape sequence). See
/// [`trigger_bytes_for`].
pub fn review_trigger_bytes(bindings: &KeyBindings) -> Vec<Vec<u8>> {
    trigger_bytes_for(bindings, BindableAction::OpenReviewDiff)
}

/// Raw stdin byte patterns that toggle voice input mid-attach (from the
/// [`ToggleVoiceInput`](BindableAction::ToggleVoiceInput) binding — `Alt-v` by
/// default, encoded as the `ESC v` metaSendsEscape sequence). See
/// [`trigger_bytes_for`]. Unlike the review/editor triggers these don't exit the
/// attach — the attach loop swallows them and toggles the mic in place.
pub fn voice_trigger_bytes(bindings: &KeyBindings) -> Vec<Vec<u8>> {
    trigger_bytes_for(bindings, BindableAction::ToggleVoiceInput)
}

/// Raw stdin byte patterns that toggle dictation mid-attach (from the
/// [`ToggleDictation`](BindableAction::ToggleDictation) binding — `Alt-t` by
/// default, encoded as the `ESC t` metaSendsEscape sequence). See
/// [`trigger_bytes_for`]. Like the voice trigger these don't exit the attach —
/// the attach loop swallows them and arms the mic in place — but unlike it they
/// are intercepted on a shell pane as well as an agent one, because dictation
/// types into whichever pane is on screen.
pub fn dictation_trigger_bytes(bindings: &KeyBindings) -> Vec<Vec<u8>> {
    trigger_bytes_for(bindings, BindableAction::ToggleDictation)
}

/// Whether a binding survives the [`trigger_bytes_for`] filter: a Ctrl- or
/// Alt-modified ASCII character. Bare bindings can't be intercepted mid-attach
/// (they're indistinguishable from typing), so they never form the toggle.
fn is_attach_interceptable(kb: &KeyBinding) -> bool {
    matches!(kb.code, KeyCode::Char(c) if c.is_ascii())
        && matches!(kb.modifiers, KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// The first [`OpenReviewDiff`](BindableAction::OpenReviewDiff) binding that
/// also works while attached to a session (`Alt-r` by default) — the key that
/// toggles between an attached session and its review diff. The review view
/// honours the same key for the way back and labels it in its footer.
pub fn review_toggle_binding(bindings: &KeyBindings) -> Option<&KeyBinding> {
    bindings
        .keys_for(BindableAction::OpenReviewDiff)
        .iter()
        .find(|kb| is_attach_interceptable(kb))
}

/// Whether `key` is one of the attach-capable
/// [`OpenReviewDiff`](BindableAction::OpenReviewDiff) bindings (see
/// [`review_toggle_binding`]).
pub fn matches_review_toggle(bindings: &KeyBindings, key: &KeyEvent) -> bool {
    bindings
        .keys_for(BindableAction::OpenReviewDiff)
        .iter()
        .filter(|kb| is_attach_interceptable(kb))
        .any(|kb| kb.code == key.code && key.modifiers.contains(kb.modifiers))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Button labels --

    #[test]
    fn button_label_nonempty_for_every_action() {
        for &action in BindableAction::ALL {
            assert!(
                !action.button_label().is_empty(),
                "empty button_label for {action:?}"
            );
        }
    }

    #[test]
    fn button_label_distinct_from_description() {
        // The button label is intentionally terser than the help description.
        assert_ne!(
            BindableAction::NewSession.button_label(),
            BindableAction::NewSession.description()
        );
    }

    // -- KeyBinding parsing --

    #[test]
    fn test_parse_simple_char() {
        let kb: KeyBinding = "k".parse().unwrap();
        assert_eq!(kb.code, KeyCode::Char('k'));
        assert_eq!(kb.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn test_parse_uppercase_implies_shift() {
        let kb: KeyBinding = "N".parse().unwrap();
        assert_eq!(kb.code, KeyCode::Char('N'));
        assert_eq!(kb.modifiers, KeyModifiers::SHIFT);
    }

    #[test]
    fn test_parse_ctrl_modifier() {
        let kb: KeyBinding = "Ctrl-p".parse().unwrap();
        assert_eq!(kb.code, KeyCode::Char('p'));
        assert_eq!(kb.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn test_parse_ctrl_case_insensitive() {
        let kb: KeyBinding = "ctrl-p".parse().unwrap();
        assert_eq!(kb.code, KeyCode::Char('p'));
        assert_eq!(kb.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn test_parse_special_keys() {
        assert_eq!("Enter".parse::<KeyBinding>().unwrap().code, KeyCode::Enter);
        assert_eq!("Esc".parse::<KeyBinding>().unwrap().code, KeyCode::Esc);
        assert_eq!("Tab".parse::<KeyBinding>().unwrap().code, KeyCode::Tab);
        assert_eq!(
            "BackTab".parse::<KeyBinding>().unwrap().code,
            KeyCode::BackTab
        );
        assert_eq!(
            "Space".parse::<KeyBinding>().unwrap().code,
            KeyCode::Char(' ')
        );
        assert_eq!("Up".parse::<KeyBinding>().unwrap().code, KeyCode::Up);
        assert_eq!("Down".parse::<KeyBinding>().unwrap().code, KeyCode::Down);
        assert_eq!(
            "PageUp".parse::<KeyBinding>().unwrap().code,
            KeyCode::PageUp
        );
        assert_eq!(
            "PageDown".parse::<KeyBinding>().unwrap().code,
            KeyCode::PageDown
        );
    }

    #[test]
    fn test_parse_function_keys() {
        let kb: KeyBinding = "F1".parse().unwrap();
        assert_eq!(kb.code, KeyCode::F(1));
        let kb: KeyBinding = "F12".parse().unwrap();
        assert_eq!(kb.code, KeyCode::F(12));
    }

    #[test]
    fn test_parse_function_key_out_of_range() {
        assert!("F0".parse::<KeyBinding>().is_err());
        assert!("F13".parse::<KeyBinding>().is_err());
    }

    #[test]
    fn test_parse_bare_f_is_a_character_not_a_function_key() {
        // Regression: the function-key arm captured the bare key "f"
        // ("invalid function key: f"), and because keybinding parse errors
        // fail Config::load(), one `= "f"` binding silently reverted the
        // user's entire config to defaults.
        let kb: KeyBinding = "f".parse().unwrap();
        assert_eq!(kb.code, KeyCode::Char('f'));
        assert!(kb.modifiers.is_empty());
    }

    #[test]
    fn test_parse_modified_f_is_a_character_not_a_function_key() {
        // Modifiers are stripped before the key-name match, so these all
        // reduced to the same broken "f" case.
        let kb: KeyBinding = "Ctrl-f".parse().unwrap();
        assert_eq!(kb.code, KeyCode::Char('f'));
        assert!(kb.modifiers.contains(KeyModifiers::CONTROL));

        let kb: KeyBinding = "Alt-f".parse().unwrap();
        assert_eq!(kb.code, KeyCode::Char('f'));
        assert!(kb.modifiers.contains(KeyModifiers::ALT));

        // Uppercase implies Shift, same as every other letter (cf. "N").
        let kb: KeyBinding = "F".parse().unwrap();
        assert_eq!(kb.code, KeyCode::Char('F'));
        assert!(kb.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn test_parse_multiple_modifiers() {
        let kb: KeyBinding = "Ctrl-Shift-x".parse().unwrap();
        assert_eq!(kb.code, KeyCode::Char('x'));
        assert!(kb.modifiers.contains(KeyModifiers::CONTROL));
        assert!(kb.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn test_parse_empty_errors() {
        assert!("".parse::<KeyBinding>().is_err());
    }

    #[test]
    fn test_parse_unknown_key_errors() {
        assert!("FooBar".parse::<KeyBinding>().is_err());
    }

    // -- Display round-trip --

    #[test]
    fn test_display_round_trip() {
        let cases = ["k", "Ctrl-p", "Enter", "Tab", "F1", "Up", "PageUp", "Space"];
        for input in cases {
            let kb: KeyBinding = input.parse().unwrap();
            let displayed = kb.to_string();
            let reparsed: KeyBinding = displayed.parse().unwrap();
            assert_eq!(kb, reparsed, "round-trip failed for {input}");
        }
    }

    #[test]
    fn test_display_uppercase() {
        let kb: KeyBinding = "N".parse().unwrap();
        assert_eq!(kb.to_string(), "N");
    }

    #[test]
    fn test_display_ctrl() {
        let kb = KeyBinding::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(kb.to_string(), "Ctrl-c");
    }

    // -- KeyBindings defaults --

    #[test]
    fn test_default_new_stacked_session_bound_to_t() {
        let kb = KeyBindings::default();
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&key), Some(BindableAction::NewStackedSession));
    }

    #[test]
    fn test_navigate_left_right_default_bindings() {
        let kb = KeyBindings::default();
        for (code, action) in [
            (KeyCode::Char('h'), BindableAction::NavigateLeft),
            (KeyCode::Left, BindableAction::NavigateLeft),
            (KeyCode::Char('l'), BindableAction::NavigateRight),
            (KeyCode::Right, BindableAction::NavigateRight),
        ] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            assert_eq!(kb.resolve(&key), Some(action), "{code:?}");
        }
    }

    #[test]
    fn test_navigate_left_right_config_names_round_trip() {
        for action in [BindableAction::NavigateLeft, BindableAction::NavigateRight] {
            assert_eq!(
                BindableAction::from_str(action.config_name()).unwrap(),
                action
            );
        }
    }

    #[test]
    fn test_open_info_default_bound_to_i() {
        let kb = KeyBindings::default();
        let key = KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&key), Some(BindableAction::OpenInfo));
    }

    #[test]
    fn test_open_info_config_name_round_trips() {
        assert_eq!(
            BindableAction::from_str(BindableAction::OpenInfo.config_name()).unwrap(),
            BindableAction::OpenInfo
        );
        assert_eq!(BindableAction::OpenInfo.config_name(), "open_info");
    }

    #[test]
    fn test_default_move_to_section_bound_to_m() {
        let kb = KeyBindings::default();
        let key = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&key), Some(BindableAction::MoveToSection));
    }

    #[test]
    fn test_toggle_view_mode_default_bound_to_v() {
        let kb = KeyBindings::default();
        let key = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&key), Some(BindableAction::ToggleViewMode));
    }

    #[test]
    fn test_open_commander_default_bound_to_shift_c() {
        let kb = KeyBindings::default();
        let key = KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT);
        assert_eq!(kb.resolve(&key), Some(BindableAction::OpenCommander));
    }

    #[test]
    fn test_toggle_conversation_overlay_default_bound_to_alt_c() {
        let kb = KeyBindings::default();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT);
        assert_eq!(
            kb.resolve(&key),
            Some(BindableAction::ToggleConversationOverlay)
        );
    }

    #[test]
    fn test_toggle_voice_input_default_bound_to_alt_v() {
        let kb = KeyBindings::default();
        let key = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT);
        assert_eq!(kb.resolve(&key), Some(BindableAction::ToggleVoiceInput));
    }

    #[test]
    fn test_toggle_voice_input_config_name_roundtrips() {
        assert_eq!(
            "toggle_voice_input".parse::<BindableAction>().unwrap(),
            BindableAction::ToggleVoiceInput
        );
        assert_eq!(
            BindableAction::ToggleVoiceInput.config_name(),
            "toggle_voice_input"
        );
    }

    #[test]
    fn test_toggle_dictation_default_bound_to_alt_t() {
        let kb = KeyBindings::default();
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT);
        assert_eq!(kb.resolve(&key), Some(BindableAction::ToggleDictation));
    }

    #[test]
    fn test_toggle_dictation_config_name_roundtrips() {
        assert_eq!(
            "toggle_dictation".parse::<BindableAction>().unwrap(),
            BindableAction::ToggleDictation
        );
        assert_eq!(
            BindableAction::ToggleDictation.config_name(),
            "toggle_dictation"
        );
    }

    #[test]
    fn test_open_commander_round_trips_through_from_str() {
        let name = BindableAction::OpenCommander.config_name();
        assert_eq!(
            BindableAction::from_str(name).unwrap(),
            BindableAction::OpenCommander
        );
    }

    #[test]
    fn test_toggle_section_default_unbound() {
        let kb = KeyBindings::default();
        assert!(kb.keys_for(BindableAction::ToggleSection).is_empty());
    }

    #[test]
    fn test_cascade_actions_round_trip_through_from_str() {
        // Palette-only actions must still survive TOML round-tripping so a
        // user who binds them in config doesn't hit "unknown action".
        for action in [
            BindableAction::CascadeMergeMain,
            BindableAction::CascadeResume,
            BindableAction::CascadeAbandon,
            BindableAction::PushStack,
        ] {
            let name = action.config_name();
            assert_eq!(BindableAction::from_str(name).unwrap(), action);
        }
    }

    #[test]
    fn test_cascade_actions_default_to_unbound() {
        // They're palette-only; no default hotkey should resolve to them.
        let kb = KeyBindings::default();
        assert!(kb.keys_for(BindableAction::CascadeMergeMain).is_empty());
        assert!(kb.keys_for(BindableAction::CascadeResume).is_empty());
        assert!(kb.keys_for(BindableAction::CascadeAbandon).is_empty());
        assert!(kb.keys_for(BindableAction::PushStack).is_empty());
    }

    #[test]
    fn test_refresh_pr_status_palette_only() {
        // Palette-only: no default hotkey resolves to it, but it must still
        // round-trip through TOML so a user who binds it doesn't hit
        // "unknown action".
        let kb = KeyBindings::default();
        assert!(kb.keys_for(BindableAction::RefreshPrStatus).is_empty());
        assert_eq!(
            "refresh_pr_status".parse::<BindableAction>().unwrap(),
            BindableAction::RefreshPrStatus
        );
        assert_eq!(
            BindableAction::RefreshPrStatus.config_name(),
            "refresh_pr_status"
        );
    }

    #[test]
    fn test_remote_server_actions_palette_only() {
        // Palette-only: no default hotkey resolves to them, but they must
        // still round-trip through TOML so a user who binds them doesn't hit
        // "unknown action".
        let kb = KeyBindings::default();
        for (action, name) in [
            (BindableAction::AddRemoteServer, "add_remote_server"),
            (BindableAction::RemoveRemoteServer, "remove_remote_server"),
            (BindableAction::EditServerPrograms, "edit_server_programs"),
        ] {
            assert!(kb.keys_for(action).is_empty());
            assert_eq!(name.parse::<BindableAction>().unwrap(), action);
            assert_eq!(action.config_name(), name);
        }
    }

    #[test]
    fn test_copy_server_token_palette_only() {
        // Palette-only: pairing a client is a once-per-device action, and the
        // palette is this project's canonical command surface. It still has to
        // round-trip through TOML so a user who binds it doesn't hit "unknown
        // action".
        let kb = KeyBindings::default();
        assert!(kb.keys_for(BindableAction::CopyServerToken).is_empty());
        assert_eq!(
            "copy_server_token".parse::<BindableAction>().unwrap(),
            BindableAction::CopyServerToken
        );
        assert_eq!(
            BindableAction::CopyServerToken.config_name(),
            "copy_server_token"
        );
        assert_eq!(BindableAction::CopyServerToken.section(), "Application");
        assert!(BindableAction::ALL.contains(&BindableAction::CopyServerToken));
    }

    #[test]
    fn test_clone_repository_palette_only() {
        // Palette-only, and deliberately so: the repo picker is a rare,
        // deliberate action, and the palette is this project's canonical
        // command surface. It must still round-trip through TOML so a user who
        // binds it doesn't hit "unknown action".
        let kb = KeyBindings::default();
        assert!(kb.keys_for(BindableAction::CloneRepository).is_empty());
        assert_eq!(
            "clone_repository".parse::<BindableAction>().unwrap(),
            BindableAction::CloneRepository
        );
        assert_eq!(
            BindableAction::CloneRepository.config_name(),
            "clone_repository"
        );
        // Grouped with the other project-level actions so the help screen and
        // the settings Keybindings tab list it where a user would look.
        assert_eq!(BindableAction::CloneRepository.section(), "Projects");
        assert!(BindableAction::ALL.contains(&BindableAction::CloneRepository));
    }

    #[test]
    fn test_defaults_match_current_bindings() {
        let kb = KeyBindings::default();

        // Navigation
        let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&j), Some(BindableAction::NavigateDown));

        let k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&k), Some(BindableAction::NavigateUp));

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(kb.resolve(&up), Some(BindableAction::NavigateUp));

        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(kb.resolve(&ctrl_p), Some(BindableAction::NavigateUp));

        let rbracket = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&rbracket), Some(BindableAction::NextGroup));

        let lbracket = KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&lbracket), Some(BindableAction::PreviousGroup));

        let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(kb.resolve(&home), Some(BindableAction::NavigateFirst));

        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(kb.resolve(&end), Some(BindableAction::NavigateLast));

        // Session management
        let n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&n), Some(BindableAction::NewSession));

        let shift_n = KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT);
        assert_eq!(kb.resolve(&shift_n), Some(BindableAction::NewProject));

        let shift_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert_eq!(kb.resolve(&shift_r), Some(BindableAction::RestartSession));

        // Quit
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&q), Some(BindableAction::Quit));

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(kb.resolve(&ctrl_c), Some(BindableAction::Quit));
    }

    #[test]
    fn test_release_events_ignored() {
        let kb = KeyBindings::default();
        let key = KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::empty(),
        };
        assert_eq!(kb.resolve(&key), None);
    }

    #[test]
    fn test_unbound_key_returns_none() {
        let kb = KeyBindings::default();
        let f1 = KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&f1), None);
    }

    // -- Serde --

    #[test]
    fn test_toml_deserialization_override() {
        let toml = r#"
            quit = ["Esc"]
            navigate_up = "y"
        "#;

        let kb: KeyBindings = toml::from_str(toml).unwrap();

        // Overridden: quit is now only Esc
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(kb.resolve(&esc), Some(BindableAction::Quit));

        // Old quit binding should be gone
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_ne!(kb.resolve(&q), Some(BindableAction::Quit));

        // Overridden: navigate_up is now 'y' (single string, not array)
        let w = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&w), Some(BindableAction::NavigateUp));

        // Non-overridden defaults still work
        let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&j), Some(BindableAction::NavigateDown));
    }

    #[test]
    fn test_toml_round_trip() {
        let original = KeyBindings::default();
        let serialized = toml::to_string_pretty(&original).unwrap();
        let deserialized: KeyBindings = toml::from_str(&serialized).unwrap();

        // All default actions should resolve identically
        for action in BindableAction::ALL {
            assert_eq!(
                original.keys_for(*action),
                deserialized.keys_for(*action),
                "mismatch for {:?}",
                action
            );
        }
    }

    #[test]
    fn test_unknown_action_skipped() {
        // Unknown action names (e.g. removed features like `pause_session`) are
        // silently ignored so old configs don't break.
        let toml = r#"
            nonexistent_action = ["k"]
            navigate_up = "y"
        "#;
        let kb: KeyBindings = toml::from_str(toml).unwrap();
        // Recognised override still applied
        let w = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&w), Some(BindableAction::NavigateUp));
    }

    // -- BindableAction --

    #[test]
    fn test_action_from_str_round_trip() {
        for action in BindableAction::ALL {
            let name = action.config_name();
            let parsed = BindableAction::from_str(name).unwrap();
            assert_eq!(*action, parsed);
        }
    }

    #[test]
    fn test_sections_are_ordered_and_contiguous() {
        // `sections()` slices `ALL` on section-boundary changes, so each section
        // must appear exactly once — a section whose actions are scattered
        // through `ALL` would show up as duplicate groups.
        let kb = KeyBindings::default();
        let names: Vec<&str> = kb.sections().iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "Navigation",
                "Sessions",
                "Stacked & Cascade",
                "Projects",
                "Pull Requests",
                "Remote Servers",
                "Sections",
                "Workspaces",
                "Right Pane",
                "Review & AI",
                "Scrolling",
                "Application",
            ],
        );

        // Every action is grouped, and no group is empty.
        let grouped: usize = kb.sections().iter().map(|(_, a)| a.len()).sum();
        assert_eq!(grouped, BindableAction::ALL.len());
        assert!(kb.sections().iter().all(|(_, a)| !a.is_empty()));
    }

    #[test]
    fn test_keys_display() {
        let kb = KeyBindings::default();
        let display = kb.keys_display(BindableAction::NavigateUp);
        assert!(display.contains("k"));
        assert!(display.contains("Up"));
        assert!(display.contains("Ctrl-p"));
    }

    #[test]
    fn test_editor_trigger_bytes_ctrl_letter() {
        let mut kb = KeyBindings::default();
        kb.set_keys_for(
            BindableAction::OpenInEditor,
            vec![
                KeyBinding::new(KeyCode::Char('e'), KeyModifiers::NONE),
                KeyBinding::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            ],
        );
        let triggers = editor_trigger_bytes(&kb);
        // Plain 'e' has no encoding, Ctrl-e → 0x05
        assert_eq!(triggers, vec![vec![0x05]]);
    }

    #[test]
    fn test_editor_trigger_bytes_ctrl_non_letter() {
        let mut kb = KeyBindings::default();
        kb.set_keys_for(
            BindableAction::OpenInEditor,
            vec![
                KeyBinding::new(KeyCode::Char('.'), KeyModifiers::NONE),
                KeyBinding::new(KeyCode::Char('.'), KeyModifiers::CONTROL),
            ],
        );
        let triggers = editor_trigger_bytes(&kb);
        // Plain '.' skipped. Ctrl-. yields both CSI-u and modifyOtherKeys encodings.
        assert_eq!(triggers.len(), 2);
        assert!(triggers.contains(&b"\x1b[46;5u".to_vec()));
        assert!(triggers.contains(&b"\x1b[27;5;46~".to_vec()));
    }

    #[test]
    fn test_editor_trigger_bytes_plain_letter_skipped() {
        let mut kb = KeyBindings::default();
        kb.set_keys_for(
            BindableAction::OpenInEditor,
            vec![KeyBinding::new(KeyCode::Char('e'), KeyModifiers::NONE)],
        );
        assert!(editor_trigger_bytes(&kb).is_empty());
    }

    #[test]
    fn test_review_trigger_bytes_default_is_alt_r() {
        // Default OpenReviewDiff is `r` + Alt-r; only the Alt binding is
        // interceptable in a raw attach (Alt-r → ESC r, the metaSendsEscape
        // sequence), and bare `r` is skipped so typing `r` in the pane isn't
        // swallowed. Alt-r replaced Ctrl-r so a shell's reverse-history-search
        // (Ctrl-r) is never shadowed.
        let kb = KeyBindings::default();
        assert_eq!(review_trigger_bytes(&kb), vec![vec![0x1b, b'r']]);
    }

    #[test]
    fn test_voice_trigger_bytes_default_is_alt_v() {
        // Default ToggleVoiceInput is Alt-v, interceptable mid-attach as the
        // metaSendsEscape sequence `ESC v` so it can toggle the mic in place.
        let kb = KeyBindings::default();
        assert_eq!(voice_trigger_bytes(&kb), vec![vec![0x1b, b'v']]);
    }

    #[test]
    fn test_dictation_trigger_bytes_default_is_alt_t() {
        // Default ToggleDictation is Alt-t, interceptable mid-attach as the
        // metaSendsEscape sequence `ESC t` so it can arm the mic in place — on a
        // shell pane as well as an agent one, unlike Alt-v.
        let kb = KeyBindings::default();
        assert_eq!(dictation_trigger_bytes(&kb), vec![vec![0x1b, b't']]);
    }

    #[test]
    fn test_review_toggle_binding_default_is_alt_r() {
        // The bare `r` binding is skipped (not interceptable mid-attach); the
        // Alt-r binding is the toggle, and its display drives the footer hint.
        let kb = KeyBindings::default();
        let toggle = review_toggle_binding(&kb).expect("default has an Alt binding");
        assert_eq!(toggle.to_string(), "Alt-r");
    }

    #[test]
    fn test_review_toggle_binding_none_when_only_bare_keys() {
        let mut kb = KeyBindings::default();
        kb.set_keys_for(
            BindableAction::OpenReviewDiff,
            vec![KeyBinding::new(KeyCode::Char('r'), KeyModifiers::NONE)],
        );
        assert!(review_toggle_binding(&kb).is_none());
    }

    #[test]
    fn test_matches_review_toggle_honours_rebinding() {
        let kb = KeyBindings::default();
        let alt_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT);
        let bare_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(matches_review_toggle(&kb, &alt_r));
        assert!(
            !matches_review_toggle(&kb, &bare_r),
            "bare r is review-local, not the toggle"
        );

        let mut kb = KeyBindings::default();
        kb.set_keys_for(
            BindableAction::OpenReviewDiff,
            vec![KeyBinding::new(KeyCode::Char('e'), KeyModifiers::ALT)],
        );
        let alt_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT);
        assert!(matches_review_toggle(&kb, &alt_e));
        assert!(
            !matches_review_toggle(&kb, &alt_r),
            "old key no longer toggles"
        );
    }

    #[test]
    fn test_trigger_bytes_alt_letter() {
        // Alt-<char> encodes to the two-byte ESC-prefix burst the terminal
        // emits for Meta/Alt, detected mid-attach via subsequence match.
        let mut kb = KeyBindings::default();
        kb.set_keys_for(
            BindableAction::OpenInEditor,
            vec![
                KeyBinding::new(KeyCode::Char('e'), KeyModifiers::NONE),
                KeyBinding::new(KeyCode::Char('e'), KeyModifiers::ALT),
            ],
        );
        // Plain 'e' skipped; Alt-e → ESC e (0x1b 0x65).
        assert_eq!(editor_trigger_bytes(&kb), vec![vec![0x1b, 0x65]]);
    }

    #[test]
    fn shifted_uppercase_resolves_with_or_without_shift_metadata() {
        let bindings: KeyBindings = toml::from_str("edit_session = ['E']").unwrap();
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::NONE] {
            assert_eq!(
                bindings.resolve(&KeyEvent::new(KeyCode::Char('E'), modifiers)),
                Some(BindableAction::EditSession)
            );
        }
        assert_eq!(
            bindings.resolve(&KeyEvent::new(KeyCode::Char('E'), KeyModifiers::CONTROL)),
            None
        );
        let mut explicit = bindings;
        explicit.set_keys_for(
            BindableAction::OpenInfo,
            vec![KeyBinding::new(KeyCode::Char('E'), KeyModifiers::NONE)],
        );
        assert_eq!(
            explicit.resolve(&KeyEvent::new(KeyCode::Char('E'), KeyModifiers::NONE)),
            Some(BindableAction::OpenInfo)
        );
    }

    #[test]
    fn test_rename_session_unbound_by_default() {
        // Rename dropped its default key (palette-only) so `r` could go to
        // OpenReviewDiff, pairing with the attached-session Alt-r toggle.
        let kb = KeyBindings::default();
        assert!(kb.keys_for(BindableAction::RenameSession).is_empty());
        let r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&r), Some(BindableAction::OpenReviewDiff));
    }

    #[test]
    fn test_reset_session_unbound_by_default() {
        // Reset is palette-only: no default hotkey. Shift-R stays the *resuming*
        // restart, so a mistyped restart can never discard a conversation.
        let kb = KeyBindings::default();
        assert!(kb.keys_for(BindableAction::ResetSession).is_empty());
        let shift_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert_eq!(kb.resolve(&shift_r), Some(BindableAction::RestartSession));
    }

    #[test]
    fn test_reset_session_config_name_round_trips() {
        // The `[keybindings]` key operators write in config.toml.
        assert_eq!(BindableAction::ResetSession.config_name(), "reset_session");
        assert_eq!(
            "reset_session".parse::<BindableAction>(),
            Ok(BindableAction::ResetSession)
        );
        // Listed in ALL, so the palette and the help modal both surface it
        // despite having no key.
        assert!(BindableAction::ALL.contains(&BindableAction::ResetSession));
    }

    #[test]
    fn test_toggle_keep_alive_unbound_by_default() {
        // Keep-alive is palette-only: no default hotkey, and Shift-K resolves
        // to nothing so the key stays free.
        let kb = KeyBindings::default();
        assert!(kb.keys_for(BindableAction::ToggleKeepAlive).is_empty());
        let shift_k = KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT);
        assert_eq!(kb.resolve(&shift_k), None);
    }

    /// The binding id documented in `docs/configuration.md` must actually parse
    /// — a doc example that silently no-ops is worse than none.
    #[test]
    fn test_set_session_base_config_name_roundtrips() {
        let action = BindableAction::SetSessionBase;
        assert_eq!(action.config_name(), "set_session_base");
        assert_eq!(
            "set_session_base".parse::<BindableAction>().unwrap(),
            action
        );
    }

    #[test]
    fn test_set_session_base_unbound_by_default() {
        // Restacking is a deliberate, infrequent repair — palette-only, so it
        // costs no key and can't be triggered by a slip.
        let kb = KeyBindings::default();
        assert!(kb.keys_for(BindableAction::SetSessionBase).is_empty());
    }

    #[test]
    fn test_workspace_keys_default_to_w_and_shift_w() {
        let kb = KeyBindings::default();
        let w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&w), Some(BindableAction::NextWorkspace));
        let shift_w = KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT);
        assert_eq!(kb.resolve(&shift_w), Some(BindableAction::WorkspacePicker));
    }

    #[test]
    fn test_palette_only_workspace_actions_are_unbound_but_listed() {
        let kb = KeyBindings::default();
        for action in [
            BindableAction::PreviousWorkspace,
            BindableAction::NewWorkspace,
            BindableAction::MoveProjectToWorkspace,
        ] {
            assert!(kb.keys_for(action).is_empty(), "{action:?} must be unbound");
            assert!(BindableAction::ALL.contains(&action));
            assert_eq!(action.section(), "Workspaces");
        }
        assert_eq!(BindableAction::NextWorkspace.section(), "Workspaces");
        assert_eq!(BindableAction::WorkspacePicker.section(), "Workspaces");
    }

    #[test]
    fn test_workspace_config_names_roundtrip() {
        for (action, name) in [
            (BindableAction::NextWorkspace, "next_workspace"),
            (BindableAction::PreviousWorkspace, "previous_workspace"),
            (BindableAction::WorkspacePicker, "workspace_picker"),
            (BindableAction::NewWorkspace, "new_workspace"),
            (
                BindableAction::MoveProjectToWorkspace,
                "move_project_to_workspace",
            ),
        ] {
            assert_eq!(action.config_name(), name);
            assert_eq!(name.parse::<BindableAction>().unwrap(), action);
        }
    }
}
