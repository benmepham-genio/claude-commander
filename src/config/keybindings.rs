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
    Select,
    SelectShell,
    NewSession,
    NewStackedSession,
    CascadeMergeMain,
    CascadeResume,
    CascadeAbandon,
    PushStack,
    NewProject,
    CheckoutBranch,
    DeleteSession,
    DeleteMergedPrSessions,
    RenameSession,
    RestartSession,
    RemoveProject,
    OpenInEditor,
    OpenPullRequest,
    TogglePane,
    TogglePaneReverse,
    ShrinkLeftPane,
    GrowLeftPane,
    ShowHelp,
    ShowSettings,
    Quit,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    GenerateSummary,
    ScanDirectory,
    MoveToSection,
    ToggleSection,
    ToggleViewMode,
}

impl BindableAction {
    /// All actions in display order (used for help screen sections).
    pub const ALL: &'static [BindableAction] = &[
        Self::NavigateUp,
        Self::NavigateDown,
        Self::Select,
        Self::SelectShell,
        Self::NewSession,
        Self::NewStackedSession,
        Self::CascadeMergeMain,
        Self::CascadeResume,
        Self::CascadeAbandon,
        Self::PushStack,
        Self::NewProject,
        Self::CheckoutBranch,
        Self::DeleteSession,
        Self::DeleteMergedPrSessions,
        Self::RenameSession,
        Self::RestartSession,
        Self::RemoveProject,
        Self::OpenInEditor,
        Self::OpenPullRequest,
        Self::ScanDirectory,
        Self::MoveToSection,
        Self::ToggleSection,
        Self::ToggleViewMode,
        Self::TogglePane,
        Self::TogglePaneReverse,
        Self::ShrinkLeftPane,
        Self::GrowLeftPane,
        Self::ScrollUp,
        Self::ScrollDown,
        Self::PageUp,
        Self::PageDown,
        Self::GenerateSummary,
        Self::ShowHelp,
        Self::ShowSettings,
        Self::Quit,
    ];

    /// Snake_case name used as the TOML key.
    pub fn config_name(self) -> &'static str {
        match self {
            Self::NavigateUp => "navigate_up",
            Self::NavigateDown => "navigate_down",
            Self::Select => "select",
            Self::SelectShell => "select_shell",
            Self::NewSession => "new_session",
            Self::NewStackedSession => "new_stacked_session",
            Self::CascadeMergeMain => "cascade_merge_main",
            Self::CascadeResume => "cascade_resume",
            Self::CascadeAbandon => "cascade_abandon",
            Self::PushStack => "push_stack",
            Self::NewProject => "new_project",
            Self::CheckoutBranch => "checkout_branch",
            Self::DeleteSession => "delete_session",
            Self::DeleteMergedPrSessions => "delete_merged_pr_sessions",
            Self::RenameSession => "rename_session",
            Self::RestartSession => "restart_session",
            Self::RemoveProject => "remove_project",
            Self::OpenInEditor => "open_in_editor",
            Self::OpenPullRequest => "open_pull_request",
            Self::TogglePane => "toggle_pane",
            Self::TogglePaneReverse => "toggle_pane_reverse",
            Self::ShrinkLeftPane => "shrink_left_pane",
            Self::GrowLeftPane => "grow_left_pane",
            Self::ShowHelp => "show_help",
            Self::ShowSettings => "show_settings",
            Self::Quit => "quit",
            Self::ScrollUp => "scroll_up",
            Self::ScrollDown => "scroll_down",
            Self::PageUp => "page_up",
            Self::PageDown => "page_down",
            Self::GenerateSummary => "generate_summary",
            Self::ScanDirectory => "scan_directory",
            Self::MoveToSection => "move_to_section",
            Self::ToggleSection => "toggle_section",
            Self::ToggleViewMode => "toggle_view_mode",
        }
    }

    /// Human-readable label for the help screen.
    pub fn description(self) -> &'static str {
        match self {
            Self::NavigateUp => "Navigate up",
            Self::NavigateDown => "Navigate down",
            Self::Select => "Attach to selected session",
            Self::SelectShell => "Open shell in worktree",
            Self::NewSession => "New worktree session",
            Self::NewStackedSession => "New stacked session on selected branch",
            Self::CascadeMergeMain => "Cascade merge main through stack",
            Self::CascadeResume => "Resume paused cascade merge",
            Self::CascadeAbandon => "Abandon paused cascade merge",
            Self::PushStack => "Push stack to remote (base → leaf)",
            Self::NewProject => "New project (add git repo)",
            Self::CheckoutBranch => "Checkout existing branch",
            Self::DeleteSession => "Delete/kill session",
            Self::DeleteMergedPrSessions => "Delete sessions with merged PRs",
            Self::RenameSession => "Rename session",
            Self::RestartSession => "Restart session",
            Self::RemoveProject => "Remove project",
            Self::OpenInEditor => "Open in editor/IDE",
            Self::OpenPullRequest => "Open PR in browser",
            Self::TogglePane => "Toggle preview/diff/shell view",
            Self::TogglePaneReverse => "Toggle view (reverse)",
            Self::ShrinkLeftPane => "Shrink left pane",
            Self::GrowLeftPane => "Grow left pane",
            Self::ShowHelp => "Show help",
            Self::ShowSettings => "Settings",
            Self::Quit => "Quit",
            Self::ScrollUp => "Scroll up",
            Self::ScrollDown => "Scroll down",
            Self::PageUp => "Page up",
            Self::PageDown => "Page down",
            Self::GenerateSummary => "Generate AI summary",
            Self::ScanDirectory => "Scan directory for repos",
            Self::MoveToSection => "Move session to section…",
            Self::ToggleSection => "Collapse/expand section",
            Self::ToggleViewMode => "Cycle project / sections / section stacks view",
        }
    }

    /// Help screen section for grouping.
    pub fn section(self) -> &'static str {
        match self {
            Self::NavigateUp | Self::NavigateDown | Self::Select => "Navigation",
            Self::SelectShell
            | Self::NewSession
            | Self::NewStackedSession
            | Self::CascadeMergeMain
            | Self::CascadeResume
            | Self::CascadeAbandon
            | Self::PushStack
            | Self::NewProject
            | Self::CheckoutBranch
            | Self::DeleteSession
            | Self::DeleteMergedPrSessions
            | Self::RenameSession
            | Self::RestartSession
            | Self::RemoveProject
            | Self::OpenInEditor
            | Self::OpenPullRequest
            | Self::ScanDirectory
            | Self::MoveToSection
            | Self::ToggleSection
            | Self::ToggleViewMode => "Session Management",
            Self::TogglePane
            | Self::TogglePaneReverse
            | Self::ShrinkLeftPane
            | Self::GrowLeftPane => "Layout",
            Self::ScrollUp | Self::ScrollDown | Self::PageUp | Self::PageDown => "Scrolling",
            Self::GenerateSummary => "Info Pane",
            Self::ShowHelp | Self::ShowSettings | Self::Quit => "Other",
        }
    }
}

