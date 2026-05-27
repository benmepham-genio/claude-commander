use super::actions::adjust_list_scroll;
use super::modals::centered_rect;
use super::selection::session_number_to_list_index;
use super::*;

#[test]
fn test_centered_rect() {
    let area = Rect::new(0, 0, 100, 50);
    let centered = centered_rect(50, 50, area);

    // Should be roughly centered
    assert!(centered.x > 0);
    assert!(centered.y > 0);
    assert!(centered.width < area.width);
    assert!(centered.height < area.height);
}

#[test]
fn test_app_ui_state_default() {
    let state = AppUiState::default();
    assert!(state.list_items.is_empty());
    assert!(matches!(state.focused_pane, FocusedPane::SessionList));
    assert!(matches!(state.modal, Modal::None));
    assert!(!state.should_quit);
}

fn make_project() -> SessionListItem {
    SessionListItem::Project {
        id: ProjectId::new(),
        name: "test".to_string(),
        repo_path: std::path::PathBuf::from("/tmp/test"),
        main_branch: "main".to_string(),
        worktree_count: 0,
        nested: false,
    }
}

fn make_worktree() -> SessionListItem {
    SessionListItem::Worktree {
        id: SessionId::new(),
        project_id: ProjectId::new(),
        title: "test".to_string(),
        branch: "feat".to_string(),
        status: SessionStatus::Running,
        program: "claude".to_string(),
        pr_number: None,
        pr_url: None,
        pr_merged: false,
        pr_state: None,
        pr_draft: false,
        pr_labels: Vec::new(),
        worktree_path: std::path::PathBuf::from("/tmp/test"),
        created_at: chrono::Utc::now(),
        agent_state: None,
        unread: false,
        stacked_child: false,
    }
}

#[test]
fn test_session_number_to_list_index_basic() {
    let items = vec![
        make_project(),
        make_worktree(), // index 1, session #1
        make_worktree(), // index 2, session #2
        make_project(),
        make_worktree(), // index 4, session #3
    ];
    assert_eq!(session_number_to_list_index(&items, 1), Some(1));
    assert_eq!(session_number_to_list_index(&items, 2), Some(2));
    assert_eq!(session_number_to_list_index(&items, 3), Some(4));
}

#[test]
fn test_session_number_to_list_index_out_of_range() {
    let items = vec![make_project(), make_worktree()];
    assert_eq!(session_number_to_list_index(&items, 2), None);
    assert_eq!(session_number_to_list_index(&items, 0), None);
}

#[test]
fn test_session_number_to_list_index_empty() {
    let items: Vec<SessionListItem> = vec![];
    assert_eq!(session_number_to_list_index(&items, 1), None);
}

#[test]
fn test_session_number_to_list_index_projects_only() {
    let items = vec![make_project(), make_project()];
    assert_eq!(session_number_to_list_index(&items, 1), None);
}

// ---------------------------------------------------------------------------
// Quick-switch palette expansion: session + command matching
// ---------------------------------------------------------------------------

use crate::config::KeyBindings;

// --- effective_palette_mode -------------------------------------------------

#[test]
fn test_effective_mode_unified_plain_query_stays_unified() {
    assert_eq!(
        App::effective_palette_mode(PaletteMode::Unified, ""),
        PaletteMode::Unified
    );
    assert_eq!(
        App::effective_palette_mode(PaletteMode::Unified, "foo"),
        PaletteMode::Unified
    );
}

#[test]
fn test_effective_mode_unified_gt_prefix_promotes_to_command_only() {
    assert_eq!(
        App::effective_palette_mode(PaletteMode::Unified, ">"),
        PaletteMode::CommandOnly
    );
    assert_eq!(
        App::effective_palette_mode(PaletteMode::Unified, "> foo"),
        PaletteMode::CommandOnly
    );
    assert_eq!(
        App::effective_palette_mode(PaletteMode::Unified, ">foo"),
        PaletteMode::CommandOnly
    );
}

#[test]
fn test_effective_mode_command_only_is_sticky() {
    // Shift+leader entry mode stays CommandOnly regardless of query prefix
    assert_eq!(
        App::effective_palette_mode(PaletteMode::CommandOnly, ""),
        PaletteMode::CommandOnly
    );
    assert_eq!(
        App::effective_palette_mode(PaletteMode::CommandOnly, "foo"),
        PaletteMode::CommandOnly
    );
}

