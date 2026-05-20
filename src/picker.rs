//! Standalone session picker for use inside a tmux popup.
//!
//! Invoked as `claude-commander pick-session --out <path>` from within
//! an attached session (Ctrl+O while attached). Reads the persisted
//! `AppState`, lets the user fuzzy-filter sessions, and writes the
//! chosen session's `tmux_session_name` to `<path>`. The outer TUI
//! reads that file when the popup closes and re-attaches to the chosen
//! session.

use std::io::{self, Stdout};
use std::path::Path;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use chrono::{DateTime, Utc};

use crate::config::AppState;
use crate::error::{Result, TuiError};
use crate::session::{SessionStatus, display_branch};

struct Match {
    tmux_name: String,
    title: String,
    branch: String,
    project_name: String,
    status: SessionStatus,
    score: i64,
    last_attached_at: Option<DateTime<Utc>>,
}

fn gather_matches(state: &AppState, query: &str, current: Option<&str>) -> Vec<Match> {
    let mut scored: Vec<Match> = Vec::new();
    for session in state.sessions.values() {
        if session.status == SessionStatus::Creating {
            continue;
        }
        // Alt+Tab semantics: never list the session the user is already
        // attached to — switching to it is a no-op.
        if let Some(c) = current
            && session.matches_tmux_name(c)
        {
            continue;
        }
        let Some(score) = session.fuzzy_score(query) else {
            continue;
        };
        let project_name = state
            .get_project(&session.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        scored.push(Match {
            tmux_name: session.tmux_session_name.clone(),
            title: session.title.clone(),
            branch: session.branch.clone(),
            project_name,
            status: session.status,
            score,
            last_attached_at: session.last_attached_at,
        });
    }
    if query.is_empty() {
        // Most-recently attached first; sessions never attached fall to
        // the bottom, ordered by title for stable navigation.
        scored.sort_by(|a, b| {
            b.last_attached_at
                .cmp(&a.last_attached_at)
                .then_with(|| a.title.cmp(&b.title))
        });
    } else {
        scored.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    }
    scored
}

/// Run the picker. Writes the chosen tmux session name to `out_path`
/// on Enter; leaves it untouched on Esc / Ctrl+C / no matches.
///
/// `current` names the session the user is attached to right now; that
/// session is excluded from the list so the top row is always the
/// previously-viewed session (Alt+Tab semantics).
pub fn run_session_picker(out_path: &Path, current: Option<&str>) -> Result<()> {
    let state = AppState::load().unwrap_or_else(|_| AppState::new());

    enable_raw_mode().map_err(|e| TuiError::InitFailed(e.to_string()))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| TuiError::InitFailed(e.to_string()))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| TuiError::InitFailed(e.to_string()))?;

    let result = run_loop(&mut terminal, &state, current);

    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    if let Some(tmux_name) = result? {
        std::fs::write(out_path, tmux_name)
            .map_err(|e| TuiError::InitFailed(format!("write out file: {}", e)))?;
    }
    Ok(())
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &AppState,
    current: Option<&str>,
) -> Result<Option<String>> {
    let mut query = String::new();
    let mut selected_idx: usize = 0;
    let mut scroll: usize = 0;

    loop {
        let matches = gather_matches(state, &query, current);
        if selected_idx >= matches.len() {
            selected_idx = matches.len().saturating_sub(1);
        }

        terminal
            .draw(|f| draw(f, &query, &matches, selected_idx, &mut scroll))
            .map_err(|e| TuiError::InitFailed(e.to_string()))?;

        let Event::Key(key) = event::read().map_err(|e| TuiError::InitFailed(e.to_string()))?
        else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => return Ok(None),
            (KeyCode::Char('c' | 'g'), KeyModifiers::CONTROL) => return Ok(None),
            (KeyCode::Enter, _) => {
                if let Some(m) = matches.get(selected_idx) {
                    return Ok(Some(m.tmux_name.clone()));
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                selected_idx = selected_idx.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL)
                if selected_idx + 1 < matches.len() =>
            {
                selected_idx += 1;
            }
            (KeyCode::Backspace, _) => {
                query.pop();
                selected_idx = 0;
                scroll = 0;
            }
            (KeyCode::Char(c), m)
                if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
            {
                query.push(c);
                selected_idx = 0;
                scroll = 0;
            }
            _ => {}
        }
    }
}

