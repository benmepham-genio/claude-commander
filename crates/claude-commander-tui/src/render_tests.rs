//! Snapshot tests for TUI widget rendering
//!
//! Uses ratatui's TestBackend + insta for visual regression testing.
//! Run `cargo insta review` to accept/update snapshots.

use std::path::PathBuf;
use std::time::Instant;

use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    layout::{Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

use crate::app::{centered_rect, confirm_modal_area, pane_tabs};
use crate::theme::Theme;
use crate::widgets::{Preview, TreeList};
use claude_commander_core::git::DiffInfo;
use claude_commander_core::session::{ProjectId, SessionId, SessionListItem, SessionStatus};

/// Fixed theme for reproducible snapshots (no terminal detection)
fn test_theme() -> Theme {
    Theme::basic()
}

/// The frame the Info modal actually draws around [`InfoView`] (see
/// `App::render_info_modal`). Kept in one place so these snapshots track the
/// real chrome rather than a stale copy of it — the old pane-tab strip these
/// tests used disappeared with the right-hand preview pane in #260.
fn info_modal_area(area: Rect) -> Rect {
    centered_rect(70, 80, area)
}

fn info_modal_frame<'a>(theme: &Theme, session_title: &str, frame: &Frame) -> (Block<'a>, Rect) {
    let area = info_modal_area(frame.area());
    let block = Block::default()
        .title(format!(" Info — {session_title} "))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.modal_info));
    let inner = block.inner(area);
    (block, inner)
}

// ── Session list ───────────────────────────────────────────────────