// --- palette_filter_query ---------------------------------------------------

#[test]
fn test_palette_filter_query_strips_gt_prefix_only_in_command_only() {
    // Unified keeps the query verbatim (session search uses `>` literally if typed)
    assert_eq!(
        App::palette_filter_query(PaletteMode::Unified, "foo"),
        "foo"
    );
    // CommandOnly derived from `>` prefix strips it and trims following space
    assert_eq!(App::palette_filter_query(PaletteMode::CommandOnly, ">"), "");
    assert_eq!(
        App::palette_filter_query(PaletteMode::CommandOnly, "> foo"),
        "foo"
    );
    assert_eq!(
        App::palette_filter_query(PaletteMode::CommandOnly, ">foo"),
        "foo"
    );
    // CommandOnly without a `>` prefix (Shift+leader entry) passes through
    assert_eq!(
        App::palette_filter_query(PaletteMode::CommandOnly, "foo"),
        "foo"
    );
}

// --- is_command_available ---------------------------------------------------

fn ui_state_with(
    session: Option<SessionId>,
    project: Option<ProjectId>,
    right_pane: RightPaneView,
) -> AppUiState {
    AppUiState {
        selected_session_id: session,
        selected_project_id: project,
        right_pane_view: right_pane,
        ..AppUiState::default()
    }
}

#[test]
fn test_is_command_available_session_scoped_hidden_without_session() {
    let s = ui_state_with(None, None, RightPaneView::Preview);
    for action in [
        BindableAction::Select,
        BindableAction::SelectShell,
        BindableAction::DeleteSession,
        BindableAction::RenameSession,
        BindableAction::RestartSession,
        BindableAction::OpenInEditor,
        BindableAction::OpenPullRequest,
    ] {
        assert!(
            !s.is_command_available(action),
            "{action:?} should be hidden without a session"
        );
    }
}

#[test]
fn test_is_command_available_session_scoped_shown_with_session() {
    let s = ui_state_with(Some(SessionId::new()), None, RightPaneView::Preview);
    for action in [
        BindableAction::Select,
        BindableAction::SelectShell,
        BindableAction::DeleteSession,
        BindableAction::RenameSession,
        BindableAction::RestartSession,
        BindableAction::OpenInEditor,
        BindableAction::OpenPullRequest,
    ] {
        assert!(
            s.is_command_available(action),
            "{action:?} should be available with a selected session"
        );
    }
}

#[test]
fn test_is_command_available_remove_project_requires_project_without_session() {
    // project selected, no session → shown
    let s = ui_state_with(None, Some(ProjectId::new()), RightPaneView::Preview);
    assert!(s.is_command_available(BindableAction::RemoveProject));
    // project selected but session also selected → hidden
    let s = ui_state_with(
        Some(SessionId::new()),
        Some(ProjectId::new()),
        RightPaneView::Preview,
    );
    assert!(!s.is_command_available(BindableAction::RemoveProject));
    // nothing selected → hidden
    let s = ui_state_with(None, None, RightPaneView::Preview);
    assert!(!s.is_command_available(BindableAction::RemoveProject));
}

#[test]
fn test_is_command_available_generate_summary_requires_info_pane_and_session() {
    // info pane + session → shown
    let s = ui_state_with(Some(SessionId::new()), None, RightPaneView::Info);
    assert!(s.is_command_available(BindableAction::GenerateSummary));
    // info pane but no session → hidden
    let s = ui_state_with(None, None, RightPaneView::Info);
    assert!(!s.is_command_available(BindableAction::GenerateSummary));
    // session but preview pane → hidden
    let s = ui_state_with(Some(SessionId::new()), None, RightPaneView::Preview);
    assert!(!s.is_command_available(BindableAction::GenerateSummary));
}

#[test]
fn test_is_command_available_unguarded_always_shown() {
    let s = ui_state_with(None, None, RightPaneView::Preview);
    for action in [
        BindableAction::NewSession,
        BindableAction::NewProject,
        BindableAction::CheckoutBranch,
        BindableAction::ScanDirectory,
        BindableAction::TogglePane,
        BindableAction::TogglePaneReverse,
        BindableAction::ShrinkLeftPane,
        BindableAction::GrowLeftPane,
        BindableAction::ShowHelp,
        BindableAction::ShowSettings,
        BindableAction::Quit,
        BindableAction::ScrollUp,
        BindableAction::ScrollDown,
        BindableAction::PageUp,
        BindableAction::PageDown,
    ] {
        assert!(
            s.is_command_available(action),
            "{action:?} should always be available"
        );
    }
}

