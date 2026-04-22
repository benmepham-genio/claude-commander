use super::*;
use crate::git::PrState;
use crate::session::{ProjectId, SessionId};
use ratatui::{Terminal, backend::TestBackend, widgets::ListState};
use std::path::PathBuf;

fn make_project(name: &str, count: usize) -> SessionListItem {
    SessionListItem::Project {
        id: ProjectId::new(),
        name: name.to_string(),
        repo_path: PathBuf::from("/tmp/test"),
        main_branch: "main".to_string(),
        worktree_count: count,
        nested: false,
    }
}

fn make_worktree(title: &str) -> SessionListItem {
    SessionListItem::Worktree {
        id: SessionId::new(),
        project_id: ProjectId::new(),
        title: title.to_string(),
        branch: "feat".to_string(),
        status: SessionStatus::Running,
        program: "claude".to_string(),
        pr_number: None,
        pr_url: None,
        pr_merged: false,
        pr_state: None,
        pr_draft: false,
        pr_labels: Vec::new(),
        worktree_path: PathBuf::from("/tmp/test"),
        created_at: chrono::Utc::now(),
        agent_state: None,
        unread: false,
    }
}

/// Render a TreeList to a buffer and return lines as strings
fn render_tree(items: &[SessionListItem], width: u16, height: u16) -> Vec<String> {
    let theme = Theme::basic();
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let tree = TreeList::new(items, &theme);
            frame.render_stateful_widget(tree, frame.area(), &mut ListState::default());
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn test_worktree_rows_use_number_prefix() {
    let items = vec![
        make_project("proj", 2),
        make_worktree("session-a"),
        make_worktree("session-b"),
    ];
    let lines = render_tree(&items, 40, 4);
    // First worktree starts with right-aligned "1"
    assert!(
        lines[1].trim_start().starts_with("1 "),
        "Expected number prefix in: '{}'",
        lines[1]
    );
    // Second worktree starts with "2"
    assert!(
        lines[2].trim_start().starts_with("2 "),
        "Expected number prefix in: '{}'",
        lines[2]
    );
    // No ASCII tree glyph — numbering is the only supported prefix.
    assert!(
        !lines[1].contains("└──"),
        "Tree glyph should no longer appear"
    );
}

#[test]
fn test_numbers_are_sequential_across_projects() {
    let items = vec![
        make_project("proj-a", 1),
        make_worktree("session-1"),
        make_project("proj-b", 1),
        make_worktree("session-2"),
    ];
    let lines = render_tree(&items, 40, 5);
    // Session under proj-a is #1
    assert!(
        lines[1].trim_start().starts_with("1 "),
        "Expected 1 in: '{}'",
        lines[1]
    );
    // Session under proj-b is #2 (not restarting)
    assert!(
        lines[3].trim_start().starts_with("2 "),
        "Expected 2 in: '{}'",
        lines[3]
    );
}

#[test]
fn test_double_digit_number_formatting() {
    let mut items = vec![make_project("proj", 12)];
    for i in 1..=12 {
        items.push(make_worktree(&format!("s-{}", i)));
    }
    let lines = render_tree(&items, 40, 14);
    // Single digit right-aligned
    assert!(
        lines[1].trim_start().starts_with("1 "),
        "line 1: '{}'",
        lines[1]
    );
    // Double digit
    assert!(
        lines[10].trim_start().starts_with("10 "),
        "line 10: '{}'",
        lines[10]
    );
    assert!(
        lines[12].trim_start().starts_with("12 "),
        "line 12: '{}'",
        lines[12]
    );
}

#[test]
fn test_tree_list_state_navigation() {
    let mut state = TreeListState::new();
    state.set_item_count(3);

    assert_eq!(state.selected(), None);

    state.next();
    assert_eq!(state.selected(), Some(0));

    state.next();
    assert_eq!(state.selected(), Some(1));

    state.next();
    assert_eq!(state.selected(), Some(2));

    // Wrap around
    state.next();
    assert_eq!(state.selected(), Some(0));

    // Previous
    state.previous();
    assert_eq!(state.selected(), Some(2));
}

#[test]
fn test_navigation_skips_unselectable_rows() {
    let mut state = TreeListState::new();
    // 5 rows; indices 0 and 3 are section headers (unselectable), rest are selectable.
    state.set_selectable(vec![false, true, true, false, true]);

    state.next();
    assert_eq!(state.selected(), Some(1));

    state.next();
    assert_eq!(state.selected(), Some(2));

    state.next();
    // skips 3, lands on 4
    assert_eq!(state.selected(), Some(4));

    state.next();
    // wraps; skips 0, lands on 1
    assert_eq!(state.selected(), Some(1));

    state.previous();
    // wraps backwards skipping 0
    assert_eq!(state.selected(), Some(4));

    state.previous();
    // skips 3
    assert_eq!(state.selected(), Some(2));
}

#[test]
fn test_tree_list_state_empty() {
    let mut state = TreeListState::new();
    state.set_item_count(0);

    state.next();
    assert_eq!(state.selected(), None);

    state.previous();
    assert_eq!(state.selected(), None);
}

#[test]
fn test_previous_wraps_to_last() {
    let mut state = TreeListState::new();
    state.set_item_count(5);
    state.select(Some(0));

    state.previous();
    assert_eq!(state.selected(), Some(4));
}

#[test]
fn test_next_wraps_to_first() {
    let mut state = TreeListState::new();
    state.set_item_count(5);
    state.select(Some(4));

    state.next();
    assert_eq!(state.selected(), Some(0));
}

#[test]
fn test_set_item_count_clamps_selection() {
    let mut state = TreeListState::new();
    state.set_item_count(10);
    state.select(Some(7));

    state.set_item_count(5);
    assert_eq!(state.selected(), Some(4));
}

#[test]
fn test_set_item_count_zero_clears_selection() {
    let mut state = TreeListState::new();
    state.set_item_count(5);
    state.select(Some(3));

    state.set_item_count(0);
    assert_eq!(state.selected(), None);
}

#[test]
fn test_set_item_count_preserves_valid_selection() {
    let mut state = TreeListState::new();
    state.set_item_count(10);
    state.select(Some(3));

    state.set_item_count(8);
    assert_eq!(state.selected(), Some(3));
}

#[test]
fn test_single_item_navigation() {
    let mut state = TreeListState::new();
    state.set_item_count(1);
    state.select(Some(0));

    state.next();
    assert_eq!(state.selected(), Some(0));

    state.previous();
    assert_eq!(state.selected(), Some(0));
}

#[test]
fn test_next_from_none_selects_first() {
    let mut state = TreeListState::new();
    state.set_item_count(3);
    assert_eq!(state.selected(), None);

    state.next();
    assert_eq!(state.selected(), Some(0));
}

#[test]
fn test_previous_from_none_selects_first() {
    let mut state = TreeListState::new();
    state.set_item_count(3);
    assert_eq!(state.selected(), None);

    state.previous();
    assert_eq!(state.selected(), Some(0));
}

// -- pr_badge_color --

fn review_labels() -> Vec<String> {
    vec![
        "dev-review-required".into(),
        "ready-for-test".into(),
        "trivial".into(),
    ]
}

#[test]
fn test_pr_badge_color_open() {
    let theme = Theme::basic();
    let c = pr_colors::pr_badge_color(
        &theme,
        Some(PrState::Open),
        false,
        false,
        &[],
        &review_labels(),
    );
    assert_eq!(c, theme.pr_open);
}

#[test]
fn test_pr_badge_color_merged() {
    let theme = Theme::basic();
    let c = pr_colors::pr_badge_color(
        &theme,
        Some(PrState::Merged),
        true,
        false,
        &[],
        &review_labels(),
    );
    assert_eq!(c, theme.status_pr_merged);
}

#[test]
fn test_pr_badge_color_closed() {
    let theme = Theme::basic();
    let c = pr_colors::pr_badge_color(
        &theme,
        Some(PrState::Closed),
        false,
        false,
        &[],
        &review_labels(),
    );
    assert_eq!(c, theme.pr_closed);
}

#[test]
fn test_pr_badge_color_draft_takes_precedence_over_label() {
    let theme = Theme::basic();
    let labels = vec!["dev-review-required".into()];
    let c = pr_colors::pr_badge_color(
        &theme,
        Some(PrState::Open),
        false,
        true,
        &labels,
        &review_labels(),
    );
    assert_eq!(c, theme.pr_draft);
}

#[test]
fn test_pr_badge_color_review_label_match() {
    let theme = Theme::basic();
    let labels = vec!["unrelated".into(), "ready-for-test".into()];
    let c = pr_colors::pr_badge_color(
        &theme,
        Some(PrState::Open),
        false,
        false,
        &labels,
        &review_labels(),
    );
    assert_eq!(c, theme.status_pr);
}

#[test]
fn test_pr_badge_color_review_label_case_insensitive() {
    let theme = Theme::basic();
    let labels = vec!["Dev-Review-Required".into()];
    let c = pr_colors::pr_badge_color(
        &theme,
        Some(PrState::Open),
        false,
        false,
        &labels,
        &review_labels(),
    );
    assert_eq!(c, theme.status_pr);
}

#[test]
fn test_pr_badge_color_non_matching_labels_fall_through_to_open() {
    let theme = Theme::basic();
    let labels = vec!["bug".into(), "documentation".into()];
    let c = pr_colors::pr_badge_color(
        &theme,
        Some(PrState::Open),
        false,
        false,
        &labels,
        &review_labels(),
    );
    assert_eq!(c, theme.pr_open);
}

#[test]
fn test_pr_badge_color_unknown_state_uses_pr_merged_flag_for_merged() {
    // Backward compat: pre-pr_state state.json with pr_merged=true
    let theme = Theme::basic();
    let c = pr_colors::pr_badge_color(&theme, None, true, false, &[], &review_labels());
    assert_eq!(c, theme.status_pr_merged);
}

#[test]
fn test_pr_badge_color_unknown_state_falls_back_to_open() {
    // Backward compat: pre-pr_state state.json with pr_merged=false
    let theme = Theme::basic();
    let c = pr_colors::pr_badge_color(&theme, None, false, false, &[], &review_labels());
    assert_eq!(c, theme.pr_open);
}

// -- session_status_glyph (single unified icon column) --

fn empty_items() -> [SessionListItem; 0] {
    []
}

fn make_tree<'a>(theme: &'a Theme, items: &'a [SessionListItem]) -> TreeList<'a> {
    TreeList::new(items, theme)
}

#[test]
fn test_glyph_working_shows_spinner() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    let (g, c) = tree
        .session_status_glyph(SessionStatus::Running, Some(AgentState::Working), false)
        .unwrap();
    assert!(SPINNER_FRAMES.contains(&g.as_str()));
    // Default theme uses Rainbow → colour comes from the rainbow palette
    assert!(crate::config::theme::RAINBOW_PALETTE.contains(&c));
}

