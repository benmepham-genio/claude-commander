//! TreeList rendering: StatefulWidget impl, list item construction, hyperlinks.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListState, StatefulWidget},
};

use super::*;

impl<'a> StatefulWidget for TreeList<'a> {
    type State = ListState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        // Collect PR data before self is consumed
        let pr_data: Vec<Option<(u32, String)>> = self
            .items
            .iter()
            .map(|item| match item {
                SessionListItem::Worktree {
                    pr_number: Some(n),
                    pr_url: Some(url),
                    ..
                } => Some((*n, url.clone())),
                _ => None,
            })
            .collect();

        // Compute inner area before block is moved
        let list_area = self.block.as_ref().map_or(area, |b| b.inner(area));

        let items = self.to_list_items();
        let list = List::new(items).highlight_style(self.highlight_style);
        let list = if let Some(block) = self.block {
            list.block(block)
        } else {
            list
        };

        StatefulWidget::render(list, area, buf, state);

        // Post-process: inject OSC 8 hyperlinks for PR badges
        inject_pr_hyperlinks(list_area, buf, &pr_data, state);
    }
}

impl<'a> TreeList<'a> {
    /// Convert items to list items
    pub(super) fn to_list_items(&self) -> Vec<ListItem<'a>> {
        let show_program = self.show_session_program && self.has_mixed_programs();
        let mut project_index: usize = 0;
        let mut worktree_number: usize = 0;
        let mut current_session_color = self.theme.project_color(0).1;

        self.items
            .iter()
            .map(|item| match item {
                SessionListItem::Project {
                    id,
                    name,
                    main_branch,
                    worktree_count,
                    nested,
                    ..
                } => {
                    let (proj_color, sess_color) = self.theme.project_color(project_index);
                    current_session_color = sess_color;
                    project_index += 1;

                    let count_str = if *worktree_count > 0 {
                        format!(" ({})", worktree_count)
                    } else {
                        String::new()
                    };

                    // Project sub-headers nested under a section header are
                    // indented one tree-level deeper so the hierarchy reads
                    // SectionHeader > Project > Worktree.
                    let pad = if *nested { "   " } else { " " };

                    let mut spans: Vec<Span<'static>> = vec![
                        Span::raw(pad),
                        Span::styled(
                            name.clone(),
                            Style::default().fg(proj_color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" [{}]", main_branch),
                            Style::default().fg(self.theme.text_accent),
                        ),
                        Span::styled(count_str, Style::default().fg(self.theme.text_secondary)),
                    ];
                    if self.project_is_pull_blocked(id) {
                        spans.push(Span::styled(
                            " ⚠".to_string(),
                            Style::default().fg(self.theme.agent_waiting),
                        ));
                    }

                    ListItem::new(Line::from(spans))
                }
                SessionListItem::Spacer => {
                    let rule: String = "─".repeat(20);
                    let line = Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            rule,
                            Style::default()
                                .fg(self.theme.text_secondary)
                                .add_modifier(Modifier::DIM),
                        ),
                    ]);
                    ListItem::new(line)
                }

                SessionListItem::Worktree {
                    title,
                    branch,
                    status,
                    program,
                    pr_number,
                    pr_merged,
                    pr_state,
                    pr_draft,
                    pr_labels,
                    agent_state,
                    unread,
                    stacked_child,
                    ..
                } => {
                    worktree_number += 1;

                    // Right-aligned session number prefix, with an extra
                    // indent for stacked children so they sit one level deeper
                    // than their stack base.
                    let stack_prefix = if *stacked_child { STACK_INDENT } else { "" };
                    let indent_span = Span::styled(
                        format!(
                            "{stack_prefix}{:>width$} ",
                            worktree_number,
                            width = NUMBER_WIDTH
                        ),
                        Style::default().fg(self.theme.text_secondary),
                    );
                    let mut spans = vec![indent_span];

                    // Single status glyph: spinner > waiting > unread > running > stopped
                    if let Some((glyph, color)) =
                        self.session_status_glyph(*status, *agent_state, *unread)
                    {
                        spans.push(Span::styled(
                            format!("{glyph} "),
                            Style::default().fg(color),
                        ));
                    }

                    let title_style = if *unread {
                        Style::default()
                            .fg(current_session_color)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(current_session_color)
                    };
                    spans.push(Span::styled(title.clone(), title_style));
                    if let Some(shown_branch) = crate::session::display_branch(title, branch) {
                        spans.push(Span::styled(
                            format!(" [{}]", shown_branch),
                            Style::default().fg(self.theme.text_accent),
                        ));
                    }

                    if let Some(pr_num) = pr_number {
                        if self.invert_pr_label_color {
                            // Pre-pill behavior: colored text on default bg.
                            let badge_color = pr_colors::pr_badge_color(
                                self.theme,
                                *pr_state,
                                *pr_merged,
                                *pr_draft,
                                pr_labels,
                                self.review_labels,
                            );
                            spans.push(Span::styled(
                                format!(" PR #{}", pr_num),
                                Style::default().fg(badge_color),
                            ));
                        } else {
                            // Pill: non-colored separator space, then a pill
                            // with internal padding, colored bg, and bold
                            // contrast text so it stands out from the row.
                            let pill_bg = pr_colors::pr_pill_bg_color(
                                self.theme,
                                *pr_state,
                                *pr_merged,
                                *pr_draft,
                                pr_labels,
                                self.review_labels,
                            );
                            spans.push(Span::raw(" "));
                            spans.push(Span::styled(
                                format!(" PR #{} ", pr_num),
                                Style::default()
                                    .bg(pill_bg)
                                    .fg(self.theme.pr_pill_text)
                                    .add_modifier(Modifier::BOLD),
                            ));
                        }
                    }

                    if show_program {
                        spans.push(Span::raw(" "));
                        spans.push(Span::styled(
                            format!("({})", program_name(program)),
                            Style::default().fg(self.theme.text_secondary),
                        ));
                    }

                    let line = Line::from(spans);

                    ListItem::new(line)
                }
                SessionListItem::SectionHeader {
                    name,
                    count,
                    collapsed,
                } => {
                    let twistie = if *collapsed { "▸ " } else { "▾ " };
                    let line = Line::from(vec![
                        Span::raw(" "),
                        Span::styled(twistie, Style::default().fg(self.theme.text_secondary)),
                        Span::styled(
                            name.clone(),
                            Style::default()
                                .fg(self.theme.text_accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" ({})", count),
                            Style::default().fg(self.theme.text_secondary),
                        ),
                    ]);
                    ListItem::new(line)
                }
            })
            .collect()
    }
}