// --- gather_command_entries -------------------------------------------------

#[test]
fn test_gather_command_entries_excludes_navigation() {
    let s = ui_state_with(Some(SessionId::new()), None, RightPaneView::Info);
    let kb = KeyBindings::default();
    let entries = s.gather_command_entries(&kb, "");
    for e in &entries {
        assert!(
            !matches!(
                e.action,
                BindableAction::NavigateUp | BindableAction::NavigateDown
            ),
            "palette should never list list-navigation actions, got {:?}",
            e.action
        );
    }
}

#[test]
fn test_gather_command_entries_hides_context_unavailable() {
    // nothing selected → session-scoped and GenerateSummary all hidden
    let s = ui_state_with(None, None, RightPaneView::Preview);
    let kb = KeyBindings::default();
    let entries = s.gather_command_entries(&kb, "");
    let actions: std::collections::HashSet<BindableAction> =
        entries.iter().map(|e| e.action).collect();
    for hidden in [
        BindableAction::DeleteSession,
        BindableAction::RenameSession,
        BindableAction::RestartSession,
        BindableAction::OpenInEditor,
        BindableAction::OpenPullRequest,
        BindableAction::GenerateSummary,
        BindableAction::RemoveProject,
    ] {
        assert!(
            !actions.contains(&hidden),
            "{hidden:?} should be hidden when nothing is selected"
        );
    }
}

#[test]
fn test_gather_command_entries_includes_actions_with_no_keybinding() {
    // ScrollUp/ScrollDown have no default binding (see KeyBindings::default()).
    // The palette is the primary access surface going forward, so commands
    // with no hotkey must still appear (with an empty `keys` field).
    let s = AppUiState::default();
    let kb = KeyBindings::default();
    let entries = s.gather_command_entries(&kb, "");

    let scroll_up = entries
        .iter()
        .find(|e| e.action == BindableAction::ScrollUp)
        .expect("ScrollUp should appear in the palette even without a keybinding");
    assert!(
        scroll_up.keys.is_empty(),
        "ScrollUp has no default binding, so `keys` must be empty"
    );

    let scroll_down = entries
        .iter()
        .find(|e| e.action == BindableAction::ScrollDown)
        .expect("ScrollDown should appear in the palette even without a keybinding");
    assert!(scroll_down.keys.is_empty());
}

#[test]
fn test_gather_command_entries_query_filters_by_label() {
    let s = ui_state_with(Some(SessionId::new()), None, RightPaneView::Info);
    let kb = KeyBindings::default();
    // "summary" matches only GenerateSummary (description "Generate AI summary")
    let entries = s.gather_command_entries(&kb, "summary");
    let actions: Vec<BindableAction> = entries.iter().map(|e| e.action).collect();
    assert_eq!(actions, vec![BindableAction::GenerateSummary]);
}

#[test]
fn test_gather_command_entries_case_insensitive() {
    let s = AppUiState::default();
    let kb = KeyBindings::default();
    let lower = s.gather_command_entries(&kb, "help");
    let upper = s.gather_command_entries(&kb, "HELP");
    let lower_actions: Vec<BindableAction> = lower.iter().map(|e| e.action).collect();
    let upper_actions: Vec<BindableAction> = upper.iter().map(|e| e.action).collect();
    assert_eq!(lower_actions, upper_actions);
    assert!(lower_actions.contains(&BindableAction::ShowHelp));
}

// ---------------------------------------------------------------------------
// Palette scroll: keep selection visible as the list exceeds max_visible
// ---------------------------------------------------------------------------

#[test]
fn test_adjust_list_scroll_noop_when_selection_in_window() {
    // selected in middle of window, scroll unchanged
    assert_eq!(adjust_list_scroll(5, 3, 10), 3);
    // selected at top of window, scroll unchanged
    assert_eq!(adjust_list_scroll(3, 3, 10), 3);
    // selected at bottom of window (scroll..scroll+visible is exclusive)
    assert_eq!(adjust_list_scroll(12, 3, 10), 3);
}