impl FromStr for BindableAction {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "navigate_up" => Ok(Self::NavigateUp),
            "navigate_down" => Ok(Self::NavigateDown),
            "select" => Ok(Self::Select),
            "select_shell" => Ok(Self::SelectShell),
            "new_session" => Ok(Self::NewSession),
            "new_stacked_session" => Ok(Self::NewStackedSession),
            "cascade_merge_main" => Ok(Self::CascadeMergeMain),
            "cascade_resume" => Ok(Self::CascadeResume),
            "cascade_abandon" => Ok(Self::CascadeAbandon),
            "push_stack" => Ok(Self::PushStack),
            "new_project" => Ok(Self::NewProject),
            "checkout_branch" => Ok(Self::CheckoutBranch),
            "delete_session" => Ok(Self::DeleteSession),
            "delete_merged_pr_sessions" => Ok(Self::DeleteMergedPrSessions),
            "rename_session" => Ok(Self::RenameSession),
            "restart_session" => Ok(Self::RestartSession),
            "remove_project" => Ok(Self::RemoveProject),
            "open_in_editor" => Ok(Self::OpenInEditor),
            "open_pull_request" => Ok(Self::OpenPullRequest),
            "toggle_pane" => Ok(Self::TogglePane),
            "toggle_pane_reverse" => Ok(Self::TogglePaneReverse),
            "shrink_left_pane" => Ok(Self::ShrinkLeftPane),
            "grow_left_pane" => Ok(Self::GrowLeftPane),
            "show_help" => Ok(Self::ShowHelp),
            "show_settings" => Ok(Self::ShowSettings),
            "quit" => Ok(Self::Quit),
            "scroll_up" => Ok(Self::ScrollUp),
            "scroll_down" => Ok(Self::ScrollDown),
            "page_up" => Ok(Self::PageUp),
            "page_down" => Ok(Self::PageDown),
            "generate_summary" => Ok(Self::GenerateSummary),
            "scan_directory" => Ok(Self::ScanDirectory),
            "move_to_section" => Ok(Self::MoveToSection),
            "toggle_section" => Ok(Self::ToggleSection),
            "toggle_view_mode" => Ok(Self::ToggleViewMode),
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
            f if f.starts_with('f') && f.len() <= 3 => {
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
        self.lookup.get(&(event.code, event.modifiers)).copied()
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
            BindableAction::RenameSession,
            vec![kb(KeyCode::Char('r'), none)],
        );
        bindings.insert(
            BindableAction::RestartSession,
            vec![kb(KeyCode::Char('R'), shift)],
        );
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