#[test]
fn test_glyph_working_beats_unread() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    let (g, _) = tree
        .session_status_glyph(SessionStatus::Running, Some(AgentState::Working), true)
        .unwrap();
    assert!(SPINNER_FRAMES.contains(&g.as_str()));
}

#[test]
fn test_glyph_waiting_for_input() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    let (g, c) = tree
        .session_status_glyph(
            SessionStatus::Running,
            Some(AgentState::WaitingForInput),
            false,
        )
        .unwrap();
    assert_eq!(g, "?");
    assert_eq!(c, theme.agent_waiting);
}

#[test]
fn test_glyph_waiting_beats_unread() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    let (g, _) = tree
        .session_status_glyph(
            SessionStatus::Running,
            Some(AgentState::WaitingForInput),
            true,
        )
        .unwrap();
    assert_eq!(g, "?");
}

#[test]
fn test_glyph_unread_when_idle() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    let (g, c) = tree
        .session_status_glyph(SessionStatus::Running, Some(AgentState::Idle), true)
        .unwrap();
    assert_eq!(g, "◆");
    assert_eq!(c, theme.unread_indicator);
}

#[test]
fn test_glyph_running_idle_no_unread() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    let (g, c) = tree
        .session_status_glyph(SessionStatus::Running, Some(AgentState::Idle), false)
        .unwrap();
    assert_eq!(g, "●");
    assert_eq!(c, theme.status_running);
}

