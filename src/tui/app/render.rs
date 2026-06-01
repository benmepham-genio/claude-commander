//! Rendering: main layout, session list, preview/info/shell panes, status bar.

use super::*;

impl App {
    /// Return the border type based on config: rounded or plain (square).
    pub(super) fn border_type(&self) -> BorderType {
        if self.config.rounded_borders {
            BorderType::Rounded
        } else {
            BorderType::Plain
        }
    }

    /// Render the UI
    pub(super) fn render(&mut self, frame: &mut Frame) {
        let size = frame.area();
        self.ui_state.terminal_size = size;

        // Content area with margin on top, left, right, and space for status bar at bottom
        let content_area = Rect {
            x: size.x + 1,
            y: size.y + 1,
            width: size.width.saturating_sub(2),
            height: size.height.saturating_sub(3), // 1 top margin + 1 bottom margin + 1 status bar
        };

        // Main layout: session list on left, right pane fills rest
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(self.ui_state.left_pane_pct),
                Constraint::Percentage(100 - self.ui_state.left_pane_pct),
            ])
            .split(content_area);

        // Render session list
        self.render_session_list(frame, main_chunks[0]);

        // Render either preview, diff, or shell based on current view
        // Defensive: if a project is selected and view is Preview, render Shell instead
        let view = if self.is_project_selected()
            && self.ui_state.right_pane_view == RightPaneView::Preview
        {
            RightPaneView::Shell
        } else {
            self.ui_state.right_pane_view
        };
        match view {
            RightPaneView::Preview => self.render_preview(frame, main_chunks[1]),
            RightPaneView::Info => self.render_info(frame, main_chunks[1]),
            RightPaneView::Shell => self.render_shell(frame, main_chunks[1]),
        }

        // Render modal if open
        self.render_modal(frame, content_area);

        // Render status bar at the very bottom of the screen
        self.render_status_bar(frame, size);
    }

    /// Render the session list
    pub(super) fn render_session_list(&mut self, frame: &mut Frame, area: Rect) {
        // Split into a 1-line heading bar and the list below
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);

        // Full-width heading bar with dark grey background
        let heading_style = self.theme.status_bar();
        let heading =
            Paragraph::new(Line::styled(" Sessions:", heading_style)).style(heading_style);
        frame.render_widget(heading, chunks[0]);

        let blocked: std::collections::HashMap<ProjectId, &str> = self
            .ui_state
            .project_pull_blocked
            .iter()
            .map(|(id, r)| (*id, r.as_str()))
            .collect();

        let tree_list = TreeList::new(&self.ui_state.list_items, &self.theme)
            .tick(self.ui_state.tick_count)
            .highlight_style(self.theme.selection().add_modifier(Modifier::BOLD))
            .review_labels(&self.config.pr_review_labels)
            .invert_pr_label_color(self.config.invert_pr_label_color)
            .show_session_program(self.config.show_session_program)
            .pull_blocked_projects(blocked);

        frame.render_stateful_widget(
            tree_list,
            chunks[1],
            &mut self.ui_state.list_state.list_state,
        );
    }

    /// Build a styled tab title line for the pane header.
    ///
    /// `tabs` is the list of tab labels, `active` is the index of the currently
    /// selected tab. The active tab is rendered bold in the accent color; inactive
    /// tabs use the secondary text color. Tabs are separated by ` · `.
    pub(super) fn build_pane_tabs(&self, tabs: &[&str], active: usize) -> Line<'static> {
        let active_style = Style::default()
            .fg(self.theme.text_accent)
            .add_modifier(Modifier::BOLD);
        let inactive_style = Style::default().fg(self.theme.text_secondary);
        let sep_style = Style::default().fg(self.theme.text_secondary);

        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::raw(" "));
        for (i, tab) in tabs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", sep_style));
            }
            let style = if i == active {
                active_style
            } else {
                inactive_style
            };
            spans.push(Span::styled(tab.to_string(), style));
        }
        spans.push(Span::raw(" "));
        Line::from(spans)
    }

    /// Build a standard right-pane block with tabs, border styling, and focus state.
    fn pane_block(&self, tabs: &[&str], active_tab: usize) -> Block<'static> {
        let is_focused = matches!(self.ui_state.focused_pane, FocusedPane::RightPane);
        let title = self.build_pane_tabs(tabs, active_tab);
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(if is_focused {
                self.theme.border_focused()
            } else {
                self.theme.border_unfocused()
            })
    }

    /// Return `Some(opacity)` when the right pane is unfocused and dim is enabled,
    /// `None` otherwise. Used by preview and shell panes.
    fn pane_dim_opacity(&self) -> Option<f32> {
        let is_focused = matches!(self.ui_state.focused_pane, FocusedPane::RightPane);
        if !is_focused && self.config.dim_unfocused_preview {
            Some(self.config.dim_unfocused_opacity)
        } else {
            None
        }
    }

    /// Render the preview pane
    pub(super) fn render_preview(&mut self, frame: &mut Frame, area: Rect) {
        let dim_opacity = self.pane_dim_opacity();
        let block = self.pane_block(&["Preview", "Info", "Shell"], 0);

        // Update preview state with visible area
        let inner_height = area.height.saturating_sub(2);
        self.ui_state
            .preview_state
            .set_content(&self.ui_state.preview_content, inner_height);

        let preview = Preview::new(&self.ui_state.preview_content)
            .block(block)
            .scroll(self.ui_state.preview_state.scroll_offset)
            .dim_opacity(dim_opacity);

        frame.render_widget(preview, area);
    }

    /// Render the info pane (session metadata, PR details, AI summary)
    pub(super) fn render_info(&mut self, frame: &mut Frame, area: Rect) {
        let on_project = self.is_project_selected();

        // Compute display string for the generate-summary hotkey (None = AI disabled)
        let summary_key_hint = if self.config.ai_summary_enabled {
            self.config
                .keybindings
                .keys_for(BindableAction::GenerateSummary)
                .first()
                .map(|k| k.to_string())
        } else {
            None
        };

        let block = if on_project {
            self.pane_block(&["Shell", "Info"], 1)
        } else {
            self.pane_block(&["Preview", "Info", "Shell"], 1)
        };

        // Build the info content based on current selection
        let content = if let Some(session_id) = self.ui_state.selected_session_id {
            // Find the session data from list_items (includes all needed fields)
            let session_data = self.ui_state.list_items.iter().find_map(|item| {
                if let SessionListItem::Worktree {
                    id,
                    title,
                    branch,
                    status,
                    program,
                    pr_number,
                    pr_url,
                    pr_merged,
                    worktree_path,
                    created_at,
                    ..
                } = item
                {
                    if *id == session_id {
                        Some((
                            title.clone(),
                            branch.clone(),
                            *status,
                            program.clone(),
                            *pr_number,
                            pr_url.clone(),
                            *pr_merged,
                            worktree_path.display().to_string(),
                            created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            });

            if let Some((
                title,
                branch,
                status,
                program,
                pr_number,
                pr_url,
                pr_merged,
                worktree_path,
                created_at,
            )) = session_data
            {
                let enriched_pr = self
                    .ui_state
                    .enriched_pr
                    .as_ref()
                    .and_then(|(sid, pr)| if *sid == session_id { Some(pr) } else { None });

                let ai_summary = if self.config.ai_summary_enabled {
                    self.ui_state.ai_summaries.get(&session_id)
                } else {
                    None
                };

                InfoContent::Session(InfoSessionData {
                    title,
                    branch,
                    created_at,
                    status,
                    program,
                    worktree_path,
                    diff_info: &self.ui_state.diff_info,
                    pr_number,
                    pr_url,
                    pr_merged,
                    enriched_pr,
                    ai_summary,
                    summary_key_hint,
                    stack_chain: &self.ui_state.stack_chain,
                })
            } else {
                InfoContent::Empty
            }
        } else if let Some(project_id) = self.ui_state.selected_project_id {
            let project_data = self.ui_state.list_items.iter().find_map(|item| {
                if let SessionListItem::Project {
                    id,
                    name,
                    repo_path,
                    main_branch,
                    ..
                } = item
                {
                    if *id == project_id {
                        Some((
                            name.clone(),
                            repo_path.display().to_string(),
                            main_branch.clone(),
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            });

            if let Some((name, repo_path, main_branch)) = project_data {
                let pull_blocked = self
                    .ui_state
                    .project_pull_blocked
                    .get(&project_id)
                    .map(|r| r.as_str().to_string());
                InfoContent::Project(InfoProjectData {
                    name,
                    repo_path,
                    main_branch,
                    pull_blocked,
                })
            } else {
                InfoContent::Empty
            }
        } else {
            InfoContent::Empty
        };

        // Build lines once, use for both scroll metrics and rendering
        let info_view = InfoView::new(content, &self.theme);
        let lines = info_view.build_lines();
        let inner_height = area.height.saturating_sub(2);
        self.ui_state
            .info_state
            .set_metrics(lines.len(), inner_height);

        let info_view = info_view
            .with_prebuilt_lines(lines)
            .block(block)
            .scroll(self.ui_state.info_state.scroll_offset);

        frame.render_widget(info_view, area);
    }

    /// Render the shell pane
    pub(super) fn render_shell(&mut self, frame: &mut Frame, area: Rect) {
        let dim_opacity = self.pane_dim_opacity();

        let block = if self.is_project_selected() {
            self.pane_block(&["Shell", "Info"], 0)
        } else {
            self.pane_block(&["Preview", "Info", "Shell"], 2)
        };

        let inner_height = area.height.saturating_sub(2);
        self.ui_state
            .shell_state
            .set_content(&self.ui_state.shell_content, inner_height);

        let preview = Preview::new(&self.ui_state.shell_content)
            .block(block)
            .scroll(self.ui_state.shell_state.scroll_offset)
            .dim_opacity(dim_opacity);

        frame.render_widget(preview, area);
    }

    /// Render status bar
    pub(super) fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        if area.height < 2 {
            return;
        }

        let status_area = Rect {
            x: area.x,
            y: area.bottom().saturating_sub(1),
            width: area.width,
            height: 1,
        };

        let base_style = self.theme.status_bar();
        let sep = Span::styled(" \u{2502} ", base_style);

        // Fill the entire status bar background
        let bg_line = Line::from(vec![Span::styled(
            " ".repeat(status_area.width as usize),
            base_style,
        )]);
        frame.render_widget(Paragraph::new(bg_line), status_area);

        let toast = if let Some((ref msg, expires)) = self.ui_state.status_message {
            if Instant::now() < expires {
                Some(msg.clone())
            } else {
                None
            }
        } else {
            None
        };

        let restart_needed = self.service.config_store().restart_required();

        let session_count = self
            .ui_state
            .list_items
            .iter()
            .filter(|i| i.is_worktree())
            .count();

        let sessions_span = Span::styled(
            format!(" Sessions: {session_count}"),
            base_style.add_modifier(Modifier::BOLD),
        );

        let help_hint = Span::styled("? help ", base_style);

        // Build left-side spans and right-side help hint based on state
        let left_spans = if let Some(msg) = toast {
            let mut spans = vec![sessions_span, sep.clone(), Span::styled(msg, base_style)];
            if restart_needed {
                spans.push(sep);
                spans.push(Span::styled("Restart to apply config changes", base_style));
            }
            spans
        } else if restart_needed {
            vec![
                sessions_span,
                sep,
                Span::styled("Restart to apply config changes", base_style),
            ]
        } else {
            vec![
                sessions_span,
                sep.clone(),
                Span::styled("n", base_style.add_modifier(Modifier::BOLD)),
                Span::styled(": new session", base_style),
                sep,
                Span::styled("N", base_style.add_modifier(Modifier::BOLD)),
                Span::styled(": add project", base_style),
            ]
        };

        // Split the status area into left (fill) and right (fixed width for help hint)
        let help_width = 8u16; // "? help " + padding
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Fill(1), Constraint::Length(help_width)])
            .split(status_area);

        let left_line = Line::from(left_spans);
        frame.render_widget(Paragraph::new(left_line).style(base_style), chunks[0]);

        let right_line = Line::from(vec![help_hint]).alignment(Alignment::Right);
        frame.render_widget(Paragraph::new(right_line).style(base_style), chunks[1]);
    }
}