#[test]
fn test_adjust_list_scroll_pulls_up_when_selection_above() {
    // Pressing Up past the top of the window: scroll snaps to selected
    assert_eq!(adjust_list_scroll(2, 5, 10), 2);
    assert_eq!(adjust_list_scroll(0, 5, 10), 0);
}

#[test]
fn test_adjust_list_scroll_pushes_down_when_selection_below() {
    // Pressing Down off the bottom: scroll advances just enough to keep
    // the selection on the last visible row.
    assert_eq!(adjust_list_scroll(13, 3, 10), 4);
    assert_eq!(adjust_list_scroll(20, 0, 10), 11);
}

#[test]
fn test_adjust_list_scroll_wrap_up_from_top_lands_on_last_row() {
    // A 25-item list, currently at the top. Pressing Up wraps to index 24;
    // scroll must jump so 24 is visible.
    assert_eq!(adjust_list_scroll(24, 0, 10), 15);
}

#[test]
fn test_adjust_list_scroll_wrap_down_from_bottom_lands_on_first_row() {
    // 25-item list, selection on the last row; scrolled to the bottom.
    // Pressing Down wraps to 0, which is above the window — scroll to 0.
    assert_eq!(adjust_list_scroll(0, 15, 10), 0);
}

#[test]
fn test_adjust_list_scroll_zero_visible_rows_safe() {
    // Degenerate case: never panic.
    assert_eq!(adjust_list_scroll(5, 3, 0), 0);
}

#[test]
fn test_adjust_list_scroll_short_list_stays_at_top() {
    // When the list is shorter than the window, scroll should always be 0.
    // (The caller starts at 0; our function returns 0 because selected
    // is always in [0, visible).)
    assert_eq!(adjust_list_scroll(2, 0, 10), 0);
    assert_eq!(adjust_list_scroll(0, 0, 10), 0);
}

// ---------------------------------------------------------------------------
// ViewMode toggle
// ---------------------------------------------------------------------------

#[test]
fn test_view_mode_default_is_project_grouped() {
    let s = AppUiState::default();
    assert_eq!(s.view_mode, ViewMode::ProjectGrouped);
}

#[test]
fn test_toggle_view_mode_always_available_in_palette() {
    let s = AppUiState::default();
    assert!(s.is_command_available(BindableAction::ToggleViewMode));
}

// ---------------------------------------------------------------------------
// Stack chain info in Info pane
// ---------------------------------------------------------------------------

#[test]
fn test_info_view_renders_stack_chain() {
    use crate::tui::widgets::{InfoContent, InfoSessionData, InfoView};

    let theme = crate::tui::theme::Theme::basic();
    let diff = crate::git::DiffInfo::empty();
    let chain = vec![
        StackChainEntry {
            title: "base".into(),
            status: SessionStatus::Running,
            is_current: false,
        },
        StackChainEntry {
            title: "child".into(),
            status: SessionStatus::Running,
            is_current: true,
        },
    ];
    let data = InfoSessionData {
        title: "child".into(),
        branch: "child-br".into(),
        created_at: "now".into(),
        status: SessionStatus::Running,
        program: "claude".into(),
        worktree_path: "/tmp".into(),
        diff_info: &diff,
        pr_number: None,
        pr_url: None,
        pr_merged: false,
        enriched_pr: None,
        ai_summary: None,
        summary_key_hint: None,
        stack_chain: &chain,
    };
    let view = InfoView::new(InfoContent::Session(data), &theme);
    let lines = view.build_lines();
    let text: String = lines
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Stack (2 sessions)"),
        "should show stack header"
    );
    assert!(text.contains("base"), "should list base session");
    assert!(text.contains("← current"), "should mark current session");
}

#[test]
fn test_info_view_no_stack_section_for_unstacked() {
    use crate::tui::widgets::{InfoContent, InfoSessionData, InfoView};

    let theme = crate::tui::theme::Theme::basic();
    let diff = crate::git::DiffInfo::empty();
    let data = InfoSessionData {
        title: "solo".into(),
        branch: "solo-br".into(),
        created_at: "now".into(),
        status: SessionStatus::Running,
        program: "claude".into(),
        worktree_path: "/tmp".into(),
        diff_info: &diff,
        pr_number: None,
        pr_url: None,
        pr_merged: false,
        enriched_pr: None,
        ai_summary: None,
        summary_key_hint: None,
        stack_chain: &[],
    };
    let view = InfoView::new(InfoContent::Session(data), &theme);
    let lines = view.build_lines();
    let text: String = lines
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains("Stack"),
        "unstacked session should not show stack section"
    );
}

