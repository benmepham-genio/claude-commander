//! Info pane widget
//!
//! Displays session metadata, PR details, and AI-generated summaries.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Widget, Wrap},
};

use crate::git::{AiSummary, ChecksStatus, DiffInfo, EnrichedPrInfo, PrState};
use crate::session::SessionStatus;
use crate::tui::theme::Theme;

/// Data required to render the Info pane for a session.
pub struct InfoSessionData<'a> {
    pub title: String,
    pub branch: String,
    pub created_at: String,
    pub status: SessionStatus,
    pub program: String,
    pub worktree_path: String,
    pub diff_info: &'a DiffInfo,
    pub pr_number: Option<u32>,
    pub pr_url: Option<String>,
    pub pr_merged: bool,
    pub enriched_pr: Option<&'a EnrichedPrInfo>,
    pub ai_summary: Option<&'a AiSummary>,
    /// Display string for the generate-summary hotkey (e.g. "g"). None = AI disabled.
    pub summary_key_hint: Option<String>,
}

/// Data required to render the Info pane for a project.
pub struct InfoProjectData {
    pub name: String,
    pub repo_path: String,
    pub main_branch: String,
}

/// Info pane content — either session or project data.
pub enum InfoContent<'a> {
    Session(InfoSessionData<'a>),
    Project(InfoProjectData),
    Empty,
}

/// Info view widget for the right pane.
pub struct InfoView<'a> {
    content: InfoContent<'a>,
    theme: &'a Theme,
    block: Option<Block<'a>>,
    scroll: u16,
    /// Pre-built lines to render, bypassing `build_lines()` during `Widget::render`.
    prebuilt_lines: Option<Vec<Line<'static>>>,
}

