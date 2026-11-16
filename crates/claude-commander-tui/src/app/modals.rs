//! Modal rendering: help, input, confirm, error, settings, quick-switch, checkout overlays.

use super::*;

/// Place the real terminal cursor for a single-line text input whose visible
/// text starts at `(text_x, text_y)` and spans `text_width` columns. Uses
/// tui-input's wide-char-aware `visual_cursor`, clamped within the row so a long
/// value can't push the cursor past the field's edge. Preferred over splicing a
/// caret glyph into the text: the glyph reads as a stray blank when the cursor
/// sits mid-string.
fn place_input_cursor(
    frame: &mut Frame,
    input: &tui_input::Input,
    text_x: u16,
    text_y: u16,
    text_width: u16,
) {
    let col = input.visual_cursor() as u16;
    let max_x = text_x + text_width.saturating_sub(1);
    frame.set_cursor_position(((text_x + col).min(max_x), text_y));
}

/// Where the real terminal cursor should sit in the New Session dialog.
pub(super) enum ActiveCursor {
    /// The editable name field. `base_col`/`row` are the start of the value
    /// (relative to the inner area); the caller applies the tui-input visual
    /// cursor from there.
    Name { row: u16, base_col: u16 },
    /// The project filter (a plain `String`); the cursor sits at absolute `col`.
    Filter { row: u16, col: u16 },
}

/// Label column contents for the collapsed grid; the widest determines the
/// value column offset.
const NAME_LABEL: &str = "Session name";
const SERVER_LABEL: &str = "Server";
const PROJECT_LABEL: &str = "Project";
const PROGRAM_LABEL: &str = "Program";
const SECTION_LABEL: &str = "Section";

/// Build the body lines for the New Session (`Modal::Input`) dialog.
///
/// Pure so it can be unit-tested without a terminal. Laid out like the settings
/// modal: the fields (Session name / Server / Project / Program / Section) are
/// shown collapsed as aligned `label  value` rows, and a picker only expands into
/// an inline dropdown — filter line + `theme.selection()` bar rows — when its row
/// is focused and `expanded`. The Server field appears only with >1 backend and
/// the Section field only when the backend has configured sections. The `❯` caret
/// marks only the two text inputs (the name value and, when open, the project
/// filter). Flows without any picker (Rename, AddProject…) fall back to the
/// original prompt + single input.
/// `width` is the inner content width, used to pad the selection bar. Returns
/// the lines plus where the active text cursor should be placed, if any.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_input_modal_lines(
    prompt: &str,
    value: &str,
    hint: Option<&str>,
    project_picker: Option<&ProjectPicker>,
    program_picker: Option<&ProgramPicker>,
    server_picker: Option<&ServerPicker>,
    section_picker: Option<&SectionPicker>,
    focus: InputFocus,
    expanded: bool,
    max_rows: usize,
    width: u16,
    theme: &Theme,
) -> (Vec<Line<'static>>, Option<ActiveCursor>) {
    let header_style = Style::default()
        .fg(theme.text_accent)
        .add_modifier(Modifier::BOLD);
    let dim_style = Style::default().add_modifier(Modifier::DIM);
    let italic_info = Style::default()
        .fg(theme.modal_info)
        .add_modifier(Modifier::ITALIC);
    let selection_style = theme.selection();

    // Non-creating flows (Rename, AddProject…) have no pickers: keep the
    // original compact prompt + single input.
    if project_picker.is_none()
        && program_picker.is_none()
        && server_picker.is_none()
        && section_picker.is_none()
    {
        let mut lines = vec![
            Line::from(prompt.to_string()),
            Line::from(""),
            Line::from(format!("❯ {value}")),
        ];
        if let Some(h) = hint {
            lines.push(Line::from(Span::styled(h.to_string(), italic_info)));
        }
        return (
            lines,
            Some(ActiveCursor::Name {
                row: 2,
                base_col: 2,
            }),
        );
    }

    // A dropdown list row, indented two columns; the selected row becomes a
    // full-width `theme.selection()` bar (text padded to `width`).
    let row_line = |text: String, selected: bool| -> Line<'static> {
        if selected {
            let padded = format!("  {text:<pad$}", pad = (width as usize).saturating_sub(2));
            Line::from(Span::styled(padded, selection_style))
        } else {
            Line::from(vec![Span::raw("  "), Span::styled(text, dim_style)])
        }
    };

    let label_w = NAME_LABEL.len();
    // Value column: label + "  ❯ " (name) / "    " (pickers), so values align.
    let value_col = (label_w + 4) as u16;

    let mut lines: Vec<Line> = Vec::new();
    let mut cursor: Option<ActiveCursor> = None;

    // A collapsed field row: `label    value  ▾`. `▾` only when the row can be
    // expanded (pickers) and isn't currently open.
    let field_row = |label: &str, val: String, focused: bool, chevron: bool| -> Line<'static> {
        let label_style = if focused { header_style } else { dim_style };
        let mut spans = vec![
            Span::styled(format!("{label:<label_w$}"), label_style),
            Span::raw("    "),
            Span::raw(val),
        ];
        if chevron {
            spans.push(Span::styled("  ▾", dim_style));
        }
        Line::from(spans)
    };

    // Name field.
    {
        let focused = focus == InputFocus::Name;
        let label_style = if focused { header_style } else { dim_style };
        lines.push(Line::from(vec![
            Span::styled(format!("{NAME_LABEL:<label_w$}"), label_style),
            Span::raw("  ❯ "),
            Span::raw(value.to_string()),
        ]));
        if focused {
            cursor = Some(ActiveCursor::Name {
                row: (lines.len() - 1) as u16,
                base_col: value_col,
            });
        }
    }

    // Server field (+ inline dropdown when open). Only shown when a picker is
    // present (i.e. more than one backend is configured).
    if let Some(picker) = server_picker {
        let focused = focus == InputFocus::Server;
        let open = focused && expanded;
        let val = picker
            .choices
            .get(picker.selected)
            .map(|(_, name)| name.clone())
            .unwrap_or_default();
        lines.push(field_row(SERVER_LABEL, val, focused, !open));

        if open {
            for (i, (_, name)) in picker.choices.iter().enumerate() {
                lines.push(row_line(name.clone(), i == picker.selected));
            }
        }
    }

    // Project field (+ inline dropdown when open).
    if let Some(picker) = project_picker {
        let focused = focus == InputFocus::Project;
        let open = focused && expanded;
        let val = picker
            .selected_choice()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "(none)".to_string());
        lines.push(field_row(PROJECT_LABEL, val, focused, !open));

        if open {
            let filter_row = lines.len() as u16;
            if picker.filter.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("  ❯ "),
                    Span::styled("type to filter…", dim_style),
                ]));
            } else {
                lines.push(Line::from(format!("  ❯ {}", picker.filter)));
            }
            cursor = Some(ActiveCursor::Filter {
                row: filter_row,
                col: 4 + picker.filter.chars().count() as u16,
            });

            if picker.filtered.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  (no matching projects)",
                    dim_style,
                )));
            } else {
                let start = picker.scroll;
                let end = (start + max_rows).min(picker.filtered.len());
                for row in start..end {
                    let choice = &picker.choices[picker.filtered[row]];
                    lines.push(row_line(choice.name.clone(), row == picker.selected));
                }
            }
        }
    }

    // Program field (+ inline dropdown when open).
    if let Some(picker) = program_picker {
        let focused = focus == InputFocus::Program;
        let open = focused && expanded;
        let val = picker
            .choices
            .get(picker.selected)
            .map(|e| e.label.clone())
            .unwrap_or_default();
        lines.push(field_row(PROGRAM_LABEL, val, focused, !open));

        if open {
            for (i, entry) in picker.choices.iter().enumerate() {
                // Append the command in parens only when it differs from the
                // label (the synthesised entry has label == command).
                let text = if entry.label == entry.command {
                    entry.label.clone()
                } else {
                    format!("{}  ({})", entry.label, entry.command)
                };
                lines.push(row_line(text, i == picker.selected));
            }
        }
    }

    // Section field (+ inline dropdown when open). Hidden unless the picker holds
    // more than just the catch-all (no configured sections → nothing to choose).
    if let Some(picker) = section_picker.filter(|p| p.choices.len() > 1) {
        let focused = focus == InputFocus::Section;
        let open = focused && expanded;
        let val = picker
            .choices
            .get(picker.selected)
            .cloned()
            .unwrap_or_default();
        lines.push(field_row(SECTION_LABEL, val, focused, !open));

        if open {
            for (i, name) in picker.choices.iter().enumerate() {
                lines.push(row_line(name.clone(), i == picker.selected));
            }
        }
    }

    if let Some(h) = hint {
        lines.push(Line::from(Span::styled(h.to_string(), italic_info)));
    }

    // Footer hint (context-sensitive), separated by a blank line. On the name
    // row Space types a literal space, so only advertise "Space choose" when a
    // picker row is focused.
    let footer = if expanded {
        "↑↓ choose · Enter select · Esc close"
    } else if focus == InputFocus::Name {
        "↑↓ move · Enter create · Esc cancel"
    } else {
        "↑↓ move · Space choose · Enter create · Esc cancel"
    };
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(footer, dim_style)));

    (lines, cursor)
}