#[test]
fn test_session_list_empty() {
    let backend = TestBackend::new(60, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let items: Vec<SessionListItem> = vec![];
            let tree_list = TreeList::new(&items, &theme)
                .highlight_style(theme.selection().add_modifier(Modifier::BOLD));
            frame.render_stateful_widget(
                tree_list,
                frame.area(),
                &mut ratatui::widgets::ListState::default(),
            );
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_session_list_single_project() {
    let backend = TestBackend::new(60, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let items = vec![SessionListItem::Project {
                id: ProjectId::new(),
                name: "my-project".to_string(),
                repo_path: PathBuf::from("/home/user/projects/my-project"),
                main_branch: "main".to_string(),
                worktree_count: 0,
                nested: false,
            }];
            let tree_list = TreeList::new(&items, &theme)
                .highlight_style(theme.selection().add_modifier(Modifier::BOLD));
            frame.render_stateful_widget(
                tree_list,
                frame.area(),
                &mut ratatui::widgets::ListState::default(),
            );
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_session_list_with_sessions() {
    let backend = TestBackend::new(70, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();
    let project_id = ProjectId::new();

    terminal
        .draw(|frame| {
            let items = vec![
                SessionListItem::Project {
                    id: project_id,
                    name: "claude-commander".to_string(),
                    repo_path: PathBuf::from("/home/user/projects/cc"),
                    main_branch: "main".to_string(),
                    worktree_count: 3,
                    nested: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Add auth feature".to_string(),
                    branch: "feature-auth".to_string(),
                    status: SessionStatus::Running,
                    program: "claude".to_string(),
                    pr_number: None,
                    pr_url: None,
                    pr_merged: false,
                    pr_state: None,
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Fix login bug".to_string(),
                    branch: "fix-login".to_string(),
                    status: SessionStatus::Stopped,
                    program: "claude".to_string(),
                    pr_number: None,
                    pr_url: None,
                    pr_merged: false,
                    pr_state: None,
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Refactor DB".to_string(),
                    branch: "refactor-db".to_string(),
                    status: SessionStatus::Stopped,
                    program: "claude".to_string(),
                    pr_number: None,
                    pr_url: None,
                    pr_merged: false,
                    pr_state: None,
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
            ];
            let tree_list = TreeList::new(&items, &theme)
                .highlight_style(theme.selection().add_modifier(Modifier::BOLD));
            frame.render_stateful_widget(
                tree_list,
                frame.area(),
                &mut ratatui::widgets::ListState::default(),
            );
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_session_list_with_pr_badges() {
    let backend = TestBackend::new(120, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();
    let project_id = ProjectId::new();

    terminal
        .draw(|frame| {
            let items = vec![
                SessionListItem::Project {
                    id: project_id,
                    name: "my-app".to_string(),
                    repo_path: PathBuf::from("/home/user/my-app"),
                    main_branch: "main".to_string(),
                    worktree_count: 2,
                    nested: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Add feature".to_string(),
                    branch: "feat-x".to_string(),
                    status: SessionStatus::Running,
                    program: "claude".to_string(),
                    pr_number: Some(42),
                    pr_url: Some("https://github.com/org/repo/pull/42".to_string()),
                    pr_merged: false,
                    pr_state: Some(claude_commander_core::git::PrState::Open),
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Old PR".to_string(),
                    branch: "old-pr".to_string(),
                    status: SessionStatus::Stopped,
                    program: "claude".to_string(),
                    pr_number: Some(10),
                    pr_url: Some("https://github.com/org/repo/pull/10".to_string()),
                    pr_merged: true,
                    pr_state: Some(claude_commander_core::git::PrState::Merged),
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
            ];
            let tree_list = TreeList::new(&items, &theme)
                .highlight_style(theme.selection().add_modifier(Modifier::BOLD));
            frame.render_stateful_widget(
                tree_list,
                frame.area(),
                &mut ratatui::widgets::ListState::default(),
            );
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_session_list_mixed_programs() {
    let backend = TestBackend::new(120, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();
    let project_id = ProjectId::new();

    terminal
        .draw(|frame| {
            let items = vec![
                SessionListItem::Project {
                    id: project_id,
                    name: "multi-agent".to_string(),
                    repo_path: PathBuf::from("/home/user/multi"),
                    main_branch: "main".to_string(),
                    worktree_count: 2,
                    nested: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Claude task".to_string(),
                    branch: "claude-task".to_string(),
                    status: SessionStatus::Running,
                    program: "claude".to_string(),
                    pr_number: None,
                    pr_url: None,
                    pr_merged: false,
                    pr_state: None,
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Aider task".to_string(),
                    branch: "aider-task".to_string(),
                    status: SessionStatus::Running,
                    program: "aider".to_string(),
                    pr_number: None,
                    pr_url: None,
                    pr_merged: false,
                    pr_state: None,
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
            ];
            let tree_list = TreeList::new(&items, &theme)
                .highlight_style(theme.selection().add_modifier(Modifier::BOLD));
            frame.render_stateful_widget(
                tree_list,
                frame.area(),
                &mut ratatui::widgets::ListState::default(),
            );
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_session_list_with_numbers() {
    let backend = TestBackend::new(70, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();
    let project_id = ProjectId::new();

    terminal
        .draw(|frame| {
            let items = vec![
                SessionListItem::Project {
                    id: project_id,
                    name: "claude-commander".to_string(),
                    repo_path: PathBuf::from("/home/user/projects/cc"),
                    main_branch: "main".to_string(),
                    worktree_count: 3,
                    nested: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Add auth feature".to_string(),
                    branch: "feature-auth".to_string(),
                    status: SessionStatus::Running,
                    program: "claude".to_string(),
                    pr_number: None,
                    pr_url: None,
                    pr_merged: false,
                    pr_state: None,
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Fix login bug".to_string(),
                    branch: "fix-login".to_string(),
                    status: SessionStatus::Stopped,
                    program: "claude".to_string(),
                    pr_number: None,
                    pr_url: None,
                    pr_merged: false,
                    pr_state: None,
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Refactor DB".to_string(),
                    branch: "refactor-db".to_string(),
                    status: SessionStatus::Stopped,
                    program: "claude".to_string(),
                    pr_number: None,
                    pr_url: None,
                    pr_merged: false,
                    pr_state: None,
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
            ];
            let tree_list = TreeList::new(&items, &theme)
                .highlight_style(theme.selection().add_modifier(Modifier::BOLD));
            frame.render_stateful_widget(
                tree_list,
                frame.area(),
                &mut ratatui::widgets::ListState::default(),
            );
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

// ── Info view ──────────────────────────────────────────────────────

#[test]
fn test_info_view_empty() {
    use crate::widgets::{InfoContent, InfoView};

    let backend = TestBackend::new(60, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let (block, inner) = info_modal_frame(&theme, "demo", frame);
            frame.render_widget(block, info_modal_area(frame.area()));
            let info_view = InfoView::new(InfoContent::Empty, &theme).scroll(0);
            frame.render_widget(info_view, inner);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_info_view_session_with_pr() {
    use crate::widgets::{InfoContent, InfoSessionData, InfoView};
    use claude_commander_core::git::{AiSummary, ChecksStatus, EnrichedPrInfo, PrLabel, PrState};

    let backend = TestBackend::new(70, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    let diff = DiffInfo {
        diff: "+added\n-removed\n".to_string(),
        files_changed: 2,
        lines_added: 10,
        lines_removed: 5,
        line_count: 2,
        computed_at: Instant::now(),
        base_commit: String::new(),
    };
    let pr = EnrichedPrInfo {
        number: 42,
        url: "https://github.com/org/repo/pull/42".to_string(),
        title: "Add authentication flow".to_string(),
        state: PrState::Open,
        is_draft: false,
        labels: vec![
            PrLabel {
                name: "bug".into(),
                color: "d73a4a".into(),
            },
            PrLabel {
                name: "enhancement".into(),
                color: "a2eeef".into(),
            },
        ],
        checks_status: ChecksStatus::Passing,
        body: "This PR adds OAuth2 auth.".to_string(),
    };

    terminal
        .draw(|frame| {
            let data = InfoSessionData {
                title: "auth-session".into(),
                branch: "feature-auth".into(),
                created_at: "2026-04-01 12:00 UTC".into(),
                status: SessionStatus::Running,
                program: "claude".into(),
                worktree_path: "/tmp/wt/auth".into(),
                diff_info: &diff,
                pr_number: Some(42),
                pr_url: Some("https://github.com/org/repo/pull/42".into()),
                pr_merged: false,
                review_label: "PR",
                enriched_pr: Some(&pr),
                ai_summary: Some(&AiSummary::Ready {
                    text: "Adds OAuth2 authentication.".into(),
                    diff_hash: 123,
                }),
                summary_key_hint: Some("g".into()),
                stack_chain: &[],
            };
            let (block, inner) = info_modal_frame(&theme, "demo", frame);
            frame.render_widget(block, info_modal_area(frame.area()));
            let info_view = InfoView::new(InfoContent::Session(data), &theme).scroll(0);
            frame.render_widget(info_view, inner);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_info_view_long_text_wraps() {
    use crate::widgets::{InfoContent, InfoSessionData, InfoView};
    use claude_commander_core::git::{AiSummary, ChecksStatus, EnrichedPrInfo, PrState};

    let backend = TestBackend::new(50, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    let diff = DiffInfo {
        diff: "+a\n".to_string(),
        files_changed: 1,
        lines_added: 1,
        lines_removed: 0,
        line_count: 1,
        computed_at: Instant::now(),
        base_commit: String::new(),
    };
    let pr = EnrichedPrInfo {
        number: 99,
        url: "https://github.com/org/repo/pull/99".to_string(),
        title: "A very long PR title that should definitely wrap past the edge of a narrow pane".to_string(),
        state: PrState::Open,
        is_draft: false,
        labels: vec![],
        checks_status: ChecksStatus::Passing,
        body: "This is a long description that tests word wrapping behavior in the info pane to make sure nothing goes off the right edge.".to_string(),
    };

    terminal
        .draw(|frame| {
            let data = InfoSessionData {
                title: "test-session".into(),
                branch: "test-branch".into(),
                created_at: "2026-04-01 12:00 UTC".into(),
                status: SessionStatus::Running,
                program: "claude".into(),
                worktree_path: "/tmp/wt".into(),
                diff_info: &diff,
                pr_number: Some(99),
                pr_url: Some("https://github.com/org/repo/pull/99".into()),
                pr_merged: false,
                review_label: "PR",
                enriched_pr: Some(&pr),
                ai_summary: Some(&AiSummary::Ready {
                    text: "This summary is intentionally long to verify that the info pane correctly wraps text at the pane boundary instead of clipping it.".into(),
                    diff_hash: 1,
                }),
                summary_key_hint: Some("g".into()),
                stack_chain: &[],
            };
            let (block, inner) = info_modal_frame(&theme, "demo", frame);
            frame.render_widget(block, info_modal_area(frame.area()));
            let info_view = InfoView::new(InfoContent::Session(data), &theme).scroll(0);
            frame.render_widget(info_view, inner);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_info_view_summary_placeholder() {
    use crate::widgets::{InfoContent, InfoSessionData, InfoView};

    let backend = TestBackend::new(60, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    let diff = DiffInfo::empty();
    terminal
        .draw(|frame| {
            let data = InfoSessionData {
                title: "my-session".into(),
                branch: "feature-x".into(),
                created_at: "2026-04-10 09:00 UTC".into(),
                status: SessionStatus::Running,
                program: "claude".into(),
                worktree_path: "/tmp/wt".into(),
                diff_info: &diff,
                pr_number: None,
                pr_url: None,
                pr_merged: false,
                review_label: "PR",
                enriched_pr: None,
                ai_summary: None,
                summary_key_hint: Some("g".into()),
                stack_chain: &[],
            };
            let (block, inner) = info_modal_frame(&theme, "demo", frame);
            frame.render_widget(block, info_modal_area(frame.area()));
            let info_view = InfoView::new(InfoContent::Session(data), &theme).scroll(0);
            frame.render_widget(info_view, inner);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

// ── Modals ─────────────────────────────────────────────────────────

#[test]
fn test_modal_input() {
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let modal_area = centered_rect(60, 20, area);
            frame.render_widget(Clear, modal_area);

            let block = Block::default()
                .title(" New Session ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.modal_warning));

            let inner = block.inner(modal_area);
            frame.render_widget(block, modal_area);

            let text = "Enter session name:\n\n> my-feature_";
            let paragraph = Paragraph::new(text);
            frame.render_widget(paragraph, inner);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_modal_confirm() {
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let text = "Delete session 'fix-login'?\n\n[Enter] Confirm  [Esc] Cancel";
            // Mirror the app's real Confirm geometry so the snapshot tracks it.
            let modal_area = confirm_modal_area("Delete session 'fix-login'?", area);
            frame.render_widget(Clear, modal_area);

            let block = Block::default()
                .title(" Delete Session ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.modal_error));

            let inner = block.inner(modal_area);
            frame.render_widget(block, modal_area);

            let paragraph = Paragraph::new(text).wrap(Wrap { trim: true });
            frame.render_widget(paragraph, inner);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_modal_confirm_restart() {
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let message = "This will kill the current tmux session and start a fresh one.\nClaude will pick up where it left off via /resume.";
            let text = "This will kill the current tmux session and start a fresh one.\nClaude will pick up where it left off via /resume.\n\n[Enter] Confirm  [Esc] Cancel";
            // Mirror the app's real Confirm geometry so the snapshot tracks it.
            let modal_area = confirm_modal_area(message, area);
            frame.render_widget(Clear, modal_area);

            let block = Block::default()
                .title(" Restart Session ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.modal_error));

            let inner = block.inner(modal_area);
            frame.render_widget(block, modal_area);

            let paragraph = Paragraph::new(text).wrap(Wrap { trim: true });
            frame.render_widget(paragraph, inner);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_modal_error() {
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let modal_area = centered_rect(60, 20, area);
            frame.render_widget(Clear, modal_area);

            let block = Block::default()
                .title(" Error ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.modal_error));

            let inner = block.inner(modal_area);
            frame.render_widget(block, modal_area);

            let text =
                "Failed to create session: git worktree add failed\n\nPress any key to close.";
            let paragraph = Paragraph::new(text).wrap(Wrap { trim: true });
            frame.render_widget(paragraph, inner);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_modal_help() {
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let modal_area = centered_rect(70, 80, area);
            frame.render_widget(Clear, modal_area);

            let block = Block::default()
                .title(" Help ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.modal_info));

            let inner = block.inner(modal_area);
            frame.render_widget(block, modal_area);

            let content_area = inner.inner(Margin {
                horizontal: 2,
                vertical: 1,
            });

            let help_lines = vec![
                Line::from("Navigation:"),
                Line::from("  j/k, Up/Down    Navigate session list"),
                Line::from("  Enter           Attach to selected session"),
                Line::from("  Tab/Shift+Tab   Toggle preview/diff/shell view"),
                Line::from(""),
                Line::from("Session Management:"),
                Line::from("  n               New worktree session"),
                Line::from("  N               New project (add git repo)"),
                Line::from("  d               Delete/kill session"),
                Line::from(""),
                Line::from("Press any key to close this help."),
            ];

            let paragraph = Paragraph::new(help_lines);
            frame.render_widget(paragraph, content_area);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_modal_loading() {
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let modal_area = centered_rect(60, 20, area);
            frame.render_widget(Clear, modal_area);

            let block = Block::default()
                .title(" New Session ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.modal_info));

            let inner = block.inner(modal_area);
            frame.render_widget(block, modal_area);

            let text = "⠋ Creating \"my-feature\"...";
            let paragraph = Paragraph::new(text);
            frame.render_widget(paragraph, inner);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

// ── Status bar ─────────────────────────────────────────────────────

#[test]
fn test_status_bar_default() {
    let backend = TestBackend::new(120, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let text = "Sessions: 3 | Press ? for help | n: new session | N: add project";
            let paragraph = Paragraph::new(text).style(theme.status_bar());
            frame.render_widget(paragraph, frame.area());
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_status_bar_with_message() {
    let backend = TestBackend::new(120, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let text = "Created session abc12345";
            let paragraph = Paragraph::new(text).style(theme.status_bar());
            frame.render_widget(paragraph, frame.area());
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

// ── Quick-switch modal ─────────────────────────────────────────────

#[test]
fn test_quick_switch_empty_query() {
    let backend = TestBackend::new(80, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let modal_width = area.width * 60 / 100;
            let modal_area = Rect {
                x: area.x + (area.width - modal_width) / 2,
                y: area.y + area.height / 5,
                width: modal_width,
                height: 3, // border + input + border
            };
            frame.render_widget(Clear, modal_area);

            let block = Block::default()
                .title(" Quick Switch ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.modal_info));
            let inner = block.inner(modal_area);
            frame.render_widget(block, modal_area);

            let input_line = Line::from("> _");
            frame.render_widget(Paragraph::new(input_line), inner);
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_quick_switch_with_matches() {
    let backend = TestBackend::new(80, 15);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let modal_width = area.width * 60 / 100;
            let matches = [
                (
                    "●",
                    theme.status_running,
                    "Add auth",
                    "feature-auth",
                    "my-app",
                    true,
                ),
                (
                    "○",
                    theme.status_stopped,
                    "Fix login",
                    "fix-login",
                    "my-app",
                    false,
                ),
                (
                    "○",
                    theme.status_stopped,
                    "Old task",
                    "old-branch",
                    "other",
                    false,
                ),
            ];
            let modal_area = Rect {
                x: area.x + (area.width - modal_width) / 2,
                y: area.y + area.height / 5,
                width: modal_width,
                height: 3 + matches.len() as u16,
            };
            frame.render_widget(Clear, modal_area);

            let block = Block::default()
                .title(" Quick Switch ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.modal_info));
            let inner = block.inner(modal_area);
            frame.render_widget(block, modal_area);

            // Input line
            let input_area = Rect { height: 1, ..inner };
            frame.render_widget(Paragraph::new(Line::from("❯ auth_")), input_area);

            // Match lines
            for (i, (icon, color, title, branch, project, selected)) in matches.iter().enumerate() {
                let row = inner.y + 1 + i as u16;
                let mut spans = vec![
                    Span::styled(format!(" {} ", icon), Style::default().fg(*color)),
                    Span::styled(
                        title.to_string(),
                        if *selected {
                            theme.selection()
                        } else {
                            Style::default()
                        },
                    ),
                ];
                if let Some(shown_branch) =
                    claude_commander_core::session::display_branch(title, branch)
                {
                    spans.push(Span::styled(
                        format!(" [{}]", shown_branch),
                        Style::default().fg(theme.text_accent),
                    ));
                }
                spans.push(Span::styled(
                    format!(" ({})", project),
                    Style::default().fg(theme.text_secondary),
                ));
                let line = Line::from(spans);
                let line_area = Rect {
                    y: row,
                    height: 1,
                    ..inner
                };
                frame.render_widget(Paragraph::new(line), line_area);
            }
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_session_list_creating_status() {
    let backend = TestBackend::new(70, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();
    let project_id = ProjectId::new();

    terminal
        .draw(|frame| {
            let items = vec![
                SessionListItem::Project {
                    id: project_id,
                    name: "my-project".to_string(),
                    repo_path: PathBuf::from("/home/user/my-project"),
                    main_branch: "main".to_string(),
                    worktree_count: 2,
                    nested: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "Existing session".to_string(),
                    branch: "feature-existing".to_string(),
                    status: SessionStatus::Running,
                    program: "claude".to_string(),
                    pr_number: None,
                    pr_url: None,
                    pr_merged: false,
                    pr_state: None,
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::from("/tmp/wt"),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
                SessionListItem::Worktree {
                    id: SessionId::new(),
                    project_id,
                    title: "New session".to_string(),
                    branch: "feature-new".to_string(),
                    status: SessionStatus::Creating,
                    program: "claude".to_string(),
                    pr_number: None,
                    pr_url: None,
                    pr_merged: false,
                    pr_state: None,
                    pr_draft: false,
                    pr_labels: Vec::new(),
                    worktree_path: PathBuf::new(),
                    created_at: chrono::Utc::now(),
                    agent_state: None,
                    unread: false,
                    keep_alive: false,
                    lfs_pulling: false,
                    stacked_child: false,
                },
            ];
            // tick=0 → spinner frame 0 → "⠋"
            let tree_list = TreeList::new(&items, &theme)
                .tick(0)
                .highlight_style(theme.selection().add_modifier(Modifier::BOLD));
            frame.render_stateful_widget(
                tree_list,
                frame.area(),
                &mut ratatui::widgets::ListState::default(),
            );
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

// ── Preview pane (restored with the right-hand pane in #267) ───────

#[test]
fn test_preview_empty() {
    let backend = TestBackend::new(60, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    terminal
        .draw(|frame| {
            let preview = Preview::new("")
                .block(
                    Block::default()
                        .title(pane_tabs(&theme, &["Preview", "Info", "Shell"], 0))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                )
                .scroll(0);
            frame.render_widget(preview, frame.area());
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_preview_with_content() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    let content = "$ claude --resume\n\nClaude is thinking...\n\n> I'll help you fix the auth bug.\n> Let me look at the code first.\n\nReading src/auth.rs...";

    terminal
        .draw(|frame| {
            let preview = Preview::new(content)
                .block(
                    Block::default()
                        .title(pane_tabs(&theme, &["Preview", "Info", "Shell"], 0))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                )
                .scroll(0);
            frame.render_widget(preview, frame.area());
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_preview_scrolled() {
    let backend = TestBackend::new(60, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    let content = (0..50)
        .map(|i| format!("Line {}: some content here", i))
        .collect::<Vec<_>>()
        .join("\n");

    terminal
        .draw(|frame| {
            let preview = Preview::new(&content)
                .block(
                    Block::default()
                        .title(pane_tabs(&theme, &["Preview", "Info", "Shell"], 0))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                )
                .scroll(20);
            frame.render_widget(preview, frame.area());
        })
        .unwrap();

    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_preview_content_replacement_no_clear() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    // Render content A (simulating session 1 selected)
    terminal
        .draw(|frame| {
            let preview = Preview::new("Session 1 output\nLine 2\nLine 3\nLine 4\nLine 5")
                .block(
                    Block::default()
                        .title(pane_tabs(&theme, &["Preview", "Info", "Shell"], 0))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                )
                .scroll(0);
            frame.render_widget(preview, frame.area());
        })
        .unwrap();

    // Render content B WITHOUT clearing (simulating scrolling to session 2)
    terminal
        .draw(|frame| {
            let preview = Preview::new("Session 2 output\nDifferent content")
                .block(
                    Block::default()
                        .title(pane_tabs(&theme, &["Preview", "Info", "Shell"], 0))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                )
                .scroll(0);
            frame.render_widget(preview, frame.area());
        })
        .unwrap();

    // Snapshot should show clean content B with no artifacts from A
    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_preview_to_info_view_switch_no_clear() {
    use crate::widgets::{InfoContent, InfoSessionData, InfoView};

    let backend = TestBackend::new(70, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    // Render Preview pane first
    terminal
        .draw(|frame| {
            let preview = Preview::new("Preview content here\nLine 2\nLine 3")
                .block(
                    Block::default()
                        .title(pane_tabs(&theme, &["Preview", "Info", "Shell"], 0))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                )
                .scroll(0);
            frame.render_widget(preview, frame.area());
        })
        .unwrap();

    // Switch to Info view WITHOUT clearing
    let diff = DiffInfo::empty();
    terminal
        .draw(|frame| {
            let data = InfoSessionData {
                title: "test-session".into(),
                branch: "test-branch".into(),
                created_at: "2026-04-01 12:00 UTC".into(),
                status: SessionStatus::Running,
                program: "claude".into(),
                worktree_path: "/tmp/wt".into(),
                diff_info: &diff,
                pr_number: None,
                pr_url: None,
                pr_merged: false,
                review_label: "PR",
                enriched_pr: None,
                ai_summary: None,
                summary_key_hint: Some("g".into()),
                stack_chain: &[],
            };
            // `InfoView` lost its `.block()` builder in #260 — the right pane
            // (and the Info modal) draws the block and renders the view into
            // `block.inner(...)`. Mirror that so the switch is snapshotted the
            // way it actually happens.
            let block = Block::default()
                .title(pane_tabs(&theme, &["Preview", "Info", "Shell"], 1))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded);
            let inner = block.inner(frame.area());
            frame.render_widget(block, frame.area());
            frame.render_widget(
                InfoView::new(InfoContent::Session(data), &theme).scroll(0),
                inner,
            );
        })
        .unwrap();

    // Snapshot should show clean Info view with no Preview artifacts
    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn test_preview_to_shell_view_switch_no_clear() {
    let backend = TestBackend::new(60, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = test_theme();

    // Render Preview pane first
    terminal
        .draw(|frame| {
            let preview = Preview::new("Preview content\nWith multiple lines\nOf output")
                .block(
                    Block::default()
                        .title(pane_tabs(&theme, &["Preview", "Info", "Shell"], 0))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                )
                .scroll(0);
            frame.render_widget(preview, frame.area());
        })
        .unwrap();

    // Switch to Shell view WITHOUT clearing
    terminal
        .draw(|frame| {
            let shell = Preview::new("$ ls -la\ntotal 42\ndrwxr-xr-x 5 user user 4096")
                .block(
                    Block::default()
                        .title(pane_tabs(&theme, &["Preview", "Info", "Shell"], 2))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                )
                .scroll(0);
            frame.render_widget(shell, frame.area());
        })
        .unwrap();

    // Snapshot should show clean Shell view with no Preview artifacts
    insta::assert_snapshot!(terminal.backend());
}

// ── Telemetry must never be live in this crate's tests ─────────────

/// Core's `would_be_enabled` short-circuit is `cfg!(test)`, which is true only
/// for core's OWN test binary. This crate compiles core as a normal dependency,
/// so extracting the frontend silently re-enabled telemetry for every test here —
/// each `App::new` built a live sink and posted `session_start` to the production
/// stream (the added latency also made timing-sensitive async tests flaky).
///
/// The fix is core's `test-support` feature, which this crate enables as a
/// dev-dependency. This pins it: if that guard is dropped or the dev-dependency
/// loses the feature, this fails instead of quietly shipping test telemetry.
///
/// It asserts on a *default* config — i.e. telemetry nominally on — so it tests
/// the build-level backstop, not merely a fixture that opts out.
///
/// Caveat worth knowing: `scripts/verify.sh` and `ci.yml` export
/// `DO_NOT_TRACK=1`, under which this passes via that route regardless of the
/// feature clause. So it is a bare `cargo test` that actually exercises the
/// clause — which is also the case that matters, since that is what a developer
/// runs and what has no other backstop.
#[test]
fn telemetry_is_never_live_in_this_crates_tests() {
    let nominally_on = claude_commander_core::config::TelemetryConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(
        !claude_commander_core::telemetry::would_be_enabled(&nominally_on),
        "telemetry must be inert when core is built with `test-support`; \
         otherwise this crate's suite posts events to the live stream"
    );
}
