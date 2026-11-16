//! Shared row-rendering helpers for the session list widgets.
//!
//! Extracted from the old tree list so the kanban
//! [`BoardWidget`](super::board::BoardWidget) can render the same per-session
//! row content (status glyph, markers, program suffix). These free helpers hold
//! the pieces so the logic lives in exactly one place.

use ratatui::style::Color;

use crate::theme::Theme;
use claude_commander_core::session::{AgentState, SessionStatus};

/// Braille spinner frames for the Creating / Working status indicators.
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Marker shown on a session row that has pending review comments. Matches the
/// review view's comment marker.
pub const COMMENT_MARKER: char = '*';

/// Marker shown on a session row the user has kept alive (opted out of
/// auto-hibernation) — an anchor: the session stays put and won't hibernate.
pub const KEEP_ALIVE_MARKER: char = '⚓';

/// Suffix shown on a session row whose worktree is pulling Git LFS objects.
pub const LFS_MARKER: &str = " ⇣ LFS";

/// Whether a session in `status` shows the animated spinner regardless of its
/// agent state (Creating / Merging / Pushing). The tick loop uses this to decide
/// whether a redraw is needed to advance the animation.
pub fn status_spins(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Creating | SessionStatus::Merging | SessionStatus::Pushing
    )
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
pub fn session_status_glyph(
    theme: &Theme,
    tick: u64,
    status: SessionStatus,
    agent_state: Option<AgentState>,
    unread: bool,
) -> Option<(String, Color)> {
    if status_spins(status) {
        let step = tick as usize / 3;
        let frame = SPINNER_FRAMES[step % SPINNER_FRAMES.len()];
        return Some((frame.to_string(), theme.status_creating));
    }
    if status == SessionStatus::CascadePaused {
        return Some(("⏸".to_string(), theme.agent_waiting));
    }
    if status == SessionStatus::Running {
        match agent_state {
            Some(AgentState::Working) => {
                let step = tick as usize / 3;
                let frame = SPINNER_FRAMES[step % SPINNER_FRAMES.len()];
                let color = theme.agent_working.color_for_tick(step as u64);
                return Some((frame.to_string(), color));
            }
            Some(AgentState::WaitingForInput) => {
                return Some(("?".to_string(), theme.agent_waiting));
            }
            _ => {}
        }
        if unread {
            return Some(("◆".to_string(), theme.unread_indicator));
        }
        return Some(("●".to_string(), theme.status_running));
    }
    // Stopped
    Some(("○".to_string(), theme.status_stopped))
}

/// A short word describing the session's state, mirroring
/// [`session_status_glyph`]'s precedence exactly so the pair always agree
/// (glyph + word render together on a board card's body line).
pub fn status_label(
    status: SessionStatus,
    agent_state: Option<AgentState>,
    unread: bool,
) -> &'static str {
    match status {
        SessionStatus::Creating => "creating…",
        SessionStatus::Merging => "merging…",
        SessionStatus::Pushing => "pushing…",
        SessionStatus::CascadePaused => "paused",
        SessionStatus::Running => match agent_state {
            Some(AgentState::Working) => "working…",
            Some(AgentState::WaitingForInput) => "waiting",
            _ if unread => "unread",
            _ => "idle",
        },
        SessionStatus::Stopped => "stopped",
    }
}

/// The base program name, excluding any arguments (e.g. "claude --mode auto" -> "claude").
pub fn program_name(program: &str) -> &str {
    program.split_whitespace().next().unwrap_or(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_mirrors_glyph_precedence() {
        // Same precedence as session_status_glyph: working beats unread,
        // waiting beats unread, unread beats idle.
        assert_eq!(
            status_label(SessionStatus::Running, Some(AgentState::Working), true),
            "working…"
        );
        assert_eq!(
            status_label(
                SessionStatus::Running,
                Some(AgentState::WaitingForInput),
                true
            ),
            "waiting"
        );
        assert_eq!(status_label(SessionStatus::Running, None, true), "unread");
        assert_eq!(
            status_label(SessionStatus::Running, Some(AgentState::Idle), false),
            "idle"
        );
        assert_eq!(
            status_label(SessionStatus::Creating, None, false),
            "creating…"
        );
        assert_eq!(
            status_label(SessionStatus::CascadePaused, None, false),
            "paused"
        );
        assert_eq!(status_label(SessionStatus::Stopped, None, false), "stopped");
    }
}