impl App {
    pub(super) fn render_modal(&mut self, frame: &mut Frame, area: Rect) {
        // Record the review body geometry (depends only on `area`) so mouse
        // events can map a screen position to a diff line. Done before the
        // borrow below since it mutates `ui_state`.
        self.ui_state.review_body_rect = match self.ui_state.modal {
            Modal::ReviewDiff(_) => Some(super::review::review_body_inner_rect(area)),
            _ => None,
        };

        // Record the rows-area of any open list modal so mouse events can
        // map a click position to a list index (same pattern as
        // `review_body_rect` above).
        self.ui_state.modal_list_rect = match &self.ui_state.modal {
            Modal::QuickSwitch { matches, .. } => Some(quick_switch_areas(area, matches.len()).1),
            Modal::CheckoutBranch { filtered, .. } => {
                Some(checkout_branch_areas(area, filtered.len()).1)
            }
            Modal::PathInput { .. } => Some(path_input_areas(area).1),
            _ => None,
        };

        // Fit an open colour picker's grid to the pane it is about to be drawn
        // in, so the rows drawn and the rows `j`/`k` step through agree —
        // including after a resize.
        if let Modal::Settings(SettingsState {
            editing: Some(SettingsEditing::Colour { picker }),
            ..
        }) = &mut self.ui_state.modal
        {
            picker.fit_to_width(super::settings::colour_picker_width(area));
        }

        match &self.ui_state.modal {
            Modal::EditSession(editor) => {
                super::edit_session::render(frame, area, editor, false, &self.theme)
            }
            Modal::EditSessionRestart(editor) => {
                super::edit_session::render(frame, area, editor, true, &self.theme)
            }
            Modal::None => {}

            // Full-screen takeovers are rendered directly in `render()`, not here.
            Modal::Conversation { .. } => {}
            Modal::ReviewDiff(_) => {}

            Modal::Input {
                title,
                prompt,
                value,
                existing_branches,
                project_picker,
                program_picker,
                server_picker,
                section_picker,
                focus,
                expanded,
                mask,
                ..
            } => {
                // Cap the visible project rows so a long list can't grow the
                // modal off-screen; the list scrolls (via `picker.scroll`) to
                // keep the highlight in view.
                let max_rows = super::MAX_PROJECT_ROWS;

                // Resolve the existing-branch collision hint up-front.
                let hint = existing_branches.as_ref().and_then(|branches| {
                    claude_commander_core::session::match_existing_branch(
                        value.value(),
                        &self.config.branch_prefix,
                        branches,
                    )
                    .map(|b| format!("↳ existing branch: {} — will check out", b))
                });

                // Secret entry (e.g. a bearer token) renders as bullets; the
                // underlying Input still holds the real value, and bullets are
                // width-1 so the tui-input cursor math is unaffected.
                let masked;
                let shown_value = if *mask {
                    masked = "•".repeat(value.value().chars().count());
                    masked.as_str()
                } else {
                    value.value()
                };

                // Width is independent of height, so size the modal to the exact
                // number of body lines the (collapsed-or-expanded) layout emits.
                let modal_width = (area.width * 60 / 100).max(40);
                let inner_width = modal_width.saturating_sub(2);
                let (lines, cursor) = build_input_modal_lines(
                    prompt,
                    shown_value,
                    hint.as_deref(),
                    project_picker.as_ref(),
                    program_picker.as_ref(),
                    server_picker.as_ref(),
                    section_picker.as_ref(),
                    *focus,
                    *expanded,
                    max_rows,
                    inner_width,
                    &self.theme,
                );
                let modal_height = (lines.len() as u16 + 2).min(area.height);
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
                    .border_type(self.border_type())
                    .border_style(Style::default().fg(self.theme.modal_warning));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);
                frame.render_widget(Paragraph::new(lines), inner);

                // Place the real cursor at whichever text field the layout
                // reported as active (the name input or the project filter).
                match cursor {
                    Some(ActiveCursor::Name { row, base_col }) => place_input_cursor(
                        frame,
                        value,
                        inner.x + base_col,
                        inner.y + row,
                        inner.width.saturating_sub(base_col),
                    ),
                    Some(ActiveCursor::Filter { row, col }) => {
                        frame.set_cursor_position((
                            (inner.x + col).min(inner.x + inner.width.saturating_sub(1)),
                            inner.y + row,
                        ));
                    }
                    None => {}
                }
            }

            Modal::PathInput {
                title,
                prompt,
                value,
                completer,
                scroll,
                ..
            } => {
                let (modal_area, rows_area) = path_input_areas(area);
                frame.render_widget(Clear, modal_area);

                let block = Block::default()
                    .title(format!(" {} ", title))
                    .borders(Borders::ALL)
                    .border_type(self.border_type())
                    .border_style(Style::default().fg(self.theme.modal_warning));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);

                // Prompt + input on the top three rows, completions below,
                // hint on the last row (geometry shared with the mouse
                // handler via `path_input_areas`).
                let input_area = Rect {
                    height: inner.height.min(3),
                    ..inner
                };
                let input_text = format!("{}\n\n❯ {}", prompt, value.value());
                let input_para = Paragraph::new(input_text);
                frame.render_widget(input_para, input_area);
                // The "❯ " input line is the third row of `input_area`.
                place_input_cursor(
                    frame,
                    value,
                    input_area.x + 2,
                    input_area.y + 2,
                    input_area.width.saturating_sub(2),
                );

                // Render completions list with a scroll window so the
                // highlighted row stays on-screen even when the list is
                // longer than the visible area.
                let (completions, highlighted) = completer.visible_completions();
                if !completions.is_empty() && rows_area.height > 0 {
                    let visible = rows_area.height as usize;
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
                    frame.render_widget(completions_para, rows_area);
                }

                if inner.height >= 5 {
                    let hint_area = Rect {
                        y: inner.y + inner.height - 1,
                        height: 1,
                        ..inner
                    };
                    let hint = Line::from(Span::styled(
                        "↑/↓ navigate  Tab complete  Enter submit  Esc cancel",
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                    frame.render_widget(Paragraph::new(hint), hint_area);
                }
            }

            Modal::Loading {
                title,
                message,
                hint,
            } => {
                let modal_area = centered_rect(60, 20, area);
                frame.render_widget(Clear, modal_area);

                let block = Block::default()
                    .title(format!(" {} ", title))
                    .borders(Borders::ALL)
                    .border_type(self.border_type())
                    .border_style(Style::default().fg(self.theme.modal_info));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);

                // Spinner on the first row; an optional dimmed hint two rows
                // below it (a blank gap keeps them visually separate).
                let throbber_area = Rect { height: 1, ..inner };

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
                frame.render_stateful_widget(
                    throbber,
                    throbber_area,
                    &mut self.ui_state.throbber_state,
                );

                if let Some(hint) = hint
                    && inner.height >= 3
                {
                    // Indent to line up under the spinner's label (the throbber
                    // glyph + space offsets the message text by two cells).
                    let hint_area = Rect {
                        x: inner.x + 2,
                        y: inner.y + 2,
                        width: inner.width.saturating_sub(2),
                        height: 1,
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            hint.as_str(),
                            Style::default()
                                .fg(ratatui::style::Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ))),
                        hint_area,
                    );
                }
            }

            Modal::Confirm { title, message, .. } => {
                let modal_area = confirm_modal_area(message, area);
                frame.render_widget(Clear, modal_area);

                let block = Block::default()
                    .title(format!(" {} ", title))
                    .borders(Borders::ALL)
                    .border_type(self.border_type())
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
                    .border_type(self.border_type())
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

            Modal::Info { scroll } => {
                let mut offset = *scroll;
                self.render_info_modal(frame, area, &mut offset);
                // The selected session may have vanished (deleted while open),
                // in which case `render_info_modal` closes the modal — only
                // write the clamped scroll back if it's still an Info modal.
                if let Modal::Info { scroll } = &mut self.ui_state.modal {
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
                // Rendered by `render`, underneath this palette.
                review: _,
            } => {
                let (modal_area, rows_area) = quick_switch_areas(area, matches.len());
                frame.render_widget(Clear, modal_area);
                self.render_quick_switch(
                    frame,
                    modal_area,
                    rows_area,
                    *mode,
                    query,
                    matches,
                    *selected_idx,
                    *scroll,
                );
            }

            Modal::CheckoutBranch {
                query,
                filtered,
                selected_idx,
                scroll,
                fetching,
                ..
            } => {
                let (modal_area, rows_area) = checkout_branch_areas(area, filtered.len());

                frame.render_widget(Clear, modal_area);

                let title = if *fetching {
                    " Checkout Branch — fetching origin… "
                } else {
                    " Checkout Branch "
                };
                let block = Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_type(self.border_type())
                    .border_style(Style::default().fg(self.theme.modal_info));

                let inner = block.inner(modal_area);
                frame.render_widget(block, modal_area);

                if inner.height == 0 {
                    return;
                }

                // Input line
                let input_line = Line::from(format!("❯ {}", query.value()));
                let input_area = Rect { height: 1, ..inner };
                frame.render_widget(Paragraph::new(input_line), input_area);
                place_input_cursor(
                    frame,
                    query,
                    input_area.x + 2,
                    input_area.y,
                    input_area.width.saturating_sub(2),
                );

                // Hint line
                let hint = if filtered.is_empty() {
                    if query.value().is_empty() {
                        "No branches found. Press Esc to cancel.".to_string()
                    } else {
                        format!(
                            "No match — press Enter to use '{}' as-is, or keep typing.",
                            query.value()
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
                let list_top = rows_area.y;
                if rows_area.height == 0 {
                    return;
                }
                let list_height = rows_area.height as usize;
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

        // Board (hardcoded main-view keys, not bindable actions).
        lines.push(Line::from(""));
        lines.push(Line::from("Board:"));
        lines.push(Line::from(format!(
            "  {:<width$}Enter on a sidebar project filters the board to it;",
            "filter",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}Enter again (or Esc) clears the filter",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}Jump to a session by its number",
            "1-99",
            width = key_col_width,
        )));

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
            "  {:<width$}Quick switch (same palette as the in-session switcher; in the review diff, sessions only)",
            "Ctrl+Space",
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
        lines.push(Line::from(format!(
            "  {:<width$}The palette searches every workspace: the active one's",
            "workspaces",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}sessions rank first, others are tagged with their",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}workspace, and picking one switches workspace first.",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}Each workspace can have its own theme, worn while it",
            "themes",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}is active: edit it in Settings → Theme (\"Theme for\"),",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}or Enter on its Theme row in Settings → Workspaces.",
            "",
            width = key_col_width,
        )));

        // Clone picker (in-modal keys, not bindable actions). The command
        // itself is listed under Projects above; these are the picker's own.
        lines.push(Line::from(""));
        lines.push(Line::from("Clone Repository:"));
        lines.push(Line::from(format!(
            "  {:<width$}Re-list the selected code host's repos (the listing can be slow)",
            "Ctrl+R",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}With nothing matching, the typed text is used as a",
            "type a URL",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}clone URL — for repos `gh` doesn't list.",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}Enter then edits the destination directory name.",
            "",
            width = key_col_width,
        )));

        // Remote image paste (an intercepted key in the attach loop, not a
        // bindable action — only active when attached to a remote session).
        lines.push(Line::from(""));
        lines.push(Line::from("Remote Image Paste:"));
        lines.push(Line::from(format!(
            "  {:<width$}Attached to a REMOTE session: paste a clipboard image —",
            "Ctrl+V",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}uploaded to the server and its path typed into the prompt.",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}Local sessions forward Ctrl+V to Claude unchanged.",
            "",
            width = key_col_width,
        )));

        // Global voice hotkey (a desktop shortcut, not an in-app keybinding).
        lines.push(Line::from(""));
        lines.push(Line::from("Global Voice Hotkey:"));
        lines.push(Line::from(format!(
            "  {:<width$}Toggle voice input system-wide: bind a desktop",
            "system-wide",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}shortcut to `claude-commander listen-toggle`",
            "",
            width = key_col_width,
        )));

        // Embedded server (ambient fact, not a keybinding — same category as
        // the global voice hotkey above).
        lines.push(Line::from(""));
        lines.push(Line::from("Embedded Server:"));
        lines.push(Line::from(format!(
            "  {:<width$}Run the HTTP API in this process so clients can reach",
            "--serve",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}this machine; set `[server] auto_start` to make it the",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}default. Settings > Server edits bind/port/token.",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}Use the palette's \"Copy server token\" to pair a",
            "palette",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}client; the URL is shown in the status bar.",
            "",
            width = key_col_width,
        )));

        // Mouse (the status/review bars surface primary actions as buttons).
        lines.push(Line::from(""));
        lines.push(Line::from("Mouse:"));
        lines.push(Line::from(format!(
            "  {:<width$}Primary actions appear as clickable buttons in the",
            "click",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}status bar (and review footer); the bracketed letter",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}is the hotkey. Clicking fires the same action.",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}Each session card carries [>_] shell, [±] review",
            "card buttons",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}diff, and [i] info buttons; double-click a card to",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}attach.",
            "",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}Click a server heading in the sidebar to edit its",
            "click ⚙",
            width = key_col_width,
        )));
        lines.push(Line::from(format!(
            "  {:<width$}program list (shown with multiple servers).",
            "",
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
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("\u{21c5}", Style::default().fg(self.theme.status_running)),
            Span::raw("  HTTP API served from this process, on that port"),
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
            .border_type(self.border_type())
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

    /// Render the session Info modal (`Modal::Info`). Mirrors
    /// [`render_help_modal`]: the caller passes the current scroll, this clamps
    /// it against the composed content height and writes back the clamp result.
    ///
    /// If the selected session has disappeared (e.g. deleted while the modal is
    /// open) the composed content is `Empty`; rather than show a stray "select a
    /// session" panel we close the modal gracefully.
    pub(super) fn render_info_modal(&mut self, frame: &mut Frame, area: Rect, scroll: &mut u16) {
        // Resolve the session title up front from owned data so the close
        // decision doesn't hold a borrow of `self` into the mutation below.
        // Resolve from the owning backend's snapshot (always populated), not the
        // board — the board is only built in board view, so a board lookup would
        // wrongly close the modal in any list view.
        let title = self
            .ui_state
            .selected_session_id
            .and_then(|sref| self.session(sref))
            .map(|s| s.title.clone());
        let Some(title) = title else {
            self.ui_state.modal = Modal::None;
            return;
        };

        let modal_area = centered_rect(70, 80, area);
        frame.render_widget(Clear, modal_area);

        // Truncate a long session title so the border title can't overflow.
        let max_title = modal_area.width.saturating_sub(12) as usize;
        let shown_title = if title.chars().count() > max_title && max_title > 1 {
            let head: String = title.chars().take(max_title.saturating_sub(1)).collect();
            format!("{head}…")
        } else {
            title
        };

        let block = Block::default()
            .title(format!(" Info — {shown_title} "))
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(Style::default().fg(self.theme.modal_info));
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let view = InfoView::new(self.build_info_content(), &self.theme);
        let lines = view.build_lines();

        let visible = inner.height;
        let max_scroll = (lines.len() as u16).saturating_sub(visible);
        if *scroll > max_scroll {
            *scroll = max_scroll;
        }
        let offset = *scroll;

        frame.render_widget(view.with_prebuilt_lines(lines).scroll(offset), inner);

        if max_scroll > 0 {
            let mut sb_state = ScrollbarState::new(max_scroll as usize + 1)
                .position(offset as usize)
                .viewport_content_length(visible as usize);
            let scrollbar = Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);
            frame.render_stateful_widget(scrollbar, inner, &mut sb_state);
        }
    }
}

/// Helper to center a rect within an area
pub(crate) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
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

/// Rows one logical line occupies once word-wrapped to `width` columns,
/// mirroring ratatui's `WordWrapper` (the wrap `Paragraph` uses): whitespace-
/// separated words are packed greedily, and a word longer than `width` is
/// hard-split across rows. Char count stands in for display width (the confirm
/// messages are ASCII branch names / prose); the estimate never *under*-counts
/// versus the real wrap, so the modal is sized conservatively and never clips.
fn wrapped_line_rows(line: &str, width: usize) -> u16 {
    let mut rows: u16 = 1;
    let mut col = 0usize; // columns already used on the current row
    for word in line.split_whitespace() {
        let wlen = word.chars().count();
        // A space precedes the word only when the row already holds content.
        if col > 0 && col + 1 + wlen > width {
            rows += 1;
            col = 0;
        }
        if wlen <= width {
            col = if col == 0 { wlen } else { col + 1 + wlen };
        } else {
            // Over-long word (starts on a fresh row): fills whole rows, and the
            // remainder — if any — continues packing on the last row.
            let full = (wlen / width) as u16;
            let rem = wlen % width;
            if rem == 0 {
                rows += full.saturating_sub(1);
                col = width;
            } else {
                rows += full;
                col = rem;
            }
        }
    }
    rows
}

/// Rows a multi-line block occupies once wrapped to `width`. Blank lines count
/// as one row each, matching how `Paragraph` renders them.
fn wrapped_rows(text: &str, width: usize) -> u16 {
    text.split('\n').map(|l| wrapped_line_rows(l, width)).sum()
}

/// Geometry of a `Confirm` modal sized to fit `message` plus the confirm/cancel
/// footer, rather than a fixed fraction of the screen. Height grows with the
/// wrapped content and is capped at the terminal height so a long branch list
/// (e.g. the "Delete merged-PR sessions" prompt) is fully shown instead of
/// clipped. Width tracks 50% of the screen (min 40), centred like `centered_rect`.
pub(crate) fn confirm_modal_area(message: &str, area: Rect) -> Rect {
    const FOOTER: &str = "[Enter] Confirm  [Esc] Cancel";
    // `min` last, not `clamp(40, ..)`: on a terminal narrower than 40 the low
    // bound would exceed the high bound and `clamp` panics, so clamp down to the
    // screen after applying the 40-col floor.
    let modal_width = (area.width / 2).max(40).min(area.width.max(1));
    let inner_width = (modal_width.saturating_sub(2).max(1)) as usize;

    // message + blank separator + footer — mirrors the render arm's layout.
    let content_rows = wrapped_rows(message, inner_width) + 1 + wrapped_rows(FOOTER, inner_width);
    let modal_height = (content_rows + 2).min(area.height.max(1));

    Rect {
        x: area.x + (area.width.saturating_sub(modal_width)) / 2,
        y: area.y + (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    }
}

impl App {
    /// Render the quick-switch palette into a caller-supplied geometry.
    ///
    /// Split out of [`Self::render_modal`] because the palette has two callers
    /// with different ideas of where it lives: the in-TUI modal derives its
    /// areas from the full screen, while the in-session switcher overlay draws
    /// into a fixed rectangle it has reserved over a live tmux pane. Sharing the
    /// renderer is what makes them the *same* palette rather than two that drift.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_quick_switch(
        &self,
        frame: &mut Frame<'_>,
        modal_area: Rect,
        rows_area: Rect,
        mode: PaletteMode,
        query: &Input,
        matches: &[QuickSwitchItem],
        selected_idx: usize,
        scroll: usize,
    ) {
        let max_visible = super::actions::LIST_MAX_VISIBLE;
        // Switch the modal title by effective mode so a `>`-prefixed
        // query in unified mode reads as "Commands" while we type.
        let effective_mode = App::effective_palette_mode(mode, query.value());
        let title = match effective_mode {
            PaletteMode::Unified => " Quick Switch ",
            PaletteMode::CommandOnly => " Commands ",
            PaletteMode::SessionOnly => " Switch Session ",
            PaletteMode::SectionPicker { .. } => " Move to Section ",
            PaletteMode::RemoteServerPicker => " Remove Remote Server ",
            PaletteMode::ProgramPicker { .. } => " Change Program ",
            PaletteMode::BasePicker { .. } => " Set Session Base ",
            PaletteMode::WorkspacePicker => " Switch Workspace — or type a new name ",
            PaletteMode::MoveProjectPicker { .. } => " Move Project to Workspace ",
            // The fetch state lives in the title (as the Checkout modal
            // does with "fetching origin…") so a slow or failed provider
            // listing is visible rather than reading as an empty account.
            // The failure's detail goes to the status bar.
            PaletteMode::RepositoryPicker
                if self.ui_state.repo_picker.host.provider
                    == claude_commander_protocol::hosting::CodeHostProvider::Gitlab =>
            {
                match self.ui_state.repo_picker.fetch {
                    RepoFetch::Loading => " Clone Repository — listing GitLab projects… ",
                    RepoFetch::Ready if self.ui_state.repo_picker.repos.is_empty() => {
                        " Clone Repository — no GitLab projects; type a URL "
                    }
                    RepoFetch::Ready => " Clone Repository — Enter a GitLab project or URL ",
                    RepoFetch::Failed(_) => {
                        " Clone Repository — GitLab list unavailable; type a URL "
                    }
                }
            }
            PaletteMode::RepositoryPicker => match self.ui_state.repo_picker.fetch {
                RepoFetch::Loading => " Clone Repository — listing hosted repositories… ",
                RepoFetch::Ready => " Clone Repository — Enter a repo, or type a URL ",
                RepoFetch::Failed(_) => " Clone Repository — no repo list; type a URL ",
            },
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(Style::default().fg(self.theme.modal_info));

        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        if inner.height == 0 {
            return;
        }

        // Input line
        let input_line = Line::from(format!("❯ {}", query.value()));
        let input_area = Rect { height: 1, ..inner };
        frame.render_widget(Paragraph::new(input_line), input_area);
        place_input_cursor(
            frame,
            query,
            input_area.x + 2,
            input_area.y,
            input_area.width.saturating_sub(2),
        );

        // Match lines. The `scroll` offset lets us page through a
        // list longer than `max_visible`; rows below `scroll` are
        // off the top of the window, rows at/after
        // `scroll + max_visible` are off the bottom.
        let start = scroll.min(matches.len());
        for (i, item) in matches.iter().skip(start).take(max_visible).enumerate() {
            let row = rows_area.y + i as u16;
            if row >= rows_area.y + rows_area.height {
                break;
            }
            let abs_idx = start + i;
            let is_selected = abs_idx == selected_idx;

            let line_area = Rect {
                y: row,
                height: 1,
                ..inner
            };

            match item {
                QuickSwitchItem::Session(m) => {
                    // The tree's own glyph, from the shared helper: the palette
                    // used to switch on `SessionStatus` alone, which drew every
                    // running session as an idle `●` however its agent was doing
                    // — so the same session read differently in the two places.
                    let mut spans = Vec::new();
                    if let Some((glyph, color)) = status_glyph::session_status_glyph(
                        &self.theme,
                        self.ui_state.tick_count,
                        m.status,
                        m.agent_state,
                        m.unread,
                    ) {
                        spans.push(Span::styled(
                            format!(" {glyph} "),
                            Style::default().fg(color),
                        ));
                    }
                    spans.push(Span::styled(
                        m.title.clone(),
                        match (is_selected, m.unread) {
                            (true, _) => self.theme.selection(),
                            // Unread titles are bold in the tree too.
                            (false, true) => Style::default().add_modifier(Modifier::BOLD),
                            (false, false) => Style::default(),
                        },
                    ));
                    if let Some(shown_branch) =
                        claude_commander_core::session::display_branch(&m.title, &m.branch)
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
                    // A session outside the active workspace: tag it, dimly —
                    // picking it switches workspace first.
                    if let Some(workspace) = &m.other_workspace {
                        spans.push(Span::styled(
                            format!(" \u{00b7} {workspace}"),
                            Style::default()
                                .fg(self.theme.text_secondary)
                                .add_modifier(Modifier::DIM),
                        ));
                    }
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
                QuickSwitchItem::SectionMove { label, .. }
                | QuickSwitchItem::RemoteServerRemove { label, .. }
                | QuickSwitchItem::HostedRepository { label, .. }
                | QuickSwitchItem::BaseChange { label, .. }
                | QuickSwitchItem::ProgramChange { label, .. }
                | QuickSwitchItem::Workspace { label, .. }
                | QuickSwitchItem::ProjectWorkspace { label, .. } => {
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
}

// List-modal geometry, shared between the render arms and the mouse handler
// so a click maps onto exactly the rows the renderer drew. Each `*_areas`
// function returns `(modal_area, rows_area)` where `rows_area` covers only
// the selectable list rows (zero-height when the modal is too small to show
// any).

/// Geometry of the quick-switch palette for `n_matches` filtered rows.
pub(super) fn quick_switch_areas(area: Rect, n_matches: usize) -> (Rect, Rect) {
    let visible = n_matches.min(super::actions::LIST_MAX_VISIBLE);
    // Dynamic height: border(2) + input(1) + rows, positioned in the
    // upper third of the screen.
    let modal_height = (3 + visible) as u16;
    let modal_width = (area.width * 60 / 100).max(40);
    let modal_area = Rect {
        x: area.x + (area.width.saturating_sub(modal_width)) / 2,
        y: area.y + area.height / 5,
        width: modal_width,
        height: modal_height.min(area.height),
    };
    let inner = modal_area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let rows = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    (modal_area, rows)
}

/// Geometry of the checkout-branch modal for `n_filtered` branch rows.
pub(super) fn checkout_branch_areas(area: Rect, n_filtered: usize) -> (Rect, Rect) {
    // Target up to 12 visible branch rows, but always at least one row of
    // space so the "no match" state doesn't collapse the modal.
    let desired_visible = n_filtered.clamp(1, 12);
    // border(2) + input(1) + hint(1) + rows
    let modal_height = (4 + desired_visible) as u16;
    let modal_width = (area.width * 70 / 100).max(50);
    let modal_area = Rect {
        x: area.x + (area.width.saturating_sub(modal_width)) / 2,
        y: area.y + area.height / 6,
        width: modal_width,
        height: modal_height.min(area.height),
    };
    let inner = modal_area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let rows = Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(2),
        ..inner
    };
    (modal_area, rows)
}

/// Geometry of the path-input modal. Fixed height: border(2) +
/// prompt/input(3) + LIST_MAX_VISIBLE rows + hint(1) when the full window
/// fits, capped to the terminal height. Keeps the modal size predictable so
/// navigation (which assumes LIST_MAX_VISIBLE) lines up with the rendered
/// window.
pub(super) fn path_input_areas(area: Rect) -> (Rect, Rect) {
    let list_rows = super::actions::LIST_MAX_VISIBLE as u16;
    let modal_height: u16 = (2 + 3 + list_rows + 1).min(area.height.max(1));
    let modal_width = (area.width * 60 / 100).max(50);
    let modal_area = Rect {
        x: area.x + (area.width.saturating_sub(modal_width)) / 2,
        y: area.y + (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };
    let inner = modal_area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let rows = Rect {
        y: inner.y + 3,
        height: inner.height.saturating_sub(4),
        ..inner
    };
    (modal_area, rows)
}

#[cfg(test)]
mod tests {
    use super::{ActiveCursor, build_input_modal_lines};
    use crate::app::{
        InputFocus, ProgramPicker, ProjectChoice, ProjectPicker, SectionPicker, ServerPicker,
    };
    use crate::theme::Theme;
    use claude_commander_core::backend::{BackendId, LOCAL_BACKEND_ID};
    use claude_commander_core::config::ProgramEntry;
    use claude_commander_core::session::ProjectId;
    use ratatui::text::Line;

    const MAX_ROWS: usize = 8;
    const WIDTH: u16 = 40;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Number of body lines carrying the `❯` input caret. Only text-input lines
    /// (the name value and, when open, the project filter) ever carry it — never
    /// a list row.
    fn caret_lines(lines: &[Line]) -> usize {
        lines.iter().filter(|l| line_text(l).contains('❯')).count()
    }

    fn has_line(lines: &[Line], substr: &str) -> bool {
        lines.iter().any(|l| line_text(l).contains(substr))
    }

    /// The style of the dropdown row whose trimmed text is `text`, taken from the
    /// span that carries it. A selected row is one padded span styled with the
    /// selection bar; an unselected row's text lives in its own dim span.
    fn row_style(lines: &[Line], text: &str) -> Option<ratatui::style::Style> {
        lines
            .iter()
            .find(|l| line_text(l).trim() == text)
            .and_then(|l| l.spans.iter().find(|s| s.content.contains(text)))
            .map(|s| s.style)
    }

    fn project_choices(names: &[&str]) -> Vec<ProjectChoice> {
        names
            .iter()
            .map(|n| ProjectChoice {
                id: ProjectId::new(),
                name: n.to_string(),
                repo_path: std::path::PathBuf::from(format!("/repos/{n}")),
            })
            .collect()
    }

    fn program_fixture(entries: &[(&str, &str)], selected: usize) -> ProgramPicker {
        ProgramPicker {
            choices: entries
                .iter()
                .map(|(label, command)| ProgramEntry {
                    label: label.to_string(),
                    command: command.to_string(),
                })
                .collect(),
            selected,
        }
    }

    /// Build with the default test prompt/theme/width; the varying inputs are
    /// the value, project/program pickers, focus, and expansion. No server or
    /// section picker (see [`build_full`] for those).
    fn build(
        value: &str,
        project: Option<&ProjectPicker>,
        program: Option<&ProgramPicker>,
        focus: InputFocus,
        expanded: bool,
    ) -> (Vec<Line<'static>>, Option<ActiveCursor>) {
        build_full(value, project, program, None, None, focus, expanded)
    }

    /// Build with every picker specified, for the server/section field tests.
    #[allow(clippy::too_many_arguments)]
    fn build_full(
        value: &str,
        project: Option<&ProjectPicker>,
        program: Option<&ProgramPicker>,
        server: Option<&ServerPicker>,
        section: Option<&SectionPicker>,
        focus: InputFocus,
        expanded: bool,
    ) -> (Vec<Line<'static>>, Option<ActiveCursor>) {
        build_input_modal_lines(
            "Enter session name:",
            value,
            None,
            project,
            program,
            server,
            section,
            focus,
            expanded,
            MAX_ROWS,
            WIDTH,
            &Theme::basic(),
        )
    }

    #[test]
    fn no_picker_flow_keeps_simple_prompt_layout() {
        // Rename / AddProject: no pickers → original prompt + single input.
        let (lines, cursor) = build("my-feature", None, None, InputFocus::Name, false);
        assert!(has_line(&lines, "Enter session name:"));
        assert!(lines.iter().any(|l| line_text(l) == "❯ my-feature"));
        assert!(matches!(cursor, Some(ActiveCursor::Name { row: 2, .. })));
        // No grid labels or footer in the simple layout.
        assert!(!has_line(&lines, "Session name"));
    }

    #[test]
    fn collapsed_grid_shows_three_field_rows_with_values() {
        let choices = project_choices(&["alpha", "beta", "gamma"]);
        let project = ProjectPicker::new(choices.clone(), choices[1].id);
        let program = program_fixture(&[("claude", "claude"), ("codex", "codex")], 0);

        let (lines, cursor) = build(
            "feat",
            Some(&project),
            Some(&program),
            InputFocus::Name,
            false,
        );

        // Three labelled rows, each showing its current value.
        assert!(has_line(&lines, "Session name"));
        assert!(lines.iter().any(|l| {
            let t = line_text(l);
            t.contains("Project") && t.contains("beta")
        }));
        assert!(lines.iter().any(|l| {
            let t = line_text(l);
            t.contains("Program") && t.contains("claude")
        }));

        // Collapsed: only the name field carries a caret; no dropdown lines.
        assert_eq!(caret_lines(&lines), 1);
        assert!(!has_line(&lines, "type to filter"));
        assert!(!has_line(&lines, "gamma")); // project dropdown item not shown
        // The name field is where the cursor lives.
        assert!(matches!(cursor, Some(ActiveCursor::Name { .. })));
        // Chevron affordance on the closed picker rows.
        assert!(has_line(&lines, "▾"));
    }

    #[test]
    fn project_dropdown_expands_only_when_focused() {
        let choices = project_choices(&["alpha", "beta", "gamma"]);
        let project = ProjectPicker::new(choices.clone(), choices[1].id);

        let (lines, cursor) = build("", Some(&project), None, InputFocus::Project, true);

        // Filter line (placeholder) + dropdown items are shown; cursor is in the
        // filter, not the name field.
        assert!(has_line(&lines, "type to filter…"));
        assert!(matches!(cursor, Some(ActiveCursor::Filter { .. })));
        // Name caret + filter caret = two.
        assert_eq!(caret_lines(&lines), 2);

        // The selected item ("beta") is the settings-style selection bar; a
        // sibling has no background.
        let sel = row_style(&lines, "beta").expect("selected item rendered");
        assert_eq!(sel.bg, Theme::basic().selection().bg);
        assert!(sel.bg.is_some());
        assert!(
            row_style(&lines, "alpha")
                .expect("sibling rendered")
                .bg
                .is_none()
        );
    }

    #[test]
    fn program_dropdown_expands_only_when_focused() {
        // Second entry's command differs from its label → shown in parens.
        let program = program_fixture(&[("claude", "claude"), ("Codex", "codex --yolo")], 1);

        let (lines, cursor) = build("", None, Some(&program), InputFocus::Program, true);

        // No filter for programs → cursor stays unset; name caret only.
        assert!(cursor.is_none());
        assert_eq!(caret_lines(&lines), 1);

        // Divergent command rendered in parens, selected as the selection bar.
        let sel =
            row_style(&lines, "Codex  (codex --yolo)").expect("selected program row rendered");
        assert_eq!(sel.bg, Theme::basic().selection().bg);
        assert!(sel.bg.is_some());
    }

    #[test]
    fn project_dropdown_caps_visible_rows_at_the_scroll_window() {
        // A list longer than `max_rows` must render only the scroll window, so
        // the modal can't grow off-screen.
        let names: Vec<String> = (0..20).map(|i| format!("proj{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let choices = project_choices(&refs);
        let project = ProjectPicker::new(choices.clone(), choices[0].id);

        let (lines, _) = build("", Some(&project), None, InputFocus::Project, true);
        let item_rows = lines
            .iter()
            .filter(|l| line_text(l).trim().starts_with("proj"))
            .count();
        assert_eq!(item_rows, MAX_ROWS);
    }

    #[test]
    fn typed_filter_replaces_placeholder() {
        let choices = project_choices(&["alpha", "beta"]);
        let mut project = ProjectPicker::new(choices.clone(), choices[0].id);
        project.filter = "al".to_string();
        project.apply_filter();

        let (lines, _) = build("", Some(&project), None, InputFocus::Project, true);
        assert!(has_line(&lines, "❯ al"));
        assert!(!has_line(&lines, "type to filter…"));
    }

    #[test]
    fn no_matches_shows_placeholder_row() {
        let choices = project_choices(&["alpha", "beta"]);
        let mut project = ProjectPicker::new(choices.clone(), choices[0].id);
        project.filter = "zzz".to_string();
        project.apply_filter();

        let (lines, _) = build("", Some(&project), None, InputFocus::Project, true);
        assert!(has_line(&lines, "(no matching projects)"));
    }

    fn server_fixture(entries: &[(BackendId, &str)], selected: usize) -> ServerPicker {
        let choices = entries.iter().map(|(id, n)| (*id, n.to_string())).collect();
        let mut p = ServerPicker::new(choices, LOCAL_BACKEND_ID);
        p.selected = selected;
        p
    }

    #[test]
    fn server_field_only_rendered_when_a_picker_is_present() {
        let program = program_fixture(&[("claude", "claude")], 0);
        // No server picker → no Server row.
        let (lines, _) = build("", None, Some(&program), InputFocus::Name, false);
        assert!(!has_line(&lines, "Server"));

        // With a picker, the collapsed Server row shows the selected backend.
        let server = server_fixture(
            &[(LOCAL_BACKEND_ID, "local"), (BackendId(1), "buildbox")],
            1,
        );
        let (lines, _) = build_full(
            "",
            None,
            Some(&program),
            Some(&server),
            None,
            InputFocus::Name,
            false,
        );
        assert!(lines.iter().any(|l| {
            let t = line_text(l);
            t.contains("Server") && t.contains("buildbox")
        }));
    }

    #[test]
    fn server_dropdown_expands_only_when_focused() {
        let server = server_fixture(
            &[(LOCAL_BACKEND_ID, "local"), (BackendId(1), "buildbox")],
            1,
        );
        let (lines, cursor) = build_full(
            "",
            None,
            None,
            Some(&server),
            None,
            InputFocus::Server,
            true,
        );
        // A server picker has no filter → no caret from it; name caret only.
        assert!(cursor.is_none());
        assert_eq!(caret_lines(&lines), 1);
        // The highlighted backend is the selection bar; the sibling isn't.
        let sel = row_style(&lines, "buildbox").expect("selected server row rendered");
        assert_eq!(sel.bg, Theme::basic().selection().bg);
        assert!(
            row_style(&lines, "local")
                .expect("sibling rendered")
                .bg
                .is_none()
        );
    }

    #[test]
    fn section_field_hidden_without_configured_sections() {
        // A picker holding only the catch-all → the field is suppressed.
        let section = SectionPicker::new(Vec::new(), None);
        let (lines, _) = build_full(
            "",
            None,
            None,
            None,
            Some(&section),
            InputFocus::Name,
            false,
        );
        assert!(!has_line(&lines, "Section"));
    }

    #[test]
    fn section_dropdown_lists_catch_all_then_configured() {
        let section = SectionPicker::new(vec!["Open PRs".to_string()], None);
        // Collapsed: the Section row shows the catch-all (the default selection).
        let (lines, _) = build_full(
            "",
            None,
            None,
            None,
            Some(&section),
            InputFocus::Name,
            false,
        );
        assert!(lines.iter().any(|l| {
            let t = line_text(l);
            t.contains("Section") && t.contains(claude_commander_core::session::IN_PROGRESS)
        }));

        // Expanded: both the catch-all and the configured section are listed, and
        // the catch-all (row 0, the default) is the selection bar.
        let (lines, _) = build_full(
            "",
            None,
            None,
            None,
            Some(&section),
            InputFocus::Section,
            true,
        );
        assert!(has_line(&lines, "Open PRs"));
        let sel = row_style(&lines, claude_commander_core::session::IN_PROGRESS)
            .expect("catch-all row rendered");
        assert_eq!(sel.bg, Theme::basic().selection().bg);
    }

    #[test]
    fn confirm_modal_grows_to_fit_a_long_message() {
        use super::confirm_modal_area;
        use ratatui::layout::Rect;

        let screen = Rect::new(0, 0, 80, 40);
        // A many-branch delete prompt like "Delete merged-PR sessions".
        let message = "Delete 5 session(s) with merged PRs?\n\nBranches:\n  \
            • flutter-android\n  • terminal-scroll\n  • flutter-new-session\n  \
            • one-more\n  • and-another\n\nThis will kill the tmux sessions and \
            remove the worktrees.";

        let rect = confirm_modal_area(message, screen);

        // Body rows: message lines + blank separator + footer, plus 2 for the
        // border. The modal must be tall enough that no content line is clipped.
        let body_rows = message.split('\n').count() as u16 + 1 /* blank */ + 1 /* footer */;
        assert!(
            rect.height >= body_rows + 2,
            "modal height {} should fit {} content rows + border",
            rect.height,
            body_rows,
        );
        // The old fixed geometry was 15% of 40 = 6 rows, which clipped this
        // message — the fit-to-content modal must be taller than that.
        assert!(
            rect.height > 6,
            "modal should grow beyond the old fixed height"
        );
    }

    #[test]
    fn wrapped_line_rows_accounts_for_word_boundaries() {
        use super::wrapped_line_rows;

        // Three 20-char words at width 38: a naive char-ceil gives 60/38 = 2
        // rows, but word-wrap can't split mid-word so each word after the first
        // spills to its own row — 3 rows. The modal must be sized for 3.
        let line = "wwwwwwwwwwwwwwwwwwww xxxxxxxxxxxxxxxxxxxx yyyyyyyyyyyyyyyyyyyy";
        assert_eq!(wrapped_line_rows(line, 38), 3);
        // A single word longer than the width is hard-split across full rows.
        assert_eq!(wrapped_line_rows(&"z".repeat(80), 38), 3);
        assert_eq!(wrapped_line_rows(&"z".repeat(76), 38), 2); // exact multiple
        // Short content and blank lines are a single row.
        assert_eq!(wrapped_line_rows("short", 38), 1);
        assert_eq!(wrapped_line_rows("", 38), 1);
    }

    #[test]
    fn confirm_modal_fits_a_word_wrapped_message() {
        use super::confirm_modal_area;
        use ratatui::layout::Rect;

        // A long single-line prose message (like a git error in the "save
        // anyway?" confirm) that only word-wrap sizes correctly.
        let screen = Rect::new(0, 0, 80, 40);
        let message = "Connection test failed: could not resolve host name and \
            the request timed out after several retries against the configured \
            remote server endpoint, so the session cannot be created right now.";
        let rect = confirm_modal_area(message, screen);

        let inner_width = rect.width.saturating_sub(2) as usize;
        let expected = super::wrapped_rows(message, inner_width) + 1 /* blank */
            + super::wrapped_rows("[Enter] Confirm  [Esc] Cancel", inner_width);
        assert!(
            rect.height >= expected + 2,
            "modal height {} must fit {} word-wrapped rows + border",
            rect.height,
            expected,
        );
    }

    #[test]
    fn confirm_modal_is_capped_at_the_screen_height() {
        use super::confirm_modal_area;
        use ratatui::layout::Rect;

        let screen = Rect::new(0, 0, 60, 10);
        let message = (0..50)
            .map(|i| format!("  • branch-{i}"))
            .collect::<Vec<_>>()
            .join("\n");

        let rect = confirm_modal_area(&message, screen);

        assert!(
            rect.height <= screen.height,
            "modal height {} must not exceed screen height {}",
            rect.height,
            screen.height,
        );
    }
}