        // Pane control
        bindings.insert(BindableAction::TogglePane, vec![kb(KeyCode::Tab, none)]);
        bindings.insert(
            BindableAction::TogglePaneReverse,
            vec![kb(KeyCode::BackTab, shift)],
        );
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
        bindings.insert(
            BindableAction::PageUp,
            vec![kb(KeyCode::Char('u'), ctrl), kb(KeyCode::PageUp, none)],
        );
        bindings.insert(
            BindableAction::PageDown,
            vec![kb(KeyCode::Char('d'), ctrl), kb(KeyCode::PageDown, none)],
        );

        bindings.insert(
            BindableAction::ScanDirectory,
            vec![kb(KeyCode::Char('S'), shift)],
        );
        bindings.insert(BindableAction::ToggleSection, vec![]);
        bindings.insert(
            BindableAction::ToggleViewMode,
            vec![kb(KeyCode::Char('v'), none)],
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

/// Compute the raw byte sequences that should trigger [`OpenInEditor`] while
/// forwarding stdin to a tmux PTY.
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
///
/// [`OpenInEditor`]: BindableAction::OpenInEditor
pub fn editor_trigger_bytes(bindings: &KeyBindings) -> Vec<Vec<u8>> {
    let mut triggers = Vec::new();
    for kb in bindings.keys_for(BindableAction::OpenInEditor) {
        if kb.modifiers != KeyModifiers::CONTROL {
            continue;
        }
        let KeyCode::Char(c) = kb.code else {
            continue;
        };
        if c.is_ascii_alphabetic() {
            // Ctrl-<letter> → control byte
            let byte = (c.to_ascii_lowercase() as u8) - 0x60;
            triggers.push(vec![byte]);
        } else if c.is_ascii() {
            // Ctrl-<non-letter> → CSI-u or modifyOtherKeys sequence
            let n = c as u32;
            triggers.push(format!("\x1b[{n};5u").into_bytes());
            triggers.push(format!("\x1b[27;5;{n}~").into_bytes());
        }
    }
    triggers
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_toggle_view_mode_default_bound_to_v() {
        let kb = KeyBindings::default();
        let key = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE);
        assert_eq!(kb.resolve(&key), Some(BindableAction::ToggleViewMode));
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
            navigate_up = "w"
        "#;

        let kb: KeyBindings = toml::from_str(toml).unwrap();

        // Overridden: quit is now only Esc
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(kb.resolve(&esc), Some(BindableAction::Quit));

        // Old quit binding should be gone
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_ne!(kb.resolve(&q), Some(BindableAction::Quit));

        // Overridden: navigate_up is now 'w' (single string, not array)
        let w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE);
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
            navigate_up = "w"
        "#;
        let kb: KeyBindings = toml::from_str(toml).unwrap();
        // Recognised override still applied
        let w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE);
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
    fn test_sections_grouping() {
        let kb = KeyBindings::default();
        let sections = kb.sections();
        let section_names: Vec<&str> = sections.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            section_names,
            vec![
                "Navigation",
                "Session Management",
                "Layout",
                "Scrolling",
                "Info Pane",
                "Other"
            ]
        );
    }
}
