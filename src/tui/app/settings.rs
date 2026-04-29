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
                    SettingsRow {
                        label: "Sections".into(),
                        value: if c.sections.is_empty() {
                            "(none — see config.toml)".into()
                        } else {
                            format!("{} configured (edit config.toml)", c.sections.len())
                        },
                        field_key: "sections_readonly".into(),
                        color_swatch: None,
                    },
                ]
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

        // --- Settings rows ---
        let rows_area = Rect {
            y: content_area.y + 2,
            height: content_area.height.saturating_sub(4),
            ..content_area
        };

        let label_width = 24_u16;
        let value_width = rows_area.width.saturating_sub(label_width + 3);

        let visible_rows = rows_area.height as usize;
        let scroll_offset = if state.selected_row >= visible_rows {
            state.selected_row - visible_rows + 1
        } else {
            0
        };

        for (i, row) in state
            .rows
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_rows)
        {
            let y = rows_area.y + (i - scroll_offset) as u16;
            let is_selected = i == state.selected_row;

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

        // --- Footer ---
        let footer_area = Rect {
            y: content_area.y + content_area.height.saturating_sub(1),
            height: 1,
            ..content_area
        };
        let footer_text = if state.editing.is_some() {
            "Enter: save  Esc: cancel"
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

    /// Apply an edited value from the settings modal to the config.
    pub(super) fn apply_settings_edit(&mut self, tab: SettingsTab, field_key: &str, value: &str) {
        match tab {
            SettingsTab::General => match field_key {
                "default_program" => self.config.default_program = value.to_string(),
                "branch_prefix" => self.config.branch_prefix = value.to_string(),
                "shell_program" => self.config.shell_program = value.to_string(),
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
        }

        // Persist config via the store (updates mtime so hot-reload won't re-read our own write)
        let updated = self.config.clone();
        if let Err(e) = self.config_store.mutate(|c| *c = updated) {
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
                            let current_value = state.rows[state.selected_row].value.clone();
                            let initial = if current_value == "(auto)" || current_value == "(none)"
                            {
                                String::new()
                            } else {
                                current_value
                            };
                            state.editing = Some(SettingsEditing::TextInput { value: initial });
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
