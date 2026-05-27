//! Hierarchical tree list widget
//!
//! Displays projects and their worktree sessions in an indented list.

use ratatui::{
    style::{Color, Modifier, Style},
    widgets::{Block, ListItem},
};

use std::collections::HashMap;

use crate::session::{AgentState, ProjectId, SessionListItem, SessionStatus};
use crate::tui::theme::Theme;

pub(crate) mod pr_colors;
mod render;
mod state;

#[cfg(test)]
mod tests;

pub use state::TreeListState;

/// Width of the right-aligned session-number field. The rendered prefix is
/// `"{n:>NUMBER_WIDTH$} "` — so the number occupies NUMBER_WIDTH columns and
/// is followed by a single trailing space, giving a 7-column slot.
const NUMBER_WIDTH: usize = 6;
/// Extra indent prepended for stacked-child worktrees (3 display columns),
/// so they sit one level deeper than the stack base they sit under.
const STACK_INDENT: &str = "   ";

/// Braille spinner frames for the Creating status indicator
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Tree list widget for displaying hierarchical sessions
pub struct TreeList<'a> {
    /// Items to display
    items: &'a [SessionListItem],
    /// Theme for styling
    theme: &'a Theme,
    /// Block for borders and title
    block: Option<Block<'a>>,
    /// Style for selected item
    highlight_style: Style,
    /// Tick counter for spinner animation
    tick: u64,
    /// Label names that mark an open PR as awaiting reviewer action.
    review_labels: &'a [String],
    /// When true, render PR labels as colored text on default bg (pre-pill
    /// behavior). When false (default), render as a colored pill block.
    invert_pr_label_color: bool,
    /// When true (default), show the running program as a `(program)`
    /// suffix on session rows when sessions use more than one program.
    show_session_program: bool,
    /// Projects whose most recent auto-pull was held back, with a short
    /// reason string. A small ⚠ badge is rendered on each blocked row.
    pull_blocked_projects: HashMap<ProjectId, &'a str>,
}

impl<'a> TreeList<'a> {
    /// Create a new tree list
    pub fn new(items: &'a [SessionListItem], theme: &'a Theme) -> Self {
        Self {
            items,
            theme,
            block: None,
            highlight_style: theme.selection().add_modifier(Modifier::BOLD),
            tick: 0,
            review_labels: &[],
            invert_pr_label_color: false,
            show_session_program: true,
            pull_blocked_projects: HashMap::new(),
        }
    }

    /// Mark a set of projects as having a held-back background pull.
    /// Renders a ⚠ badge on each matching project row.
    pub fn pull_blocked_projects(mut self, blocked: HashMap<ProjectId, &'a str>) -> Self {
        self.pull_blocked_projects = blocked;
        self
    }

    /// Whether a project row should display the FF-blocked badge.
    pub(crate) fn project_is_pull_blocked(&self, id: &ProjectId) -> bool {
        self.pull_blocked_projects.contains_key(id)
    }

    /// When false, never show the `(program)` suffix. When true (default),
    /// show it only if the list has more than one distinct program.
    pub fn show_session_program(mut self, b: bool) -> Self {
        self.show_session_program = b;
        self
    }

    /// When true, render PR labels as colored text on default bg
    /// (pre-pill behavior). Default false renders them as a colored pill.
    pub fn invert_pr_label_color(mut self, b: bool) -> Self {
        self.invert_pr_label_color = b;
        self
    }

    /// Configure the labels that flag an open PR as awaiting reviewer action.
    pub fn review_labels(mut self, labels: &'a [String]) -> Self {
        self.review_labels = labels;
        self
    }

    /// Set the tick counter for spinner animation
    pub fn tick(mut self, tick: u64) -> Self {
        self.tick = tick;
        self
    }

    /// Set the highlight style
    pub fn highlight_style(mut self, style: Style) -> Self {
        self.highlight_style = style;
        self
    }

    /// Pick the single status glyph and colour for a worktree row.
    ///
    /// Priority (first wins):
    /// 1. Creating / Merging / Pushing → animated spinner
    /// 2. CascadePaused        → `⏸` with warning accent
    /// 3. Agent `Working`      → animated spinner
    /// 4. Agent `WaitingForInput` → `?` glyph
    /// 5. `unread`             → `◆` diamond
    /// 6. Running (idle/unknown, no unread) → `●` filled circle
    /// 7. Stopped              → `○` open circle
    fn session_status_glyph(
        &self,
        status: SessionStatus,
        agent_state: Option<AgentState>,
        unread: bool,
    ) -> Option<(String, Color)> {
        if matches!(
            status,
            SessionStatus::Creating | SessionStatus::Merging | SessionStatus::Pushing
        ) {
            let step = self.tick as usize / 3;
            let frame = SPINNER_FRAMES[step % SPINNER_FRAMES.len()];
            return Some((frame.to_string(), self.theme.status_creating));
        }
        if status == SessionStatus::CascadePaused {
            return Some(("⏸".to_string(), self.theme.agent_waiting));
        }
        if status == SessionStatus::Running {
            match agent_state {
                Some(AgentState::Working) => {
                    let step = self.tick as usize / 3;
                    let frame = SPINNER_FRAMES[step % SPINNER_FRAMES.len()];
                    let color = self.theme.agent_working.color_for_tick(step as u64);
                    return Some((frame.to_string(), color));
                }
                Some(AgentState::WaitingForInput) => {
                    return Some(("?".to_string(), self.theme.agent_waiting));
                }
                _ => {}
            }
            if unread {
                return Some(("◆".to_string(), self.theme.unread_indicator));
            }
            return Some(("●".to_string(), self.theme.status_running));
        }
        // Stopped
        Some(("○".to_string(), self.theme.status_stopped))
    }

    /// Check whether sessions use more than one distinct program
    fn has_mixed_programs(&self) -> bool {
        let mut first = None;
        for item in self.items {
            if let SessionListItem::Worktree { program, .. } = item {
                match first {
                    None => first = Some(program_name(program)),
                    Some(p) if p != program_name(program) => return true,
                    _ => {}
                }
            }
        }
        false
    }
}

/// The base program name, excluding any arguments (e.g. "claude --mode auto" -> "claude").
pub(super) fn program_name(program: &str) -> &str {
    program.split_whitespace().next().unwrap_or(program)
}