#[test]
fn test_glyph_running_unknown_no_unread() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    let (g, c) = tree
        .session_status_glyph(SessionStatus::Running, Some(AgentState::Unknown), false)
        .unwrap();
    assert_eq!(g, "●");
    assert_eq!(c, theme.status_running);
}

#[test]
fn test_glyph_running_no_agent_state_no_unread() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    let (g, c) = tree
        .session_status_glyph(SessionStatus::Running, None, false)
        .unwrap();
    assert_eq!(g, "●");
    assert_eq!(c, theme.status_running);
}

#[test]
fn test_glyph_stopped() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    let (g, c) = tree
        .session_status_glyph(SessionStatus::Stopped, None, false)
        .unwrap();
    assert_eq!(g, "○");
    assert_eq!(c, theme.status_stopped);
}

#[test]
fn test_glyph_stopped_ignores_unread_and_agent_state() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    // Stopped sessions can't have a meaningful agent state, but ensure
    // the glyph is consistent regardless.
    let (g, _) = tree
        .session_status_glyph(SessionStatus::Stopped, Some(AgentState::Working), true)
        .unwrap();
    assert_eq!(g, "○");
}

#[test]
fn test_glyph_creating_shows_spinner() {
    let theme = Theme::basic();
    let items = empty_items();
    let tree = make_tree(&theme, &items);
    let (g, c) = tree
        .session_status_glyph(SessionStatus::Creating, None, false)
        .unwrap();
    assert!(SPINNER_FRAMES.contains(&g.as_str()));
    assert_eq!(c, theme.status_creating);
}