impl<'a> InfoView<'a> {
    pub fn new(content: InfoContent<'a>, theme: &'a Theme) -> Self {
        Self {
            content,
            theme,
            block: None,
            scroll: 0,
            prebuilt_lines: None,
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    pub fn scroll(mut self, scroll: u16) -> Self {
        self.scroll = scroll;
        self
    }

    /// Supply pre-built lines so that `Widget::render` skips `build_lines()`.
    pub fn with_prebuilt_lines(mut self, lines: Vec<Line<'static>>) -> Self {
        self.prebuilt_lines = Some(lines);
        self
    }

    /// Build all content lines. Returns owned lines (no lifetime dependency on self).
    pub fn build_lines(&self) -> Vec<Line<'static>> {
        match &self.content {
            InfoContent::Session(data) => self.build_session_lines(data),
            InfoContent::Project(data) => self.build_project_lines(data),
            InfoContent::Empty => vec![Line::from(Span::styled(
                "Select a session to see info",
                self.secondary_style(),
            ))],
        }
    }

    fn build_session_lines(&self, data: &InfoSessionData<'_>) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let label = self.label_style();
        let value = self.value_style();

        lines.push(Line::from(vec![
            Span::styled(" Session: ", label),
            Span::styled(data.title.clone(), value),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Branch:  ", label),
            Span::styled(data.branch.clone(), value),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Created: ", label),
            Span::styled(data.created_at.clone(), value),
        ]));

        let (icon, color) = match data.status {
            SessionStatus::Creating => ("…", self.theme.status_creating),
            SessionStatus::Running => ("●", self.theme.status_running),
            SessionStatus::Stopped => ("○", self.theme.status_stopped),
            SessionStatus::Merging => ("⟳", self.theme.status_creating),
            SessionStatus::CascadePaused => ("⏸", self.theme.agent_waiting),
            SessionStatus::Pushing => ("↑", self.theme.status_creating),
        };
        lines.push(Line::from(vec![
            Span::styled(" Status:  ", label),
            Span::styled(icon, Style::default().fg(color)),
            Span::styled(format!(" {}", data.status), value),
        ]));

        lines.push(Line::from(vec![
            Span::styled(" Program: ", label),
            Span::styled(data.program.clone(), value),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Path:    ", label),
            Span::styled(data.worktree_path.clone(), value),
        ]));

        if data.diff_info.has_changes() {
            lines.push(Line::from(vec![
                Span::styled(" Changes: ", label),
                Span::styled(format!("{} file(s), ", data.diff_info.files_changed), value),
                Span::styled(
                    format!("+{}", data.diff_info.lines_added),
                    Style::default().fg(self.theme.diff_added),
                ),
                Span::styled(" ", value),
                Span::styled(
                    format!("-{}", data.diff_info.lines_removed),
                    Style::default().fg(self.theme.diff_removed),
                ),
                Span::styled(" lines", value),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(" Changes: ", label),
                Span::styled("No changes", self.secondary_style()),
            ]));
        }

        // Separator
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " ─────────────────────────────────",
            self.secondary_style(),
        )));

        // PR section
        self.build_pr_lines(data, &mut lines);

        // AI summary section (only when AI is enabled, i.e. key hint is present)
        if let Some(ref key_hint) = data.summary_key_hint {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " ─────────────────────────────────",
                self.secondary_style(),
            )));
            if let Some(summary) = data.ai_summary {
                self.build_summary_lines(summary, &mut lines);
            } else {
                // No summary generated yet — show placeholder with hotkey hint
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled(" Summary: ", label),
                    Span::styled(
                        format!("Press {key_hint} to generate"),
                        Style::default().fg(self.theme.text_accent),
                    ),
                ]));
            }
        }

        lines
    }

    fn build_pr_lines(&self, data: &InfoSessionData<'_>, lines: &mut Vec<Line<'static>>) {
        let label = self.label_style();
        let value = self.value_style();

        if let Some(pr) = data.enriched_pr {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(format!(" PR #{}: ", pr.number), label),
                Span::styled(
                    pr.title.clone(),
                    Style::default()
                        .fg(self.theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));

            if !pr.url.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(" URL:    ", label),
                    Span::styled(pr.url.clone(), Style::default().fg(self.theme.text_accent)),
                ]));
            }

            let (state_icon, state_color) = match pr.state {
                PrState::Open => ("●", self.theme.status_pr),
                PrState::Closed => ("●", self.theme.status_stopped),
                PrState::Merged => ("●", self.theme.status_pr_merged),
            };
            let state_text = if pr.is_draft {
                format!(" {} (Draft)", pr.state)
            } else {
                format!(" {}", pr.state)
            };
            lines.push(Line::from(vec![
                Span::styled(" State:  ", label),
                Span::styled(state_icon, Style::default().fg(state_color)),
                Span::styled(state_text, value),
            ]));

            if !pr.labels.is_empty() {
                let mut spans = vec![Span::styled(" Labels: ", label)];
                for (i, lbl) in pr.labels.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::styled("  ", value));
                    }
                    let color = parse_hex_color(&lbl.color).unwrap_or(self.theme.text_accent);
                    spans.push(Span::styled(lbl.name.clone(), Style::default().fg(color)));
                }
                lines.push(Line::from(spans));
            }

            let (ci_icon, ci_color, ci_text) = match pr.checks_status {
                ChecksStatus::Passing => ("✓", self.theme.diff_added, "Passing"),
                ChecksStatus::Failing => ("✗", self.theme.diff_removed, "Failing"),
                ChecksStatus::Pending => ("◌", self.theme.modal_warning, "Pending"),
                ChecksStatus::None => ("—", self.theme.text_secondary, "No checks"),
            };
            lines.push(Line::from(vec![
                Span::styled(" CI:     ", label),
                Span::styled(ci_icon, Style::default().fg(ci_color)),
                Span::styled(format!(" {ci_text}"), value),
            ]));

            if !pr.body.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(" Description:", label)));
                for body_line in pr.body.lines() {
                    lines.push(Line::from(Span::styled(
                        format!(" {body_line}"),
                        self.secondary_style(),
                    )));
                }
            }
        } else if let Some(pr_num) = data.pr_number {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(format!(" PR #{pr_num}"), label),
                if data.pr_merged {
                    Span::styled(" (merged)", self.secondary_style())
                } else {
                    Span::styled(" (open)", self.secondary_style())
                },
            ]));
            if let Some(ref url) = data.pr_url {
                lines.push(Line::from(vec![
                    Span::styled(" URL:    ", label),
                    Span::styled(url.clone(), Style::default().fg(self.theme.text_accent)),
                ]));
            }
        } else {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(" PR: ", label),
                Span::styled("None", self.secondary_style()),
            ]));
        }
    }

    fn build_summary_lines(&self, summary: &AiSummary, lines: &mut Vec<Line<'static>>) {
        let label = self.label_style();

        match summary {
            AiSummary::Loading => {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled(" Summary: ", label),
                    Span::styled("Generating...", self.secondary_style()),
                ]));
            }
            AiSummary::Ready { text, .. } => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(" Summary:", label)));
                for text_line in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!(" {text_line}"),
                        self.secondary_style(),
                    )));
                }
            }
            AiSummary::Error(msg) => {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled(" Summary: ", label),
                    Span::styled(msg.clone(), self.secondary_style()),
                ]));
            }
        }
    }

    fn build_project_lines(&self, data: &InfoProjectData) -> Vec<Line<'static>> {
        let label = self.label_style();
        let value = self.value_style();

        vec![
            Line::from(vec![
                Span::styled(" Project: ", label),
                Span::styled(data.name.clone(), value),
            ]),
            Line::from(vec![
                Span::styled(" Path:    ", label),
                Span::styled(data.repo_path.clone(), value),
            ]),
            Line::from(vec![
                Span::styled(" Branch:  ", label),
                Span::styled(data.main_branch.clone(), value),
            ]),
        ]
    }

    fn label_style(&self) -> Style {
        Style::default()
            .fg(self.theme.text_accent)
            .add_modifier(Modifier::BOLD)
    }

    fn value_style(&self) -> Style {
        Style::default().fg(self.theme.text_primary)
    }

    fn secondary_style(&self) -> Style {
        Style::default().fg(self.theme.text_secondary)
    }
}