/// Scan buffer cells in a row for a matching text string, return starting X position.
pub(super) fn find_text_in_row(
    buf: &Buffer,
    y: u16,
    x_start: u16,
    x_end: u16,
    needle: &str,
) -> Option<u16> {
    let chars: Vec<char> = needle.chars().collect();
    if chars.is_empty() {
        return None;
    }

    let width = (x_end - x_start) as usize;
    if width < chars.len() {
        return None;
    }

    // Collect symbols from buffer cells in this row
    let mut row_chars: Vec<(u16, char)> = Vec::new();
    for x in x_start..x_end {
        let cell = &buf[(x, y)];
        let sym = cell.symbol();
        for c in sym.chars() {
            row_chars.push((x, c));
        }
    }

    // Search for needle in row_chars
    'outer: for i in 0..row_chars.len().saturating_sub(chars.len() - 1) {
        for (j, &needle_char) in chars.iter().enumerate() {
            if row_chars[i + j].1 != needle_char {
                continue 'outer;
            }
        }
        return Some(row_chars[i].0);
    }

    None
}

/// Post-process buffer to wrap PR badge text in OSC 8 hyperlink escape sequences.
///
/// Uses 2-char chunking to work around terminal width calculation issues,
/// following ratatui's official hyperlink example pattern.
pub(super) fn inject_pr_hyperlinks(
    list_area: Rect,
    buf: &mut Buffer,
    pr_data: &[Option<(u32, String)>],
    state: &ListState,
) {
    let offset = state.offset();
    let visible_rows = list_area.height as usize;

    for row in 0..visible_rows {
        let item_idx = offset + row;
        if item_idx >= pr_data.len() {
            break;
        }

        let Some((pr_num, ref url)) = pr_data[item_idx] else {
            continue;
        };

        let y = list_area.y + row as u16;
        let needle = format!("PR #{}", pr_num);

        let Some(start_x) =
            find_text_in_row(buf, y, list_area.x, list_area.x + list_area.width, &needle)
        else {
            continue;
        };

        // Apply OSC 8 hyperlink via 2-char chunking
        let osc_open = format!("\x1B]8;;{}\x07", url);
        let osc_close = "\x1B]8;;\x07";

        let needle_chars: Vec<char> = needle.chars().collect();
        let mut char_idx = 0;

        while char_idx < needle_chars.len() {
            let x = start_x + char_idx as u16;
            if x >= list_area.x + list_area.width {
                break;
            }

            // Collect up to 2 characters for this chunk
            let mut chunk_end = (char_idx + 2).min(needle_chars.len());
            let mut chunk: String = needle_chars[char_idx..chunk_end].iter().collect();
            let mut chunk_len = chunk_end - char_idx;

            // If this would be a trailing 1-char chunk, extend it with the
            // following cell's first character so the chunk is always 2-char.
            // Reason: the OSC 8 escapes balloon the cell's reported symbol
            // width far beyond 1, which sets ratatui's `to_skip` and blocks
            // the very next cell from emitting — leaving a stale 1-col gap
            // in the highlight when the row is selected.
            if chunk_len == 1 && x + 1 < list_area.x + list_area.width {
                let next_char = buf[(x + 1, y)].symbol().chars().next().unwrap_or(' ');
                chunk.push(next_char);
                chunk_end += 1; // consume the borrowed cell from the loop's POV
                chunk_len = 2;
            }

            buf[(x, y)].set_symbol(&format!("{}{}{}", osc_open, chunk, osc_close));

            // If we packed 2 chars into one cell, mark the next cell as a
            // wide-char continuation so the renderer skips it entirely.
            if chunk_len == 2 && x + 1 < list_area.x + list_area.width {
                buf[(x + 1, y)].set_skip(true);
            }

            char_idx = chunk_end;
        }
    }
}