// ---------------------------------------------------------------------------
// Settings: build_settings_rows + apply_settings_edit for worktrees_dir
// ---------------------------------------------------------------------------

use crate::config::{AppState, ConfigStore, StateStore};

fn make_test_app() -> App {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let state_path = tmp.path().join("state.json");
    let config = Config::default();
    let config_store = Arc::new(ConfigStore::with_path(config, config_path));
    let store = Arc::new(StateStore::with_path(AppState::new(), state_path));
    // Leak the TempDir so paths stay valid for the lifetime of the test.
    std::mem::forget(tmp);
    App::new(config_store, store)
}

#[test]
fn test_worktrees_dir_row_shows_default_when_none() {
    let app = make_test_app();
    let rows = app.build_settings_rows(SettingsTab::General);
    let row = rows
        .iter()
        .find(|r| r.field_key == "worktrees_dir")
        .unwrap();
    assert_eq!(row.value, "(default)");
}

#[test]
fn test_worktrees_dir_row_shows_custom_path() {
    let mut app = make_test_app();
    app.config.worktrees_dir = Some(std::path::PathBuf::from("/custom/path"));
    let rows = app.build_settings_rows(SettingsTab::General);
    let row = rows
        .iter()
        .find(|r| r.field_key == "worktrees_dir")
        .unwrap();
    assert_eq!(row.value, "/custom/path");
}

#[test]
fn test_apply_worktrees_dir_sets_custom_path() {
    let mut app = make_test_app();
    app.apply_settings_edit(SettingsTab::General, "worktrees_dir", "/my/worktrees");
    assert_eq!(
        app.config.worktrees_dir,
        Some(std::path::PathBuf::from("/my/worktrees"))
    );
}

#[test]
fn test_apply_worktrees_dir_empty_clears_to_none() {
    let mut app = make_test_app();
    app.config.worktrees_dir = Some(std::path::PathBuf::from("/custom"));
    app.apply_settings_edit(SettingsTab::General, "worktrees_dir", "");
    assert_eq!(app.config.worktrees_dir, None);
}

#[test]
fn test_apply_worktrees_dir_default_sentinel_clears_to_none() {
    let mut app = make_test_app();
    app.config.worktrees_dir = Some(std::path::PathBuf::from("/custom"));
    app.apply_settings_edit(SettingsTab::General, "worktrees_dir", "(default)");
    assert_eq!(app.config.worktrees_dir, None);
}

#[test]
fn test_project_pull_rows_present_in_general_tab() {
    let app = make_test_app();
    let rows = app.build_settings_rows(SettingsTab::General);
    let enabled = rows
        .iter()
        .find(|r| r.field_key == "project_pull_enabled")
        .expect("project_pull_enabled row missing");
    assert_eq!(enabled.value, "true");
    let interval = rows
        .iter()
        .find(|r| r.field_key == "project_pull_interval_secs")
        .expect("project_pull_interval_secs row missing");
    assert_eq!(interval.value, "3600");
}

#[test]
fn test_apply_project_pull_enabled_round_trip() {
    let mut app = make_test_app();
    app.apply_settings_edit(SettingsTab::General, "project_pull_enabled", "true");
    assert!(app.config.project_pull_enabled);
    app.apply_settings_edit(SettingsTab::General, "project_pull_enabled", "false");
    assert!(!app.config.project_pull_enabled);
}

#[test]
fn test_apply_project_pull_interval_accepts_60_and_above() {
    let mut app = make_test_app();
    app.apply_settings_edit(SettingsTab::General, "project_pull_interval_secs", "120");
    assert_eq!(app.config.project_pull_interval_secs, 120);
    app.apply_settings_edit(SettingsTab::General, "project_pull_interval_secs", "60");
    assert_eq!(app.config.project_pull_interval_secs, 60);
}

#[test]
fn test_apply_project_pull_interval_rejects_below_60() {
    let mut app = make_test_app();
    app.config.project_pull_interval_secs = 3600;
    app.apply_settings_edit(SettingsTab::General, "project_pull_interval_secs", "30");
    assert_eq!(
        app.config.project_pull_interval_secs, 3600,
        "values below 60 must be rejected"
    );
    assert!(
        app.ui_state.status_message.is_some(),
        "rejection should surface a status message"
    );
}
