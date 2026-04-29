//! Modal rendering: help, input, confirm, error, settings, quick-switch, checkout overlays.

use super::*;

impl App {
    pub(super) fn render_modal(&mut self, frame: &mut Frame, area: Rect) {
        match &self.ui_state.modal {
            Modal::None => {}

            Modal::Input {
                title,
                prompt,
                value,
                ..
            } => {
                // Fixed height: 2 borders + prompt line + blank + input = 5 rows.
                // Previous 20%-of-terminal rule wasted ~4 lines of vertical
                // space for a single-line input.
                let modal_width = (area.width * 60 / 100).max(40);
                let modal_height = 5u16;
                let modal_area = Rect {
                    x: area.x + (area.width.saturating_sub(modal_width)) / 2,
                    y: area.y + (area.height.saturating_sub(modal_height)) / 2,
                    width: modal_width,
                    height: modal_height.min(area.height),
                };
                frame.render_widget(Clear, modal_area);

                let block = Block::default()
                    .title(format!(" {} ", title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.modal_warning));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);

                let text = format!("{}\n\n❯ {}_", prompt, value);
                let paragraph = Paragraph::new(text);
                frame.render_widget(paragraph, inner);
            }

            Modal::PathInput {
                title,
                prompt,
                value,
                completer,
                scroll,
                ..
            } => {
                // Fixed height: 2 border + 3 input block + LIST_MAX_VISIBLE
                // rows + 1 hint = 16 when the full window fits, capped to
                // the terminal height. Keeps the modal size predictable so
                // navigation (which assumes LIST_MAX_VISIBLE) lines up with
                // the rendered window.
                let modal_width = (area.width * 60 / 100).max(50);
                let list_rows = super::actions::LIST_MAX_VISIBLE as u16;
                let modal_height: u16 = (2 + 3 + list_rows + 1).min(area.height.max(1));
                let modal_area = Rect {
                    x: area.x + (area.width.saturating_sub(modal_width)) / 2,
                    y: area.y + (area.height.saturating_sub(modal_height)) / 2,
                    width: modal_width,
                    height: modal_height,
                };
                frame.render_widget(Clear, modal_area);

                let block = Block::default()
                    .title(format!(" {} ", title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.modal_warning));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);

                // Split: prompt+input at top, completions below, hint at bottom
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3), // prompt + input
                        Constraint::Min(1),    // completions list
                        Constraint::Length(1), // hint line
                    ])
                    .split(inner);

                let input_text = format!("{}\n\n❯ {}_", prompt, value);
                let input_para = Paragraph::new(input_text);
                frame.render_widget(input_para, chunks[0]);

                // Render completions list with a scroll window so the
                // highlighted row stays on-screen even when the list is
                // longer than the visible area.
                let (completions, highlighted) = completer.visible_completions();
                if !completions.is_empty() {
                    let visible = chunks[1].height as usize;
                    let start = (*scroll).min(completions.len());
                    let lines: Vec<Line> = completions
                        .iter()
                        .enumerate()
                        .skip(start)
                        .take(visible)
                        .map(|(abs_idx, c)| {
                            // Show just the final path component for readability
                            let display = c.rsplit('/').next().unwrap_or(c);
                            if highlighted == Some(abs_idx) {
                                Line::from(Span::styled(
                                    format!("  ❯ {}", display),
                                    Style::default()
                                        .fg(self.theme.modal_info)
                                        .add_modifier(Modifier::BOLD),
                                ))
                            } else {
                                Line::from(format!("    {}", display))
                            }
                        })
                        .collect();
                    let completions_para = Paragraph::new(lines);
                    frame.render_widget(completions_para, chunks[1]);
                }

                let hint = Line::from(Span::styled(
                    "↑/↓ navigate  Tab complete  Enter submit  Esc cancel",
                    Style::default().add_modifier(Modifier::DIM),
                ));
                frame.render_widget(Paragraph::new(hint), chunks[2]);
            }

            Modal::Loading { title, message } => {
                let modal_area = centered_rect(60, 20, area);
                frame.render_widget(Clear, modal_area);

                let block = Block::default()
                    .title(format!(" {} ", title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.modal_info));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);

                const RAINBOW: &[ratatui::style::Color] = &[
                    ratatui::style::Color::Red,
                    ratatui::style::Color::Yellow,
                    ratatui::style::Color::Green,
                    ratatui::style::Color::Cyan,
                    ratatui::style::Color::Blue,
                    ratatui::style::Color::Magenta,
                ];
                let color = RAINBOW[self.ui_state.throbber_state.index() as usize % RAINBOW.len()];
                let throbber = throbber_widgets_tui::Throbber::default()
                    .throbber_set(throbber_widgets_tui::symbols::throbber::BRAILLE_EIGHT)
                    .label(message.as_str())
                    .throbber_style(Style::default().fg(color));
                frame.render_stateful_widget(throbber, inner, &mut self.ui_state.throbber_state);
            }

            Modal::Confirm { title, message, .. } => {
                let modal_area = centered_rect(50, 15, area);
                frame.render_widget(Clear, modal_area);

                let block = Block::default()
                    .title(format!(" {} ", title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.modal_error));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);

                let text = format!("{}\n\n[Enter] Confirm  [Esc] Cancel", message);
                let paragraph = Paragraph::new(text).wrap(Wrap { trim: true });
                frame.render_widget(paragraph, inner);
            }

            Modal::Error { message } => {
                let modal_area = centered_rect(60, 20, area);
                frame.render_widget(Clear, modal_area);

                let block = Block::default()
                    .title(" Error ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.modal_error));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);

                let text = format!("{}\n\nPress any key to close.", message);
                let paragraph = Paragraph::new(text).wrap(Wrap { trim: true });
                frame.render_widget(paragraph, inner);
            }

            Modal::Help { scroll } => {
                let mut offset = *scroll;
                self.render_help_modal(frame, area, &mut offset);
                if let Modal::Help { scroll } = &mut self.ui_state.modal {
                    *scroll = offset;
                }
            }

            Modal::Settings(state) => {
                self.render_settings_modal(frame, area, state);
            }

            Modal::QuickSwitch {
                mode,
                query,
                matches,
                selected_idx,
                scroll,
            } => {
                let max_visible = super::actions::LIST_MAX_VISIBLE;
                let visible_matches = matches.len().min(max_visible);
                // Dynamic height: border(2) + input(1) + matches
                let modal_height = (3 + visible_matches) as u16;
                let modal_width = (area.width * 60 / 100).max(40);

                // Position in upper third
                let modal_area = Rect {
                    x: area.x + (area.width.saturating_sub(modal_width)) / 2,
                    y: area.y + area.height / 5,
                    width: modal_width,
                    height: modal_height.min(area.height),
                };

                frame.render_widget(Clear, modal_area);

                // Switch the modal title by effective mode so a `>`-prefixed
                // query in unified mode reads as "Commands" while we type.
                let effective_mode = App::effective_palette_mode(*mode, query);
                let title = match effective_mode {
                    PaletteMode::Unified => " Quick Switch ",
                    PaletteMode::CommandOnly => " Commands ",
                    PaletteMode::SectionPicker { .. } => " Move to Section ",
                };
                let block = Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.modal_info));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);

                if inner.height == 0 {
                    return;
                }

                // Input line
                let input_line = Line::from(format!("❯ {}_", query));
                let input_area = Rect { height: 1, ..inner };
                frame.render_widget(Paragraph::new(input_line), input_area);

                // Match lines. The `scroll` offset lets us page through a
                // list longer than `max_visible`; rows below `scroll` are
                // off the top of the window, rows at/after
                // `scroll + max_visible` are off the bottom.
                let start = (*scroll).min(matches.len());
                for (i, item) in matches.iter().skip(start).take(max_visible).enumerate() {
                    let row = inner.y + 1 + i as u16;
                    if row >= inner.y + inner.height {
                        break;
                    }
                    let abs_idx = start + i;
                    let is_selected = abs_idx == *selected_idx;

                    let line_area = Rect {
                        y: row,
                        height: 1,
                        ..inner
                    };

                    match item {
                        QuickSwitchItem::Session(m) => {
                            let status_icon = match m.status {
                                SessionStatus::Creating
                                | SessionStatus::Merging
                                | SessionStatus::Pushing => "⠋",
                                SessionStatus::Running => "●",
                                SessionStatus::Stopped => "○",
                                SessionStatus::CascadePaused => "⏸",
                            };
                            let status_color = match m.status {
                                SessionStatus::Creating
                                | SessionStatus::Merging
                                | SessionStatus::Pushing => self.theme.status_creating,
                                SessionStatus::Running => self.theme.status_running,
                                SessionStatus::Stopped => self.theme.status_stopped,
                                SessionStatus::CascadePaused => self.theme.agent_waiting,
                            };

                            let mut spans = vec![
                                Span::styled(
                                    format!(" {} ", status_icon),
                                    Style::default().fg(status_color),
                                ),
                                Span::styled(
                                    m.title.clone(),
                                    if is_selected {
                                        self.theme.selection()
                                    } else {
                                        Style::default()
                                    },
                                ),
                            ];
                            if let Some(shown_branch) =
                                crate::session::display_branch(&m.title, &m.branch)
                            {
                                spans.push(Span::styled(
                                    format!(" [{}]", shown_branch),
                                    Style::default().fg(self.theme.text_accent),
                                ));
                            }
                            spans.push(Span::styled(
                                format!(" ({})", m.project_name),
                                Style::default().fg(self.theme.text_secondary),
                            ));
                            frame.render_widget(Paragraph::new(Line::from(spans)), line_area);
                        }
                        QuickSwitchItem::Command(entry) => {
                            // Full-row background distinguishes commands from
                            // sessions at a glance. Selection highlight takes
                            // precedence over the command background.
                            let row_style = if is_selected {
                                self.theme.selection()
                            } else {
                                Style::default()
                                    .bg(self.theme.palette_command_bg)
                                    .fg(self.theme.palette_command_fg)
                            };

                            // Reserve trailing space for the right-aligned
                            // key hint; keep one space margin on each side.
                            let available = line_area.width as usize;
                            let glyph = " ❯ ";
                            let keys = &entry.keys;
                            let keys_width = keys.chars().count();
                            let label = entry.label;
                            let label_width = label.chars().count();
                            let glyph_width = glyph.chars().count();
                            let padding = available
                                .saturating_sub(glyph_width)
                                .saturating_sub(label_width)
                                .saturating_sub(keys_width)
                                // Leave a 1-char gutter before the key hint
                                // when it's non-empty.
                                .saturating_sub(if keys.is_empty() { 0 } else { 1 });

                            let gutter = if keys.is_empty() {
                                String::new()
                            } else {
                                " ".to_string()
                            };
                            let content = format!(
                                "{glyph}{label}{pad}{gutter}{keys}",
                                glyph = glyph,
                                label = label,
                                pad = " ".repeat(padding),
                                gutter = gutter,
                                keys = keys,
                            );
                            let line = Line::from(Span::styled(content, row_style));
                            frame.render_widget(Paragraph::new(line).style(row_style), line_area);
                        }
                        QuickSwitchItem::SectionMove { label, .. } => {
                            let style = if is_selected {
                                self.theme.selection()
                            } else {
                                Style::default()
                            };
                            let line = Line::from(Span::styled(format!(" ❯ {label}"), style));
                            frame.render_widget(Paragraph::new(line).style(style), line_area);
                        }
                    }
                }
            }

            Modal::CheckoutBranch {
                query,
                filtered,
                selected_idx,
                scroll,
                fetching,
                ..
            } => {
                // Target up to 12 visible branch rows; grow to fill lower two
                // thirds of the screen if there are many, but cap at a
                // reasonable height.
                let desired_visible = filtered.len().clamp(1, 12);
                // border(2) + input(1) + hint(1) + rows
                let modal_height = (4 + desired_visible) as u16;
                let modal_width = (area.width * 70 / 100).max(50);

                let modal_area = Rect {
                    x: area.x + (area.width.saturating_sub(modal_width)) / 2,
                    y: area.y + area.height / 6,
                    width: modal_width,
                    height: modal_height.min(area.height),
                };

                frame.render_widget(Clear, modal_area);

                let title = if *fetching {
                    " Checkout Branch — fetching origin… "
                } else {
                    " Checkout Branch "
                };
                let block = Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.modal_info));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);

                if inner.height == 0 {
                    return;
                }

                // Input line
                let input_line = Line::from(format!("❯ {}_", query));
                let input_area = Rect { height: 1, ..inner };
                frame.render_widget(Paragraph::new(input_line), input_area);

                // Hint line
                let hint = if filtered.is_empty() {
                    if query.is_empty() {
                        "No branches found. Press Esc to cancel.".to_string()
                    } else {
                        format!(
                            "No match — press Enter to use '{}' as-is, or keep typing.",
                            query
                        )
                    }
                } else {
                    format!(
                        "{} match{} — ↑/↓ to select, Enter to checkout, Esc to cancel",
                        filtered.len(),
                        if filtered.len() == 1 { "" } else { "es" }
                    )
                };
                if inner.height >= 2 {
                    let hint_area = Rect {
                        y: inner.y + 1,
                        height: 1,
                        ..inner
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            hint,
                            Style::default().fg(self.theme.text_secondary),
                        ))),
                        hint_area,
                    );
                }

                // Match lines
                let list_top = inner.y + 2;
                if inner.height < 3 {
                    return;
                }
                let list_height = (inner.height - 2) as usize;
                let visible_end = (scroll + list_height).min(filtered.len());
                for (i, m) in filtered[*scroll..visible_end].iter().enumerate() {
                    let row = list_top + i as u16;
                    if row >= inner.y + inner.height {
                        break;
                    }
                    let abs_idx = *scroll + i;
                    let is_selected = abs_idx == *selected_idx;
                    let marker = if m.is_remote { "⟳ " } else { "● " };
                    let marker_color = if m.is_remote {
                        self.theme.text_secondary
                    } else {
                        self.theme.status_running
                    };

                    let spans = vec![
                        Span::styled(format!(" {}", marker), Style::default().fg(marker_color)),
                        Span::styled(
                            m.display_name.clone(),
                            if is_selected {
                                self.theme.selection()
                            } else {
                                Style::default()
                            },
                        ),
                    ];
                    let line = Line::from(spans);
                    let line_area = Rect {
                        y: row,
                        height: 1,
                        ..inner
                    };
                    frame.render_widget(Paragraph::new(line), line_area);
                }
            }
        }
    }

    pub(super) fn build_help_lines(&self) -> Vec<Line<'static>> {
        let kb = &self.config.keybindings;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let key_col_width = 18;

        for (section_name, actions) in kb.sections() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(format!("{section_name}:")));

            for (action, keys_str) in &actions {
                let desc = action.description();
                let padded_keys = format!("  {keys_str:<width$}{desc}", width = key_col_width);
                lines.push(Line::from(padded_keys));
            }
        }

        // Quick-switch (hardcoded since leader_key is in config, not keybindings)
        lines.push(Line::from(""));
        lines.push(Line::from("Quick Switch:"));
        let leader_display =
            if self.config.leader_key.trim().is_empty() || self.config.leader_key == " " {
                "Space".to_string()
            } else {
                self.config.leader_key.clone()
            };
        lines.push(Line::from(format!(
            "  {:<width$}Quick switch — sessions and commands",
            leader_display,
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}Command palette (commands only)",
            format!("Shift+{leader_display}"),
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}Filter palette to commands only",
            ">",
            width = key_col_width,
        )));

        // Status indicators (not keybinding-related, stays hardcoded)
        lines.push(Line::from(""));
        lines.push(Line::from("Status Indicators:"));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("●", Style::default().fg(self.theme.status_running)),
            Span::raw("  Running (agent active)"),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("○", Style::default().fg(self.theme.status_stopped)),
            Span::raw("  Stopped"),
        ]));

        // PR badges legend
        lines.push(Line::from(""));
        lines.push(Line::from("PR Badges:"));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("●", Style::default().fg(self.theme.pr_open)),
            Span::raw("  Open"),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("●", Style::default().fg(self.theme.status_pr)),
            Span::raw("  Open — awaiting review"),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("●", Style::default().fg(self.theme.pr_draft)),
            Span::raw("  Draft"),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("●", Style::default().fg(self.theme.pr_closed)),
            Span::raw("  Closed"),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("●", Style::default().fg(self.theme.status_pr_merged)),
            Span::raw("  Merged"),
        ]));

        lines.push(Line::from(""));
        lines.push(Line::from(
            "Esc/Enter/q/? to close · ↑/↓ k/j to scroll · PgUp/PgDn · Home/End",
        ));

        lines
    }

    pub(super) fn render_help_modal(&mut self, frame: &mut Frame, area: Rect, scroll: &mut u16) {
        let modal_area = centered_rect(70, 80, area);
        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .title(" Help ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.modal_info));
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let content_area = inner.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });

        let help_lines = self.build_help_lines();
        let total_lines = help_lines.len() as u16;
        let visible = content_area.height;
        let max_scroll = total_lines.saturating_sub(visible);

        if *scroll > max_scroll {
            *scroll = max_scroll;
        }
        let offset = *scroll;

        let paragraph = Paragraph::new(help_lines).scroll((offset, 0));
        frame.render_widget(paragraph, content_area);

        if max_scroll > 0 {
            // ratatui 0.29's Scrollbar treats `content_length - 1` as the
            // max scroll position (scrollbar.rs:562). Passing the full line
            // count leaves the thumb short of the bottom at max scroll —
            // use the number of distinct scroll positions instead so the
            // thumb hits the track ends at offset=0 and offset=max_scroll.
            let mut sb_state = ScrollbarState::new(max_scroll as usize + 1)
                .position(offset as usize)
                .viewport_content_length(visible as usize);
            let scrollbar = Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);
            frame.render_stateful_widget(scrollbar, content_area, &mut sb_state);
        }
    }
}

/// Helper to center a rect within an area
pub(super) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