fn draw(
    f: &mut Frame<'_>,
    query: &str,
    matches: &[Match],
    selected_idx: usize,
    scroll: &mut usize,
) {
    let area = f.area();
    if area.height == 0 {
        return;
    }

    let input_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from(format!("❯ {}_", query))),
        input_area,
    );

    if area.height <= 1 {
        return;
    }
    let list_area = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height - 1,
    };
    let visible = list_area.height as usize;
    if visible == 0 {
        return;
    }

    if selected_idx < *scroll {
        *scroll = selected_idx;
    }
    if selected_idx >= *scroll + visible {
        *scroll = selected_idx + 1 - visible;
    }

    for (i, m) in matches.iter().skip(*scroll).take(visible).enumerate() {
        let abs_idx = *scroll + i;
        let is_selected = abs_idx == selected_idx;
        let row = Rect {
            x: list_area.x,
            y: list_area.y + i as u16,
            width: list_area.width,
            height: 1,
        };

        let (icon, color) = match m.status {
            SessionStatus::Creating | SessionStatus::Merging | SessionStatus::Pushing => {
                ("⠋", Color::Yellow)
            }
            SessionStatus::Running => ("●", Color::Green),
            SessionStatus::Stopped => ("○", Color::DarkGray),
            SessionStatus::CascadePaused => ("⏸", Color::Yellow),
        };

        let title_style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        } else {
            Style::default()
        };

        let mut spans = vec![
            Span::styled(format!(" {} ", icon), Style::default().fg(color)),
            Span::styled(m.title.clone(), title_style),
        ];
        if let Some(shown_branch) = display_branch(&m.title, &m.branch) {
            spans.push(Span::styled(
                format!(" [{}]", shown_branch),
                Style::default().fg(Color::Cyan),
            ));
        }
        spans.push(Span::styled(
            format!(" ({})", m.project_name),
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(Paragraph::new(Line::from(spans)), row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Project, ProjectId, SessionId, WorktreeSession};
    use chrono::Duration;
    use std::path::PathBuf;

    fn add_session(state: &mut AppState, project_id: ProjectId, title: &str) -> SessionId {
        let s = WorktreeSession::new(project_id, title, title, PathBuf::from("/tmp/wt"), "claude");
        let id = s.id;
        state.add_session(s);
        id
    }

    fn fixture() -> (AppState, ProjectId) {
        let mut state = AppState::new();
        let project = Project::new("p", PathBuf::from("/tmp/p"), "main");
        let project_id = project.id;
        state.add_project(project);
        (state, project_id)
    }

    #[test]
    fn empty_query_orders_by_last_attached_desc() {
        let (mut state, project_id) = fixture();
        let a = add_session(&mut state, project_id, "alpha");
        let b = add_session(&mut state, project_id, "bravo");
        let c = add_session(&mut state, project_id, "charlie");

        let now = chrono::Utc::now();
        state.sessions.get_mut(&a).unwrap().last_attached_at = Some(now - Duration::minutes(5));
        state.sessions.get_mut(&b).unwrap().last_attached_at = Some(now - Duration::minutes(1));
        state.sessions.get_mut(&c).unwrap().last_attached_at = Some(now - Duration::minutes(10));

        let matches = gather_matches(&state, "", None);
        let titles: Vec<&str> = matches.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["bravo", "alpha", "charlie"]);
    }

    #[test]
    fn never_attached_sessions_sort_to_bottom_by_title() {
        let (mut state, project_id) = fixture();
        let a = add_session(&mut state, project_id, "zulu");
        let _b = add_session(&mut state, project_id, "alpha");
        let _c = add_session(&mut state, project_id, "bravo");

        // Only zulu has been attached; alpha + bravo are unattached and
        // should fall below zulu, ordered by title.
        state.sessions.get_mut(&a).unwrap().last_attached_at = Some(chrono::Utc::now());

        let matches = gather_matches(&state, "", None);
        let titles: Vec<&str> = matches.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["zulu", "alpha", "bravo"]);
    }

    #[test]
    fn current_session_is_excluded() {
        let (mut state, project_id) = fixture();
        let _a = add_session(&mut state, project_id, "alpha");
        let b = add_session(&mut state, project_id, "bravo");
        let current_tmux = state.sessions.get(&b).unwrap().tmux_session_name.clone();

        let matches = gather_matches(&state, "", Some(&current_tmux));
        let titles: Vec<&str> = matches.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["alpha"]);
    }

    #[test]
    fn current_session_excluded_via_shell_pair_name() {
        let (mut state, project_id) = fixture();
        let _a = add_session(&mut state, project_id, "alpha");
        let b = add_session(&mut state, project_id, "bravo");
        // bravo has a paired shell session — Ctrl+\ in attach mode toggles
        // the user between them. Either name should hide bravo from the list.
        state.sessions.get_mut(&b).unwrap().shell_tmux_session_name = Some("cc-bravo-sh".into());

        let matches = gather_matches(&state, "", Some("cc-bravo-sh"));
        let titles: Vec<&str> = matches.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["alpha"]);
    }

    #[test]
    fn fuzzy_query_falls_back_to_score_order_and_still_excludes_current() {
        let (mut state, project_id) = fixture();
        let _a = add_session(&mut state, project_id, "alpha");
        let b = add_session(&mut state, project_id, "bravo");
        let _c = add_session(&mut state, project_id, "alphabet");
        let current_tmux = state.sessions.get(&b).unwrap().tmux_session_name.clone();

        let matches = gather_matches(&state, "alp", Some(&current_tmux));
        let titles: Vec<&str> = matches.iter().map(|m| m.title.as_str()).collect();
        // Both alpha-prefix titles match; exact ranking comes from fuzzy
        // score, but the property we care about is: bravo is gone and
        // both alpha-family titles are present.
        assert!(titles.contains(&"alpha"));
        assert!(titles.contains(&"alphabet"));
        assert!(!titles.contains(&"bravo"));
    }
}