impl<'a> Widget for InfoView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let inner_height = if self.block.is_some() {
            area.height.saturating_sub(2) as usize
        } else {
            area.height as usize
        };

        let all_lines = match self.prebuilt_lines {
            Some(lines) => lines,
            None => self.build_lines(),
        };

        let visible: Vec<Line<'static>> = all_lines
            .into_iter()
            .skip(self.scroll as usize)
            .take(inner_height)
            .collect();

        let paragraph = Paragraph::new(visible).wrap(Wrap { trim: false });

        let paragraph = if let Some(block) = self.block {
            paragraph.block(block)
        } else {
            paragraph
        };

        paragraph.render(area, buf);
    }
}

/// Info view state (reuses PreviewState for scrolling).
pub type InfoViewState = super::PreviewState;

/// Try to parse a GitHub hex color string (e.g. "d73a4a") into a ratatui Color.
fn parse_hex_color(hex: &str) -> Option<ratatui::style::Color> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(ratatui::style::Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::DiffInfo;
    use std::time::Instant;

    fn test_theme() -> Theme {
        Theme::basic()
    }

    fn empty_diff() -> DiffInfo {
        DiffInfo::empty()
    }

    fn sample_diff() -> DiffInfo {
        DiffInfo {
            diff: "+added\n-removed\n".to_string(),
            files_changed: 2,
            lines_added: 10,
            lines_removed: 5,
            line_count: 2,
            computed_at: Instant::now(),
            base_commit: String::new(),
        }
    }

    #[test]
    fn test_info_view_empty() {
        let theme = test_theme();
        let view = InfoView::new(InfoContent::Empty, &theme);
        let lines = view.build_lines();
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn test_info_view_session_no_pr() {
        let theme = test_theme();
        let diff = empty_diff();
        let data = InfoSessionData {
            title: "my-session".into(),
            branch: "feature-branch".into(),
            created_at: "2026-04-01 12:00 UTC".into(),
            status: SessionStatus::Running,
            program: "claude".into(),
            worktree_path: "/tmp/wt/my-session".into(),
            diff_info: &diff,
            pr_number: None,
            pr_url: None,
            pr_merged: false,
            enriched_pr: None,
            ai_summary: None,
            summary_key_hint: Some("g".into()),
        };
        let view = InfoView::new(InfoContent::Session(data), &theme);
        let lines = view.build_lines();
        assert!(lines.len() >= 9);
    }

    #[test]
    fn test_info_view_session_with_enriched_pr() {
        let theme = test_theme();
        let diff = sample_diff();
        let pr = EnrichedPrInfo {
            number: 42,
            url: "https://github.com/org/repo/pull/42".to_string(),
            title: "Add auth flow".to_string(),
            state: PrState::Open,
            is_draft: true,
            labels: vec![crate::git::PrLabel {
                name: "bug".to_string(),
                color: "d73a4a".to_string(),
            }],
            checks_status: ChecksStatus::Passing,
            body: "This PR adds auth.\nSecond line.".to_string(),
        };
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
            enriched_pr: Some(&pr),
            ai_summary: Some(&AiSummary::Ready {
                text: "This adds authentication.".to_string(),
                diff_hash: 123,
            }),
            summary_key_hint: Some("g".into()),
        };
        let view = InfoView::new(InfoContent::Session(data), &theme);
        let lines = view.build_lines();
        assert!(lines.len() > 15);
    }

    #[test]
    fn test_info_view_project() {
        let theme = test_theme();
        let data = InfoProjectData {
            name: "my-project".into(),
            repo_path: "/home/user/projects/my-project".into(),
            main_branch: "main".into(),
        };
        let view = InfoView::new(InfoContent::Project(data), &theme);
        let lines = view.build_lines();
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn test_info_view_ai_summary_loading() {
        let theme = test_theme();
        let diff = empty_diff();
        let data = InfoSessionData {
            title: "test".into(),
            branch: "test".into(),
            created_at: "now".into(),
            status: SessionStatus::Running,
            program: "claude".into(),
            worktree_path: "/tmp".into(),
            diff_info: &diff,
            pr_number: None,
            pr_url: None,
            pr_merged: false,
            enriched_pr: None,
            ai_summary: Some(&AiSummary::Loading),
            summary_key_hint: Some("g".into()),
        };
        let view = InfoView::new(InfoContent::Session(data), &theme);
        let lines = view.build_lines();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Generating..."));
    }

    #[test]
    fn test_info_view_ai_summary_error() {
        let theme = test_theme();
        let diff = empty_diff();
        let summary = AiSummary::Error("timed out".to_string());
        let data = InfoSessionData {
            title: "test".into(),
            branch: "test".into(),
            created_at: "now".into(),
            status: SessionStatus::Running,
            program: "claude".into(),
            worktree_path: "/tmp".into(),
            diff_info: &diff,
            pr_number: None,
            pr_url: None,
            pr_merged: false,
            enriched_pr: None,
            ai_summary: Some(&summary),
            summary_key_hint: Some("g".into()),
        };
        let view = InfoView::new(InfoContent::Session(data), &theme);
        let lines = view.build_lines();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("timed out"));
    }

    #[test]
    fn test_parse_hex_color_valid() {
        assert_eq!(
            parse_hex_color("d73a4a"),
            Some(ratatui::style::Color::Rgb(215, 58, 74))
        );
    }

    #[test]
    fn test_parse_hex_color_with_hash() {
        assert_eq!(
            parse_hex_color("#a2eeef"),
            Some(ratatui::style::Color::Rgb(162, 238, 239))
        );
    }

    #[test]
    fn test_parse_hex_color_invalid() {
        assert_eq!(parse_hex_color("xyz"), None);
        assert_eq!(parse_hex_color(""), None);
    }

    #[test]
    fn test_info_view_render_no_panic() {
        let theme = test_theme();
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let view = InfoView::new(InfoContent::Empty, &theme);
        view.render(area, &mut buf);
    }

    #[test]
    fn test_info_view_basic_pr_fallback() {
        let theme = test_theme();
        let diff = empty_diff();
        let data = InfoSessionData {
            title: "test".into(),
            branch: "test".into(),
            created_at: "now".into(),
            status: SessionStatus::Stopped,
            program: "claude".into(),
            worktree_path: "/tmp".into(),
            diff_info: &diff,
            pr_number: Some(99),
            pr_url: Some("https://github.com/org/repo/pull/99".into()),
            pr_merged: true,
            enriched_pr: None,
            ai_summary: None,
            summary_key_hint: Some("g".into()),
        };
        let view = InfoView::new(InfoContent::Session(data), &theme);
        let lines = view.build_lines();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("PR #99"));
        assert!(text.contains("(merged)"));
    }

    #[test]
    fn label_style_uses_accent_color_and_bold() {
        let theme = test_theme();
        let view = InfoView::new(InfoContent::Empty, &theme);
        let style = view.label_style();
        assert_eq!(style.fg, Some(theme.text_accent));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn value_style_uses_primary_text_color() {
        let theme = test_theme();
        let view = InfoView::new(InfoContent::Empty, &theme);
        let style = view.value_style();
        assert_eq!(style.fg, Some(theme.text_primary));
    }

    #[test]
    fn secondary_style_uses_secondary_text_color() {
        let theme = test_theme();
        let view = InfoView::new(InfoContent::Empty, &theme);
        let style = view.secondary_style();
        assert_eq!(style.fg, Some(theme.text_secondary));
    }
}
