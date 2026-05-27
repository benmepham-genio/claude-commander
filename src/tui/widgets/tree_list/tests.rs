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
    make_worktree_with_stack(title, false)
}

fn make_worktree_with_stack(title: &str, stacked_child: bool) -> SessionListItem {
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
        stacked_child,
    }
}

/// Render a TreeList to a buffer and return lines as strings
fn render_tree(items: &[SessionListItem], width: u16, height: u16) -> Vec<String> {
    render_tree_with(items, width, height, |t| t)
}

/// Like `render_tree`, but lets the caller configure the TreeList (e.g. to
/// toggle `show_session_program`).
fn render_tree_with<F>(
    items: &[SessionListItem],
    width: u16,
    height: u16,
    configure: F,
) -> Vec<String>
where
    F: for<'a> FnOnce(TreeList<'a>) -> TreeList<'a>,
{
    let theme = Theme::basic();
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let tree = configure(TreeList::new(items, &theme));
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

fn make_worktree_with_program(title: &str, program: &str) -> SessionListItem {
    let mut w = make_worktree(title);
    if let SessionListItem::Worktree { program: p, .. } = &mut w {
        *p = program.to_string();
    }
    w
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

// Cycle-arithmetic tests for the spinner index computation in
// `session_status_glyph`: `SPINNER_FRAMES[(tick as usize / 3) % SPINNER_FRAMES.len()]`.
// SPINNER_FRAMES has 10 entries, so the cycle is `(tick / 3) % 10`. Each
// test below picks tick values whose original-vs-mutant outputs differ:
//
//   tick=3  → original `(3/3)%10 = 1` ⇒ frame[1] = "⠙"
//             `/`→`%`: `(3%3)%10 = 0` ⇒ "⠋"  (distinguishes / from %)
//             `/`→`*`: `(3*3)%10 = 9` ⇒ "⠏"  (distinguishes / from *)
//             `%`→`/`: `(3/3)/10 = 0` ⇒ "⠋"  (distinguishes % from /)
//   tick=30 → original `(30/3)%10 = 0` ⇒ "⠋"
//             `%`→`/`: `(30/3)/10 = 1` ⇒ "⠙"  (extra guard for % from /)

#[test]
fn test_glyph_creating_spinner_uses_tick_div_three_mod_len() {
    let theme = Theme::basic();
    let items = empty_items();

    // tick=3: original index 1 → "⠙"
    let (g, _) = make_tree(&theme, &items)
        .tick(3)
        .session_status_glyph(SessionStatus::Creating, None, false)
        .unwrap();
    assert_eq!(
        g, "⠙",
        "tick=3 must select SPINNER_FRAMES[(3/3)%10] = SPINNER_FRAMES[1]"
    );

    // tick=30: original index 0 → "⠋" (catches `%` → `/` which would give 1)
    let (g, _) = make_tree(&theme, &items)
        .tick(30)
        .session_status_glyph(SessionStatus::Creating, None, false)
        .unwrap();
    assert_eq!(
        g, "⠋",
        "tick=30 must select SPINNER_FRAMES[(30/3)%10] = SPINNER_FRAMES[0]"
    );
}

#[test]
fn test_glyph_working_spinner_uses_tick_div_three_mod_len() {
    let theme = Theme::basic();
    let items = empty_items();

    // tick=3: original index 1 → "⠙"
    let (g, _) = make_tree(&theme, &items)
        .tick(3)
        .session_status_glyph(SessionStatus::Running, Some(AgentState::Working), false)
        .unwrap();
    assert_eq!(
        g, "⠙",
        "tick=3 must select SPINNER_FRAMES[(3/3)%10] = SPINNER_FRAMES[1]"
    );

    // tick=30: original index 0 → "⠋" (catches `%` → `/` which would give 1)
    let (g, _) = make_tree(&theme, &items)
        .tick(30)
        .session_status_glyph(SessionStatus::Running, Some(AgentState::Working), false)
        .unwrap();
    assert_eq!(
        g, "⠋",
        "tick=30 must select SPINNER_FRAMES[(30/3)%10] = SPINNER_FRAMES[0]"
    );
}

#[test]
fn test_stacked_child_row_has_extra_indent() {
    // The stacked child should sit one extra indent (STACK_INDENT) further
    // right than its base. The prefix is the right-aligned session number.
    let items = vec![
        make_project("proj", 2),
        make_worktree_with_stack("base", false),
        make_worktree_with_stack("child", true),
    ];
    let lines = render_tree(&items, 60, 4);

    let base_line = &lines[1];
    let child_line = &lines[2];

    // "1 " marks the base row, "2 " marks the child row. The child's number
    // should sit STACK_INDENT columns further right.
    let base_idx = base_line
        .find("1 ")
        .expect("base should have number prefix");
    let child_idx = child_line
        .find("2 ")
        .expect("child should have number prefix");
    assert_eq!(
        child_idx - base_idx,
        STACK_INDENT.chars().count(),
        "stacked child indent should be exactly STACK_INDENT wider than the base\nbase:  {base_line:?}\nchild: {child_line:?}"
    );
}

// -- section header collapsible rendering --

fn make_section_header(name: &str, count: usize, collapsed: bool) -> SessionListItem {
    SessionListItem::SectionHeader {
        name: name.to_string(),
        count,
        collapsed,
    }
}

#[test]
fn test_section_header_expanded_shows_down_twistie() {
    let items = vec![
        make_section_header("In Progress", 2, false),
        make_project("proj", 2),
        make_worktree("sess-a"),
    ];
    let lines = render_tree(&items, 60, 4);
    assert!(
        lines[0].contains("▾"),
        "Expected down-twistie for expanded section: {:?}",
        lines[0]
    );
    assert!(
        !lines[0].contains("▸"),
        "Should not have right-twistie when expanded: {:?}",
        lines[0]
    );
}

#[test]
fn test_section_header_collapsed_shows_right_twistie() {
    let items = vec![make_section_header("Done", 3, true)];
    let lines = render_tree(&items, 60, 2);
    assert!(
        lines[0].contains("▸"),
        "Expected right-twistie for collapsed section: {:?}",
        lines[0]
    );
    assert!(
        !lines[0].contains("▾"),
        "Should not have down-twistie when collapsed: {:?}",
        lines[0]
    );
}

#[test]
fn test_section_header_shows_count() {
    let items = vec![make_section_header("Review", 5, false)];
    let lines = render_tree(&items, 60, 2);
    assert!(
        lines[0].contains("(5)"),
        "Expected count in section header: {:?}",
        lines[0]
    );
}

#[test]
fn test_section_header_is_selectable() {
    let header = make_section_header("In Progress", 2, false);
    assert!(header.is_selectable());
}

#[test]
fn test_navigation_lands_on_section_headers() {
    let mut state = TreeListState::new();
    // Section header, spacer, section header — headers selectable, spacers not.
    state.set_selectable(vec![true, false, true]);

    state.next();
    assert_eq!(state.selected(), Some(0));

    state.next();
    assert_eq!(state.selected(), Some(2));

    state.previous();
    assert_eq!(state.selected(), Some(0));
}

// -- show_session_program toggle --

#[test]
fn test_program_suffix_shown_by_default_when_mixed() {
    let items = vec![
        make_project("proj", 2),
        make_worktree_with_program("sess-a", "claude"),
        make_worktree_with_program("sess-b", "codex"),
    ];
    let lines = render_tree(&items, 60, 4);
    assert!(
        lines[1].contains("(claude)"),
        "expected program suffix: {:?}",
        lines[1]
    );
    assert!(
        lines[2].contains("(codex)"),
        "expected program suffix: {:?}",
        lines[2]
    );
}

#[test]
fn test_program_suffix_hidden_when_flag_disabled() {
    let items = vec![
        make_project("proj", 2),
        make_worktree_with_program("sess-a", "claude"),
        make_worktree_with_program("sess-b", "codex"),
    ];
    let lines = render_tree_with(&items, 60, 4, |t| t.show_session_program(false));
    assert!(
        !lines[1].contains("(claude)"),
        "expected no program suffix: {:?}",
        lines[1]
    );
    assert!(
        !lines[2].contains("(codex)"),
        "expected no program suffix: {:?}",
        lines[2]
    );
}

#[test]
fn test_program_suffix_hidden_when_programs_uniform() {
    // Uniform program: suffix hidden regardless of the flag.
    let items = vec![
        make_project("proj", 2),
        make_worktree_with_program("sess-a", "claude"),
        make_worktree_with_program("sess-b", "claude"),
    ];
    let lines = render_tree(&items, 60, 4);
    assert!(
        !lines[1].contains("(claude)"),
        "uniform programs should not render suffix: {:?}",
        lines[1]
    );
}

#[test]
fn test_program_suffix_hidden_when_only_args_differ() {
    // Same base program with differing args is not "mixed": no suffix shown.
    let items = vec![
        make_project("proj", 2),
        make_worktree_with_program("sess-a", "claude"),
        make_worktree_with_program("sess-b", "claude --mode auto"),
    ];
    let lines = render_tree(&items, 60, 4);
    assert!(
        !lines[1].contains("(claude)"),
        "args-only difference should not render suffix: {:?}",
        lines[1]
    );
    assert!(
        !lines[2].contains("(claude"),
        "args-only difference should not render suffix: {:?}",
        lines[2]
    );
}

#[test]
fn test_program_suffix_shows_base_name_only_when_mixed() {
    // Different base programs trigger the suffix, but args are stripped from display.
    let items = vec![
        make_project("proj", 2),
        make_worktree_with_program("sess-a", "codex"),
        make_worktree_with_program("sess-b", "claude --mode auto"),
    ];
    let lines = render_tree(&items, 60, 4);
    assert!(
        lines[1].contains("(codex)"),
        "expected base program suffix: {:?}",
        lines[1]
    );
    assert!(
        lines[2].contains("(claude)") && !lines[2].contains("--mode"),
        "expected base name only, no args: {:?}",
        lines[2]
    );
}

// -- TreeListState boundary tests (cargo-mutants gap closure) --

#[test]
fn test_previous_on_empty_state_with_zero_index_does_not_underflow() {
    // Regression: `any_selectable()` must report `false` for an empty state so
    // that `previous()` early-returns before computing `count - 1`. If
    // `any_selectable` were to always return `true`, or if `item_count > 0`
    // were weakened to `>= 0`, this call would underflow `count - 1` (count=0)
    // and panic in debug builds.
    let mut state = TreeListState::new();
    // No `set_item_count`/`set_selectable` calls: item_count == 0, selectable empty.
    // Force selection to Some(0) so that `previous()` hits the `i == 0` arm
    // that does `count - 1` once it gets past the early-return guard.
    state.select(Some(0));
    state.previous();
    // No panic, no spurious selection change.
    assert_eq!(state.selected(), Some(0));
}

#[test]
fn test_next_on_empty_state_with_zero_index_is_a_noop() {
    // Mirror of the above test for `next()`: even if the early-return guard
    // were flipped, the modulo loop would refuse to select on an empty count,
    // but we still pin the observable behaviour to a no-op.
    let mut state = TreeListState::new();
    state.select(Some(0));
    state.next();
    assert_eq!(state.selected(), Some(0));
}

#[test]
fn test_navigation_noop_when_all_rows_unselectable() {
    // With every row unselectable, `any_selectable()` must return false. The
    // existing `selectable.iter().any(|s| *s)` branch is exercised here: if it
    // were replaced with `true`, the loop would still find no selectable index
    // but we still want to assert the observable contract — `next()` does not
    // produce a selection from `None`.
    let mut state = TreeListState::new();
    state.set_selectable(vec![false, false, false]);
    assert_eq!(state.selected(), None);
    state.next();
    assert_eq!(state.selected(), None);
    state.previous();
    assert_eq!(state.selected(), None);
}

#[test]
fn test_next_wraps_via_modulo_not_addition() {
    // Pin the wrap-around contract: from the last index, `next()` selects 0.
    // If `(i + 1) % count` were replaced with `(i + 1) + count`, the start
    // value would be `count * 2` rather than `0`; the subsequent
    // `(start + offset) % count` still normalises to zero, but we drive the
    // selectable mask so the *first* attempt lands on the wrap target.
    let mut state = TreeListState::new();
    state.set_item_count(4);
    state.select(Some(3));
    state.next();
    assert_eq!(state.selected(), Some(0));

    // And with an unselectable mask that forces traversal past the wrap point,
    // confirm we wrap forward to the next selectable row rather than walking
    // off the end.
    let mut state = TreeListState::new();
    state.set_selectable(vec![true, false, false, false]);
    state.select(Some(0));
    state.next();
    // Only index 0 is selectable, so `next()` from 0 wraps to 0.
    assert_eq!(state.selected(), Some(0));
}

#[test]
fn test_set_selectable_clears_out_of_range_selection() {
    // Kills `>= -> <` and `|| -> &&` mutants in set_selectable: when the
    // current selection is past the end of the new mask, the selection must
    // be cleared. `is_selectable(sel)` on an out-of-range index returns `true`
    // (via `unwrap_or(true)`), so the `!is_selectable(sel)` half of the `||`
    // is `false`; only the `sel >= item_count` half is `true`. Replacing `>=`
    // with `<` makes the whole expression `false`; replacing `||` with `&&`
    // also yields `false`. Either way, the mutant would *keep* the selection.
    let mut state = TreeListState::new();
    state.set_item_count(10);
    state.select(Some(7));

    state.set_selectable(vec![true, true, true]);
    assert_eq!(
        state.selected(),
        None,
        "out-of-range selection must be cleared"
    );
}

#[test]
fn test_set_selectable_clears_selection_on_unselectable_index() {
    // Kills `delete !` mutant: when the current selection is in-range but
    // points at an unselectable row, the selection must be cleared. With the
    // `!` deleted, the condition becomes `sel >= item_count || is_selectable(sel)`,
    // which is `false || false = false`, leaving the selection intact.
    let mut state = TreeListState::new();
    state.set_item_count(3);
    state.select(Some(1));

    state.set_selectable(vec![true, false, true]);
    assert_eq!(
        state.selected(),
        None,
        "selection on an unselectable row must be cleared"
    );
}

#[test]
fn test_set_selectable_preserves_valid_selection() {
    // Companion to the above: when the selection is in-range AND selectable,
    // it must be preserved. Guards against an overly-aggressive clear.
    let mut state = TreeListState::new();
    state.set_item_count(3);
    state.select(Some(2));

    state.set_selectable(vec![true, false, true]);
    assert_eq!(state.selected(), Some(2));
}
