//! Settings modal: row building, rendering, editing, and key handling.

use super::*;

impl App {
    pub(super) fn build_settings_rows(&self, tab: SettingsTab) -> Vec<SettingsRow> {
        match tab {
            SettingsTab::General => {
                let c = &self.config;
                vec![
                    SettingsRow {
                        label: "Default Program".into(),
                        value: c.default_program.clone(),
                        field_key: "default_program".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Branch Prefix".into(),
                        value: if c.branch_prefix.is_empty() {
                            "(none)".into()
                        } else {
                            c.branch_prefix.clone()
                        },
                        field_key: "branch_prefix".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Shell Program".into(),
                        value: c.shell_program.clone(),
                        field_key: "shell_program".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Worktrees Directory".into(),
                        value: c
                            .worktrees_dir
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "(default)".into()),
                        field_key: "worktrees_dir".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Per-Repo Worktree Dirs".into(),
                        value: c.per_repo_worktree_dirs.to_string(),
                        field_key: "per_repo_worktree_dirs".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Editor".into(),
                        value: c.editor.clone().unwrap_or_else(|| "(auto)".into()),
                        field_key: "editor".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Editor is GUI".into(),
                        value: match c.editor_gui {
                            Some(true) => "true".into(),
                            Some(false) => "false".into(),
                            None => "(auto)".into(),
                        },
                        field_key: "editor_gui".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Fetch Before Create".into(),
                        value: c.fetch_before_create.to_string(),
                        field_key: "fetch_before_create".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Resume Session".into(),
                        value: c.resume_session.to_string(),
                        field_key: "resume_session".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "UI Refresh FPS".into(),
                        value: c.ui_refresh_fps.to_string(),
                        field_key: "ui_refresh_fps".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "PR Check Interval (s)".into(),
                        value: c.pr_check_interval_secs.to_string(),
                        field_key: "pr_check_interval_secs".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Project Pull Enabled".into(),
                        value: c.project_pull_enabled.to_string(),
                        field_key: "project_pull_enabled".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Project Pull Interval (s)".into(),
                        value: c.project_pull_interval_secs.to_string(),
                        field_key: "project_pull_interval_secs".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Max Concurrent Tmux".into(),
                        value: c.max_concurrent_tmux.to_string(),
                        field_key: "max_concurrent_tmux".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Dim Unfocused Preview".into(),
                        value: c.dim_unfocused_preview.to_string(),
                        field_key: "dim_unfocused_preview".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Dim Opacity".into(),
                        value: format!("{:.2}", c.dim_unfocused_opacity),
                        field_key: "dim_unfocused_opacity".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Invert PR Label Color".into(),
                        value: c.invert_pr_label_color.to_string(),
                        field_key: "invert_pr_label_color".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Show Session Program".into(),
                        value: c.show_session_program.to_string(),
                        field_key: "show_session_program".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Rounded Borders".into(),
                        value: c.rounded_borders.to_string(),
                        field_key: "rounded_borders".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "Number Debounce (ms)".into(),
                        value: c.session_number_debounce_ms.to_string(),
                        field_key: "session_number_debounce_ms".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "AI Summary Enabled".into(),
                        value: c.ai_summary_enabled.to_string(),
                        field_key: "ai_summary_enabled".into(),
                        color_swatch: None,
                    },
                    SettingsRow {
                        label: "AI Summary Model".into(),
                        value: c.ai_summary_model.clone(),
                        field_key: "ai_summary_model".into(),
                        color_swatch: None,
                    },
                ]
            }
            SettingsTab::Sections => {
                vec![]
            }
            SettingsTab::Keybindings => {
                let kb = &self.config.keybindings;
                BindableAction::ALL
                    .iter()
                    .map(|&action| SettingsRow {
                        label: action.description().to_string(),
                        value: kb.keys_display(action),
                        field_key: action.config_name().to_string(),
                        color_swatch: None,
                    })
                    .collect()
            }
            SettingsTab::Theme => {
                // Show the current resolved color for each overridable field,
                // and whether it has a user override.
                let t = &self.theme;
                let o = &self.config.theme;

                macro_rules! theme_row {
                    ($label:expr, $field:ident) => {
                        SettingsRow {
                            label: $label.into(),
                            value: o
                                .$field
                                .map(|cv| {
                                    let s = toml::to_string(&cv).unwrap_or_default();
                                    s.trim().trim_matches('"').to_string()
                                })
                                .unwrap_or_else(|| format_color(t.$field)),
                            field_key: stringify!($field).into(),
                            color_swatch: Some(t.$field),
                        }
                    };
                }

                vec![
                    SettingsRow {
                        label: "Preset".into(),
                        value: o.preset.clone().unwrap_or_else(|| "(auto)".into()),
                        field_key: "preset".into(),
                        color_swatch: None,
                    },
                    theme_row!("Border Focused", border_focused),
                    theme_row!("Border Unfocused", border_unfocused),
                    theme_row!("Selection BG", selection_bg),
                    theme_row!("Status Running", status_running),
                    theme_row!("Status Stopped", status_stopped),
                    theme_row!("Status PR", status_pr),
                    theme_row!("Status PR Merged", status_pr_merged),
                    theme_row!("PR Open", pr_open),
                    theme_row!("PR Draft", pr_draft),
                    theme_row!("PR Closed", pr_closed),
                    theme_row!("Text Primary", text_primary),
                    theme_row!("Text Secondary", text_secondary),
                    theme_row!("Text Accent", text_accent),
                    theme_row!("Diff Added", diff_added),
                    theme_row!("Diff Removed", diff_removed),
                    theme_row!("Diff Hunk Header", diff_hunk_header),
                    theme_row!("Diff File Header", diff_file_header),
                    theme_row!("Modal Info", modal_info),
                    theme_row!("Modal Warning", modal_warning),
                    theme_row!("Modal Error", modal_error),
                    theme_row!("Status Bar BG", status_bar_bg),
                    theme_row!("Status Bar FG", status_bar_fg),
                ]
            }
        }
    }

    /// Render the settings modal.
    pub(super) fn render_settings_modal(
        &self,
        frame: &mut Frame,
        area: Rect,
        state: &SettingsState,
    ) {
        let modal_area = modals::centered_rect(75, 85, area);
        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .title(" Settings ")
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(Style::default().fg(self.theme.modal_info));
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let content_area = inner.inner(Margin {
            horizontal: 1,
            vertical: 0,
        });

        if content_area.height < 4 {
            return;
        }

        // --- Tab bar (row 0) ---
        let tab_area = Rect {
            height: 1,
            ..content_area
        };
        let mut tab_spans: Vec<Span> = Vec::new();
        for (i, tab) in SettingsTab::ALL.iter().enumerate() {
            if i > 0 {
                tab_spans.push(Span::raw("  "));
            }
            let style = if *tab == state.tab {
                Style::default()
                    .fg(self.theme.text_primary)
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default().fg(self.theme.text_secondary)
            };
            tab_spans.push(Span::styled(tab.label(), style));
        }
        frame.render_widget(Paragraph::new(Line::from(tab_spans)), tab_area);

        // --- Separator ---
        let sep_area = Rect {
            y: content_area.y + 1,
            height: 1,
            ..content_area
        };
        let separator = "─".repeat(content_area.width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                separator,
                Style::default().fg(self.theme.border_unfocused),
            ))),
            sep_area,
        );

        // --- Body area (between separator and footer) ---
        let body_area = Rect {
            y: content_area.y + 2,
            height: content_area.height.saturating_sub(4),
            ..content_area
        };

        // --- Footer ---
        let footer_area = Rect {
            y: content_area.y + content_area.height.saturating_sub(1),
            height: 1,
            ..content_area
        };

        if state.tab == SettingsTab::Sections {
            self.render_sections_tab(frame, body_area, footer_area, &state.sections_state);
        } else {
            self.render_settings_rows(frame, body_area, footer_area, state);
        }
    }

    fn render_settings_rows(
        &self,
        frame: &mut Frame,
        rows_area: Rect,
        footer_area: Rect,
        state: &SettingsState,
    ) {
        let label_width = 24_u16;
        let value_width = rows_area.width.saturating_sub(label_width + 3);

        let visible_rows = rows_area.height as usize;
        let scroll_offset = if state.selected_row >= visible_rows {
            state.selected_row - visible_rows + 1
        } else {
            0
        };

        // Check if the OptionPicker is active and how many rows it occupies
        let picker_info: Option<(usize, &[String], usize)> =
            if let Some(SettingsEditing::OptionPicker { options, selected }) = &state.editing {
                // screen_row is the row index (within visible area) where the picker starts
                let screen_row = state.selected_row.saturating_sub(scroll_offset);
                Some((screen_row, options.as_slice(), *selected))
            } else {
                None
            };

        // How many rows the picker will overlay (starting from the selected row)
        let picker_row_count = picker_info
            .map(|(screen_row, opts, _)| {
                let rows_below = visible_rows.saturating_sub(screen_row);
                opts.len().min(rows_below)
            })
            .unwrap_or(0);

        for (i, row) in state
            .rows
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_rows)
        {
            let screen_idx = i - scroll_offset;
            let y = rows_area.y + screen_idx as u16;
            let is_selected = i == state.selected_row;

            // If the OptionPicker is open, skip rendering normal rows that are
            // overlaid by picker options (except the first picker row itself,
            // which replaces the selected row).
            if let Some((picker_screen_row, _, _)) = picker_info
                && screen_idx > picker_screen_row
                && screen_idx < picker_screen_row + picker_row_count
            {
                continue;
            }

            let row_style = if is_selected {
                self.theme.selection()
            } else {
                Style::default()
            };

            // Label
            let label_area = Rect {
                x: rows_area.x,
                y,
                width: label_width.min(rows_area.width),
                height: 1,
            };
            let label = format!("{:<width$}", row.label, width = label_width as usize);
            frame.render_widget(Paragraph::new(Span::styled(label, row_style)), label_area);

            // Color swatch + Value
            if rows_area.width > label_width + 2 {
                let swatch_width: u16 = if row.color_swatch.is_some() { 3 } else { 0 };
                let val_x = rows_area.x + label_width + 2;

                // Render color swatch if present
                if let Some(swatch_color) = row.color_swatch {
                    let swatch_area = Rect {
                        x: val_x,
                        y,
                        width: swatch_width.min(value_width),
                        height: 1,
                    };
                    let swatch_style = if is_selected {
                        Style::default()
                            .fg(swatch_color)
                            .bg(self.theme.selection_bg)
                    } else {
                        Style::default().fg(swatch_color)
                    };
                    frame.render_widget(
                        Paragraph::new(Span::styled("██ ", swatch_style)),
                        swatch_area,
                    );
                }

                let val_area = Rect {
                    x: val_x + swatch_width,
                    y,
                    width: value_width.saturating_sub(swatch_width),
                    height: 1,
                };

                let display_val = if is_selected {
                    if let Some(SettingsEditing::TextInput { value }) = &state.editing {
                        format!("{value}▏")
                    } else if let Some(SettingsEditing::OptionPicker { options, selected }) =
                        &state.editing
                    {
                        // Show the currently highlighted option on the selected row
                        format!("▸ {}", options[*selected])
                    } else {
                        row.value.clone()
                    }
                } else {
                    row.value.clone()
                };

                let val_style = if is_selected && state.editing.is_some() {
                    row_style.add_modifier(Modifier::UNDERLINED)
                } else {
                    row_style.fg(self.theme.text_accent)
                };

                frame.render_widget(
                    Paragraph::new(Span::styled(display_val, val_style)),
                    val_area,
                );
            }
        }

        // Render the OptionPicker dropdown rows below the selected row
        if let Some((picker_screen_row, options, selected_opt)) = picker_info {
            let val_x = rows_area.x + label_width + 2;
            let val_w = value_width;

            for (opt_idx, option) in options.iter().enumerate().take(picker_row_count) {
                let row_y = rows_area.y + (picker_screen_row + opt_idx) as u16;
                let is_highlighted = opt_idx == selected_opt;

                // Clear the label area for overlay rows beyond the first
                if opt_idx > 0 {
                    let clear_area = Rect {
                        x: rows_area.x,
                        y: row_y,
                        width: label_width.min(rows_area.width),
                        height: 1,
                    };
                    frame.render_widget(Clear, clear_area);
                    frame.render_widget(Paragraph::new(Span::raw("")), clear_area);
                }

                let opt_area = Rect {
                    x: val_x,
                    y: row_y,
                    width: val_w,
                    height: 1,
                };

                // Clear before rendering
                frame.render_widget(Clear, opt_area);

                let prefix = if is_highlighted { "▸ " } else { "  " };
                let opt_style = if is_highlighted {
                    self.theme.selection()
                } else {
                    Style::default().fg(self.theme.text_accent)
                };

                frame.render_widget(
                    Paragraph::new(Span::styled(format!("{prefix}{option}"), opt_style)),
                    opt_area,
                );
            }
        }

        let footer_text = if state.editing.is_some() {
            match &state.editing {
                Some(SettingsEditing::OptionPicker { .. }) => {
                    "j/k: navigate  Enter: select  Esc: cancel"
                }
                _ => "Enter: save  Esc: cancel",
            }
        } else {
            "Tab: switch tab  j/k: navigate  Enter: edit  Esc: close"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer_text,
                Style::default().fg(self.theme.text_secondary),
            )),
            footer_area,
        );
    }

    fn render_sections_tab(
        &self,
        frame: &mut Frame,
        body_area: Rect,
        footer_area: Rect,
        sec: &SectionsState,
    ) {
        let sections = &self.config.sections;
        let list_width = body_area.width.clamp(16, 28);
        let divider_width = 1_u16;
        let pred_width = body_area
            .width
            .saturating_sub(list_width + divider_width + 1);

        let list_area = Rect {
            width: list_width,
            ..body_area
        };
        let divider_area = Rect {
            x: body_area.x + list_width,
            width: divider_width,
            ..body_area
        };
        let pred_area = Rect {
            x: body_area.x + list_width + divider_width + 1,
            width: pred_width,
            ..body_area
        };

        // --- Divider ---
        for row in 0..body_area.height {
            let y = divider_area.y + row;
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "│",
                    Style::default().fg(self.theme.border_unfocused),
                )),
                Rect {
                    x: divider_area.x,
                    y,
                    width: 1,
                    height: 1,
                },
            );
        }

        // --- Section list ---
        if let Some(SectionsEditing::CreatingSection { value }) = &sec.editing {
            let visible = list_area.height as usize;
            let name_rows = sections.len().min(visible.saturating_sub(1));
            for (i, section) in sections.iter().enumerate().take(name_rows) {
                let y = list_area.y + i as u16;
                let style = Style::default().fg(self.theme.text_secondary);
                let name = truncate_str(&section.name, list_width as usize - 2);
                frame.render_widget(
                    Paragraph::new(Span::styled(format!("  {name}"), style)),
                    Rect {
                        y,
                        height: 1,
                        ..list_area
                    },
                );
            }
            let input_y = list_area.y + name_rows as u16;
            let input_style = self.theme.selection().add_modifier(Modifier::UNDERLINED);
            let display = format!("  {value}▏");
            frame.render_widget(
                Paragraph::new(Span::styled(display, input_style)),
                Rect {
                    y: input_y,
                    height: 1,
                    ..list_area
                },
            );
        } else {
            let visible = list_area.height as usize;
            let scroll = if sec.selected_section >= visible {
                sec.selected_section - visible + 1
            } else {
                0
            };
            for (i, section) in sections.iter().enumerate().skip(scroll).take(visible) {
                let y = list_area.y + (i - scroll) as u16;
                let is_selected = i == sec.selected_section;
                let is_focused = sec.focus == SectionsFocus::List;

                let style = if is_selected && is_focused {
                    self.theme.selection()
                } else if is_selected {
                    Style::default()
                        .fg(self.theme.text_primary)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(self.theme.text_secondary)
                };

                let prefix = if is_selected { "▸ " } else { "  " };
                let name = if is_selected {
                    if let Some(SectionsEditing::RenamingSection { value }) = &sec.editing {
                        format!("{prefix}{value}▏")
                    } else {
                        let n = truncate_str(&section.name, list_width as usize - 2);
                        format!("{prefix}{n}")
                    }
                } else {
                    let n = truncate_str(&section.name, list_width as usize - 2);
                    format!("{prefix}{n}")
                };

                let row_style = if is_selected
                    && matches!(sec.editing, Some(SectionsEditing::RenamingSection { .. }))
                {
                    style.add_modifier(Modifier::UNDERLINED)
                } else {
                    style
                };

                frame.render_widget(
                    Paragraph::new(Span::styled(name, row_style)),
                    Rect {
                        y,
                        height: 1,
                        ..list_area
                    },
                );
            }

            if sections.is_empty() {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "  (no sections)",
                        Style::default().fg(self.theme.text_secondary),
                    )),
                    Rect {
                        height: 1,
                        ..list_area
                    },
                );
            }
        }

        // --- Predicate editor (right side) ---
        if !sections.is_empty() && sec.selected_section < sections.len() {
            let section = &sections[sec.selected_section];
            let pred_rows = predicate_rows(section);
            let is_pred_focused = sec.focus == SectionsFocus::Predicates;

            for (i, (label, value)) in pred_rows.iter().enumerate() {
                if i as u16 >= pred_area.height {
                    break;
                }
                let y = pred_area.y + i as u16;
                let is_selected = is_pred_focused && i == sec.pred_selected;

                let style = if is_selected {
                    self.theme.selection()
                } else {
                    Style::default()
                };

                let label_w = 18_u16.min(pred_area.width);
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!("{:<w$}", label, w = label_w as usize),
                        style,
                    )),
                    Rect {
                        x: pred_area.x,
                        y,
                        width: label_w,
                        height: 1,
                    },
                );

                if pred_area.width > label_w + 1 {
                    let val_x = pred_area.x + label_w + 1;
                    let val_w = pred_area.width.saturating_sub(label_w + 1);

                    let display_val = if is_selected {
                        if let Some(SectionsEditing::EditingPredicate { value: v }) = &sec.editing {
                            format!("{v}▏")
                        } else {
                            value.clone()
                        }
                    } else {
                        value.clone()
                    };

                    let val_style = if is_selected && sec.editing.is_some() {
                        style.add_modifier(Modifier::UNDERLINED)
                    } else {
                        style.fg(self.theme.text_accent)
                    };

                    frame.render_widget(
                        Paragraph::new(Span::styled(display_val, val_style)),
                        Rect {
                            x: val_x,
                            y,
                            width: val_w,
                            height: 1,
                        },
                    );
                }
            }
        }

        // --- Footer ---
        let footer_text = match &sec.editing {
            Some(
                SectionsEditing::RenamingSection { .. } | SectionsEditing::EditingPredicate { .. },
            ) => "Enter: save  Esc: cancel",
            Some(SectionsEditing::CreatingSection { .. }) => "Enter: create  Esc: cancel",
            None if sec.focus == SectionsFocus::List => {
                "n: new  r: rename  d: delete  J/K: reorder  →/Enter: predicates  Tab: switch tab"
            }
            None => "Enter: edit  ←: back to list  Tab: switch tab",
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer_text,
                Style::default().fg(self.theme.text_secondary),
            )),
            footer_area,
        );
    }

    /// Apply an edited value from the settings modal to the config.
    pub(super) fn apply_settings_edit(&mut self, tab: SettingsTab, field_key: &str, value: &str) {
        match tab {
            SettingsTab::General => match field_key {
                "default_program" => self.config.default_program = value.to_string(),
                "branch_prefix" => self.config.branch_prefix = value.to_string(),
                "shell_program" => self.config.shell_program = value.to_string(),
                "worktrees_dir" => {
                    self.config.worktrees_dir = if value.is_empty() || value == "(default)" {
                        None
                    } else {
                        Some(PathBuf::from(value))
                    };
                }
                "per_repo_worktree_dirs" => {
                    if let Ok(b) = value.parse::<bool>() {
                        self.config.per_repo_worktree_dirs = b;
                    }
                }
                "editor" => {
                    self.config.editor = if value.is_empty() || value == "(auto)" {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                "editor_gui" => {
                    self.config.editor_gui = match value {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    };
                }
                "fetch_before_create" => {
                    if let Ok(b) = value.parse::<bool>() {
                        self.config.fetch_before_create = b;
                    }
                }
                "resume_session" => {
                    if let Ok(b) = value.parse::<bool>() {
                        self.config.resume_session = b;
                    }
                }
                "ui_refresh_fps" => {
                    if let Ok(v) = value.parse::<u32>() {
                        self.config.ui_refresh_fps = v;
                    }
                }
                "pr_check_interval_secs" => {
                    if let Ok(v) = value.parse::<u64>() {
                        self.config.pr_check_interval_secs = v;
                    }
                }
                "project_pull_enabled" => {
                    if let Ok(b) = value.parse::<bool>() {
                        self.config.project_pull_enabled = b;
                    }
                }
                "project_pull_interval_secs" => match value.parse::<u64>() {
                    Ok(v) if v >= 60 => {
                        self.config.project_pull_interval_secs = v;
                    }
                    Ok(_) => {
                        self.ui_state.status_message = Some((
                            "Project Pull Interval must be at least 60 seconds".into(),
                            std::time::Instant::now() + std::time::Duration::from_secs(4),
                        ));
                    }
                    Err(_) => {}
                },
                "max_concurrent_tmux" => {
                    if let Ok(v) = value.parse::<usize>() {
                        self.config.max_concurrent_tmux = v;
                    }
                }
                "dim_unfocused_preview" => {
                    if let Ok(b) = value.parse::<bool>() {
                        self.config.dim_unfocused_preview = b;
                    }
                }
                "dim_unfocused_opacity" => {
                    if let Ok(v) = value.parse::<f32>() {
                        self.config.dim_unfocused_opacity = v.clamp(0.0, 1.0);
                    }
                }
                "invert_pr_label_color" => {
                    if let Ok(b) = value.parse::<bool>() {
                        self.config.invert_pr_label_color = b;
                    }
                }
                "show_session_program" => {
                    if let Ok(b) = value.parse::<bool>() {
                        self.config.show_session_program = b;
                    }
                }
                "rounded_borders" => {
                    if let Ok(b) = value.parse::<bool>() {
                        self.config.rounded_borders = b;
                    }
                }
                "session_number_debounce_ms" => {
                    if let Ok(v) = value.parse::<u64>() {
                        self.config.session_number_debounce_ms = v;
                    }
                }
                "ai_summary_enabled" => {
                    if let Ok(b) = value.parse::<bool>() {
                        self.config.ai_summary_enabled = b;
                    }
                }
                "ai_summary_model" => {
                    self.config.ai_summary_model = value.to_string();
                }
                _ => {}
            },
            SettingsTab::Theme => {
                use crate::config::theme::ColorValue;

                if field_key == "preset" {
                    self.config.theme.preset = if value.is_empty() || value == "(auto)" {
                        None
                    } else {
                        Some(value.to_string())
                    };
                } else {
                    // Try to parse the value as a ColorValue via TOML
                    let toml_input = if value.starts_with('#')
                        || value.chars().all(|c| c.is_ascii_alphabetic() || c == '_')
                    {
                        format!("c = \"{value}\"")
                    } else {
                        format!("c = {value}")
                    };

                    #[derive(serde::Deserialize)]
                    struct Wrap {
                        c: ColorValue,
                    }

                    if let Ok(w) = toml::from_str::<Wrap>(&toml_input) {
                        macro_rules! set_theme_field {
                            ($($name:ident),*) => {
                                match field_key {
                                    $(stringify!($name) => self.config.theme.$name = Some(w.c),)*
                                    _ => {}
                                }
                            };
                        }
                        set_theme_field!(
                            border_focused,
                            border_unfocused,
                            selection_bg,
                            selection_fg,
                            status_running,
                            status_stopped,
                            status_pr,
                            status_pr_merged,
                            pr_open,
                            pr_draft,
                            pr_closed,
                            text_primary,
                            text_secondary,
                            text_accent,
                            diff_added,
                            diff_removed,
                            diff_hunk_header,
                            diff_file_header,
                            diff_context,
                            modal_info,
                            modal_warning,
                            modal_error,
                            status_bar_bg,
                            status_bar_fg
                        );
                    }
                }

                // Rebuild theme from updated overrides
                let base = self
                    .config
                    .theme
                    .preset
                    .as_deref()
                    .and_then(Theme::from_preset)
                    .unwrap_or_default();
                self.theme = base.with_overrides(&self.config.theme);
            }
            SettingsTab::Keybindings => {
                use crate::config::keybindings::{BindableAction, KeyBinding};
                use std::str::FromStr;

                let Ok(action) = BindableAction::from_str(field_key) else {
                    warn!("Unknown keybinding action: {}", field_key);
                    return;
                };

                // The row value is rendered as a comma-separated list
                // (e.g. `"k, Up, Ctrl-p"`). Parse each entry back into a
                // `KeyBinding`, ignoring empty tokens. If every token fails
                // to parse we leave the binding alone rather than silently
                // clear it.
                let mut parsed: Vec<KeyBinding> = Vec::new();
                let mut had_token = false;
                let mut any_err = false;
                for token in value.split(',') {
                    let t = token.trim();
                    if t.is_empty() {
                        continue;
                    }
                    had_token = true;
                    match KeyBinding::from_str(t) {
                        Ok(kb) => parsed.push(kb),
                        Err(e) => {
                            warn!("Invalid keybinding '{}': {}", t, e);
                            any_err = true;
                        }
                    }
                }

                if had_token && parsed.is_empty() && any_err {
                    // User tried to edit but every token was malformed —
                    // show the error but don't wipe their existing binding.
                    self.ui_state.modal = Modal::Error {
                        message: format!(
                            "Could not parse any key bindings from '{}'. \
                             Use e.g. 'k', 'Ctrl-p', 'Shift-N', 'Enter'.",
                            value
                        ),
                    };
                    return;
                }

                self.config.keybindings.set_keys_for(action, parsed);
            }
            SettingsTab::Sections => {
                // Sections tab handles its own persistence via save_sections_config
                return;
            }
        }

        // Persist config via the store (updates mtime so hot-reload won't re-read our own write)
        let updated = self.config.clone();
        if let Err(e) = self.service.update_config(updated) {
            warn!("Failed to save config: {}", e);
        }
    }

    /// Handle a keypress in the settings modal.
    pub(super) async fn handle_settings_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        mut state: SettingsState,
    ) {
        use crossterm::event::KeyCode;

        if state.tab == SettingsTab::Sections {
            self.handle_sections_key(key, state).await;
            return;
        }

        if let Some(ref mut editing) = state.editing {
            // Currently editing a field
            match editing {
                SettingsEditing::TextInput { value } => match key.code {
                    KeyCode::Enter => {
                        let val = value.clone();
                        let field_key = state.rows[state.selected_row].field_key.clone();
                        state.editing = None;
                        self.apply_settings_edit(state.tab, &field_key, &val);
                        // Refresh rows after applying
                        state.rows = self.build_settings_rows(state.tab);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Esc => {
                        state.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Backspace => {
                        value.pop();
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Char(c) => {
                        value.push(c);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
                SettingsEditing::KeyCapture { .. } => {
                    // For key capture, any keypress except Esc is captured as the new binding
                    match key.code {
                        KeyCode::Esc => {
                            state.editing = None;
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        _ => {
                            // Key capture is a simplified version — store the key display
                            // Full keybinding editing would require more complex UX
                            state.editing = None;
                            self.ui_state.modal = Modal::Settings(state);
                        }
                    }
                }
                SettingsEditing::OptionPicker { options, selected } => match key.code {
                    KeyCode::Enter => {
                        let chosen = options[*selected].clone();
                        let field_key = state.rows[state.selected_row].field_key.clone();
                        // Treat "(auto)" as empty string for apply_settings_edit
                        let val = if chosen == "(auto)" {
                            String::new()
                        } else {
                            chosen
                        };
                        state.editing = None;
                        self.apply_settings_edit(state.tab, &field_key, &val);
                        state.rows = self.build_settings_rows(state.tab);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Esc => {
                        state.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *selected = (*selected + 1) % options.len();
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = if *selected == 0 {
                            options.len() - 1
                        } else {
                            *selected - 1
                        };
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
            }
        } else {
            // Not editing — navigation mode: resolve via configurable keybindings
            use crate::config::keybindings::BindableAction;

            match self.config.keybindings.resolve(&key) {
                Some(BindableAction::NavigateDown) => {
                    if !state.rows.is_empty() {
                        state.selected_row = (state.selected_row + 1) % state.rows.len();
                    }
                    self.ui_state.modal = Modal::Settings(state);
                }
                Some(BindableAction::NavigateUp) => {
                    if !state.rows.is_empty() {
                        state.selected_row = if state.selected_row == 0 {
                            state.rows.len() - 1
                        } else {
                            state.selected_row - 1
                        };
                    }
                    self.ui_state.modal = Modal::Settings(state);
                }
                Some(BindableAction::Quit) => {
                    self.ui_state.modal = Modal::None;
                }
                _ => match key.code {
                    KeyCode::Esc => {
                        self.ui_state.modal = Modal::None;
                    }
                    KeyCode::Tab => {
                        state.tab = state.tab.next();
                        state.selected_row = 0;
                        state.rows = self.build_settings_rows(state.tab);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::BackTab => {
                        state.tab = state.tab.prev();
                        state.selected_row = 0;
                        state.rows = self.build_settings_rows(state.tab);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Enter => {
                        if !state.rows.is_empty() {
                            let field_key = &state.rows[state.selected_row].field_key;
                            if state.tab == SettingsTab::Theme && field_key == "preset" {
                                // Open an inline option picker for theme presets
                                use crate::tui::theme::PRESET_NAMES;
                                let options: Vec<String> =
                                    PRESET_NAMES.iter().map(|s| (*s).to_string()).collect();
                                let current_value = &state.rows[state.selected_row].value;
                                let selected =
                                    options.iter().position(|o| o == current_value).unwrap_or(0);
                                state.editing =
                                    Some(SettingsEditing::OptionPicker { options, selected });
                            } else {
                                let current_value = state.rows[state.selected_row].value.clone();
                                let initial =
                                    if current_value == "(auto)" || current_value == "(none)" {
                                        String::new()
                                    } else {
                                        current_value
                                    };
                                state.editing = Some(SettingsEditing::TextInput { value: initial });
                            }
                        }
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
            }
        }
    }

    /// Handle a keypress while the Sections tab is active.
    async fn handle_sections_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        mut state: SettingsState,
    ) {
        use crossterm::event::KeyCode;

        let sec = &mut state.sections_state;

        // --- Editing mode ---
        if let Some(ref mut editing) = sec.editing {
            match editing {
                SectionsEditing::RenamingSection { value } => match key.code {
                    KeyCode::Enter => {
                        let new_name = value.trim().to_string();
                        if !new_name.is_empty() && sec.selected_section < self.config.sections.len()
                        {
                            let has_dup = self
                                .config
                                .sections
                                .iter()
                                .enumerate()
                                .any(|(i, s)| i != sec.selected_section && s.name == new_name);
                            if !has_dup {
                                self.config.sections[sec.selected_section].name = new_name;
                                self.save_sections_config().await;
                            }
                        }
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Esc => {
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Backspace => {
                        value.pop();
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Char(c) => {
                        value.push(c);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
                SectionsEditing::EditingPredicate { value } => match key.code {
                    KeyCode::Enter => {
                        let val = value.clone();
                        let pred_idx = sec.pred_selected;
                        if sec.selected_section < self.config.sections.len() {
                            apply_predicate_edit(
                                &mut self.config.sections[sec.selected_section],
                                pred_idx,
                                &val,
                            );
                            self.save_sections_config().await;
                        }
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Esc => {
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Backspace => {
                        value.pop();
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Char(c) => {
                        value.push(c);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
                SectionsEditing::CreatingSection { value } => match key.code {
                    KeyCode::Enter => {
                        let new_name = value.trim().to_string();
                        if !new_name.is_empty() {
                            let has_dup = self.config.sections.iter().any(|s| s.name == new_name);
                            if !has_dup {
                                self.config.sections.push(crate::session::SectionConfig {
                                    name: new_name,
                                    ..Default::default()
                                });
                                sec.selected_section = self.config.sections.len() - 1;
                                self.save_sections_config().await;
                            }
                        }
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Esc => {
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Backspace => {
                        value.pop();
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Char(c) => {
                        value.push(c);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
            }
            return;
        }

        // --- Navigation mode ---
        let sections_len = self.config.sections.len();

        match sec.focus {
            SectionsFocus::List => {
                use crate::config::keybindings::BindableAction;

                match self.config.keybindings.resolve(&key) {
                    Some(BindableAction::NavigateDown) => {
                        if sections_len > 0 {
                            sec.selected_section = (sec.selected_section + 1) % sections_len;
                            sec.pred_selected = 0;
                        }
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    Some(BindableAction::NavigateUp) => {
                        if sections_len > 0 {
                            sec.selected_section = if sec.selected_section == 0 {
                                sections_len - 1
                            } else {
                                sec.selected_section - 1
                            };
                            sec.pred_selected = 0;
                        }
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    Some(BindableAction::Quit) => {
                        self.ui_state.modal = Modal::None;
                    }
                    _ => match key.code {
                        KeyCode::Esc => {
                            self.ui_state.modal = Modal::None;
                        }
                        KeyCode::Tab => {
                            state.tab = state.tab.next();
                            state.selected_row = 0;
                            state.rows = self.build_settings_rows(state.tab);
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::BackTab => {
                            state.tab = state.tab.prev();
                            state.selected_row = 0;
                            state.rows = self.build_settings_rows(state.tab);
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Right | KeyCode::Enter => {
                            if sections_len > 0 {
                                sec.focus = SectionsFocus::Predicates;
                                sec.pred_selected = 0;
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Char('n') => {
                            sec.editing = Some(SectionsEditing::CreatingSection {
                                value: String::new(),
                            });
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Char('r') => {
                            if sec.selected_section < sections_len {
                                let current =
                                    self.config.sections[sec.selected_section].name.clone();
                                sec.editing =
                                    Some(SectionsEditing::RenamingSection { value: current });
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Char('d') => {
                            if sec.selected_section < sections_len {
                                self.config.sections.remove(sec.selected_section);
                                if sec.selected_section >= self.config.sections.len()
                                    && sec.selected_section > 0
                                {
                                    sec.selected_section -= 1;
                                }
                                self.save_sections_config().await;
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Char('J') => {
                            if sec.selected_section + 1 < sections_len {
                                self.config
                                    .sections
                                    .swap(sec.selected_section, sec.selected_section + 1);
                                sec.selected_section += 1;
                                self.save_sections_config().await;
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Char('K') => {
                            if sec.selected_section > 0 && sections_len > 0 {
                                self.config
                                    .sections
                                    .swap(sec.selected_section, sec.selected_section - 1);
                                sec.selected_section -= 1;
                                self.save_sections_config().await;
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        _ => {
                            self.ui_state.modal = Modal::Settings(state);
                        }
                    },
                }
            }
            SectionsFocus::Predicates => {
                use crate::config::keybindings::BindableAction;

                let pred_count = if sec.selected_section < sections_len {
                    predicate_rows(&self.config.sections[sec.selected_section]).len()
                } else {
                    0
                };

                match self.config.keybindings.resolve(&key) {
                    Some(BindableAction::NavigateDown) => {
                        if pred_count > 0 {
                            sec.pred_selected = (sec.pred_selected + 1) % pred_count;
                        }
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    Some(BindableAction::NavigateUp) => {
                        if pred_count > 0 {
                            sec.pred_selected = if sec.pred_selected == 0 {
                                pred_count - 1
                            } else {
                                sec.pred_selected - 1
                            };
                        }
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    Some(BindableAction::Quit) => {
                        self.ui_state.modal = Modal::None;
                    }
                    _ => match key.code {
                        KeyCode::Esc | KeyCode::Left => {
                            sec.focus = SectionsFocus::List;
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Tab => {
                            state.tab = state.tab.next();
                            state.selected_row = 0;
                            state.rows = self.build_settings_rows(state.tab);
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::BackTab => {
                            state.tab = state.tab.prev();
                            state.selected_row = 0;
                            state.rows = self.build_settings_rows(state.tab);
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Enter => {
                            if sec.selected_section < sections_len && pred_count > 0 {
                                let rows =
                                    predicate_rows(&self.config.sections[sec.selected_section]);
                                let (_, current_val) = &rows[sec.pred_selected];
                                let initial = if current_val == "(not set)" {
                                    String::new()
                                } else {
                                    current_val.clone()
                                };
                                sec.editing =
                                    Some(SectionsEditing::EditingPredicate { value: initial });
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        _ => {
                            self.ui_state.modal = Modal::Settings(state);
                        }
                    },
                }
            }
        }
    }

    /// Persist the current sections config to disk and reconcile session assignments.
    async fn save_sections_config(&mut self) {
        let updated = self.config.clone();
        if let Err(e) = self.service.update_config(updated) {
            warn!("Failed to save sections config: {}", e);
        }
        self.reconcile_section_assignments().await;
    }
}

/// Build displayable rows for a section's predicates.
fn predicate_rows(section: &crate::session::SectionConfig) -> Vec<(String, String)> {
    use crate::session::section::{
        DecisionPredicate, LabelPredicate, ReviewerPredicate, StatePredicate,
    };

    let fmt_state = |p: &StatePredicate| match p {
        crate::session::section::OneOrMany::One(v) => format!("{v:?}").to_lowercase(),
        crate::session::section::OneOrMany::Any(vs) => vs
            .iter()
            .map(|v| format!("{v:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", "),
    };

    let fmt_decision = |p: &DecisionPredicate| match p {
        crate::session::section::OneOrMany::One(v) => format!("{v:?}").to_lowercase(),
        crate::session::section::OneOrMany::Any(vs) => vs
            .iter()
            .map(|v| format!("{v:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", "),
    };

    let fmt_label = |p: &LabelPredicate| match p {
        LabelPredicate::One(s) => s.clone(),
        LabelPredicate::Any(vs) => vs.join(", "),
    };

    let fmt_reviewer = |p: &ReviewerPredicate| match p {
        ReviewerPredicate::Bool(b) => b.to_string(),
        ReviewerPredicate::One(s) => s.clone(),
        ReviewerPredicate::Any(vs) => vs.join(", "),
    };

    let not_set = "(not set)".to_string();
    vec![
        (
            "pr_state".into(),
            section
                .pr_state
                .as_ref()
                .map_or_else(|| not_set.clone(), fmt_state),
        ),
        (
            "is_draft".into(),
            section
                .is_draft
                .map_or_else(|| not_set.clone(), |b| b.to_string()),
        ),
        (
            "has_label".into(),
            section
                .has_label
                .as_ref()
                .map_or_else(|| not_set.clone(), fmt_label),
        ),
        (
            "has_pr".into(),
            section
                .has_pr
                .map_or_else(|| not_set.clone(), |b| b.to_string()),
        ),
        (
            "review_decision".into(),
            section
                .review_decision
                .as_ref()
                .map_or_else(|| not_set.clone(), fmt_decision),
        ),
        (
            "has_reviewer".into(),
            section
                .has_reviewer
                .as_ref()
                .map_or_else(|| not_set.clone(), fmt_reviewer),
        ),
    ]
}

/// Apply a user-edited predicate value string to a SectionConfig field.
fn apply_predicate_edit(section: &mut crate::session::SectionConfig, pred_idx: usize, value: &str) {
    use crate::git::{PrState, ReviewDecision};
    use crate::session::section::{LabelPredicate, OneOrMany, ReviewerPredicate};

    let trimmed = value.trim();

    match pred_idx {
        // pr_state
        0 => {
            if trimmed.is_empty() {
                section.pr_state = None;
            } else {
                let parts: Vec<&str> = trimmed.split(',').map(str::trim).collect();
                let parsed: Vec<PrState> = parts.iter().filter_map(|s| parse_pr_state(s)).collect();
                section.pr_state = match parsed.len() {
                    0 => None,
                    1 => Some(OneOrMany::One(parsed[0])),
                    _ => Some(OneOrMany::Any(parsed)),
                };
            }
        }
        // is_draft
        1 => {
            section.is_draft = match trimmed {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        }
        // has_label
        2 => {
            if trimmed.is_empty() {
                section.has_label = None;
            } else {
                let labels: Vec<String> = trimmed
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                section.has_label = match labels.len() {
                    0 => None,
                    1 => Some(LabelPredicate::One(labels.into_iter().next().unwrap())),
                    _ => Some(LabelPredicate::Any(labels)),
                };
            }
        }
        // has_pr
        3 => {
            section.has_pr = match trimmed {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        }
        // review_decision
        4 => {
            if trimmed.is_empty() {
                section.review_decision = None;
            } else {
                let parts: Vec<&str> = trimmed.split(',').map(str::trim).collect();
                let parsed: Vec<ReviewDecision> = parts
                    .iter()
                    .filter_map(|s| parse_review_decision(s))
                    .collect();
                section.review_decision = match parsed.len() {
                    0 => None,
                    1 => Some(OneOrMany::One(parsed[0])),
                    _ => Some(OneOrMany::Any(parsed)),
                };
            }
        }
        // has_reviewer
        5 => {
            if trimmed.is_empty() {
                section.has_reviewer = None;
            } else {
                match trimmed {
                    "true" => section.has_reviewer = Some(ReviewerPredicate::Bool(true)),
                    "false" => section.has_reviewer = Some(ReviewerPredicate::Bool(false)),
                    _ => {
                        let logins: Vec<String> = trimmed
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        section.has_reviewer = match logins.len() {
                            0 => None,
                            1 => Some(ReviewerPredicate::One(logins.into_iter().next().unwrap())),
                            _ => Some(ReviewerPredicate::Any(logins)),
                        };
                    }
                }
            }
        }
        _ => {}
    }
}

fn parse_pr_state(s: &str) -> Option<crate::git::PrState> {
    match s.to_lowercase().as_str() {
        "open" => Some(crate::git::PrState::Open),
        "merged" => Some(crate::git::PrState::Merged),
        "closed" => Some(crate::git::PrState::Closed),
        _ => None,
    }
}

fn parse_review_decision(s: &str) -> Option<crate::git::ReviewDecision> {
    match s.to_lowercase().replace(['-', '_'], "").as_str() {
        "approved" => Some(crate::git::ReviewDecision::Approved),
        "changesrequested" => Some(crate::git::ReviewDecision::ChangesRequested),
        "reviewrequired" => Some(crate::git::ReviewDecision::ReviewRequired),
        _ => None,
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max > 1 {
        format!("{}…", &s[..max - 1])
    } else {
        "…".to_string()
    }
}

/// Format a ratatui Color for display in the settings modal.
fn format_color(color: ratatui::style::Color) -> String {
    use ratatui::style::Color;
    match color {
        Color::Reset => "reset".into(),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "gray".into(),
        Color::DarkGray => "dark_gray".into(),
        Color::LightRed => "light_red".into(),
        Color::LightGreen => "light_green".into(),
        Color::LightYellow => "light_yellow".into(),
        Color::LightBlue => "light_blue".into(),
        Color::LightMagenta => "light_magenta".into(),
        Color::LightCyan => "light_cyan".into(),
        Color::White => "white".into(),
        Color::Indexed(i) => format!("{i}"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}
