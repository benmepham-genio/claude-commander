//! Selection and navigation: update selection, scroll dispatch, session number jumping.

use super::*;

impl App {
    /// Update selection tracking based on list position
    pub(super) fn update_selection(&mut self) {
        let old_session = self.ui_state.selected_session_id;
        let was_on_project = old_session.is_none() && self.ui_state.selected_project_id.is_some();

        if let Some(idx) = self.ui_state.list_state.selected()
            && let Some(item) = self.ui_state.list_items.get(idx)
        {
            match item {
                SessionListItem::Project { id, .. } => {
                    self.ui_state.selected_project_id = Some(*id);
                    self.ui_state.selected_session_id = None;
                }
                SessionListItem::Worktree { id, project_id, .. } => {
                    self.ui_state.selected_session_id = Some(*id);
                    self.ui_state.selected_project_id = Some(*project_id);
                }
                SessionListItem::SectionHeader { .. } | SessionListItem::Spacer => {}
            }
        }

        let now_on_project = self.ui_state.selected_session_id.is_none()
            && self.ui_state.selected_project_id.is_some();

        // Auto-switch pane when transitioning between project and session
        if now_on_project && !was_on_project {
            // Transitioning to a project: Preview → Shell
            if self.ui_state.right_pane_view == RightPaneView::Preview {
                self.ui_state.right_pane_view = RightPaneView::Shell;
                self.ui_state.clear_right_pane = true;
            }
        } else if !now_on_project && was_on_project {
            // Transitioning to a session: Shell → Preview
            if self.ui_state.right_pane_view == RightPaneView::Shell {
                self.ui_state.right_pane_view = RightPaneView::Preview;
                self.ui_state.clear_right_pane = true;
            }
        }

        // Fetch info pane data if applicable
        self.spawn_info_fetch();
    }

    /// Get mutable reference to the active pane's scroll state
    pub(super) fn active_pane_state(&mut self) -> &mut PreviewState {
        match self.ui_state.right_pane_view {
            RightPaneView::Preview => &mut self.ui_state.preview_state,
            RightPaneView::Info => &mut self.ui_state.info_state,
            RightPaneView::Shell => &mut self.ui_state.shell_state,
        }
    }

    /// Scroll the pane under the given mouse column position
    pub(super) fn scroll_pane_at(&mut self, col: u16, direction: ScrollDirection) {
        let size = self.ui_state.terminal_size;
        if size.width == 0 || size.height == 0 {
            return;
        }

        // Recompute the same content_area as render()
        let content_area = Rect {
            x: size.x + 1,
            y: size.y + 1,
            width: size.width.saturating_sub(2),
            height: size.height.saturating_sub(3),
        };

        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(self.ui_state.left_pane_pct),
                Constraint::Percentage(100 - self.ui_state.left_pane_pct),
            ])
            .split(content_area);

        let lines_per_tick: u16 = 3;

        if col < main_chunks[0].right() {
            // Left pane: scroll the session list selection
            match direction {
                ScrollDirection::Up => self.ui_state.list_state.previous(),
                ScrollDirection::Down => self.ui_state.list_state.next(),
            }
            self.update_selection();
        } else {
            // Right pane: scroll content
            match direction {
                ScrollDirection::Up => self.active_pane_state().scroll_up(lines_per_tick),
                ScrollDirection::Down => self.active_pane_state().scroll_down(lines_per_tick),
            }
        }
    }

    /// Jump the selection to the session with the given 1-based number,
    /// update the selection state, and refresh the preview pane.
    /// Does nothing if the number is out of range.
    /// Numbering matches `TreeList::to_list_items` — the Nth `Worktree` variant.
    pub(super) fn jump_to_session_number(&mut self, number: usize) {
        if let Some(idx) = session_number_to_list_index(&self.ui_state.list_items, number) {
            self.ui_state.list_state.list_state.select(Some(idx));
            self.update_selection();
            self.ui_state.preview_update_spawned_at = None;
            self.spawn_preview_update();
        }
    }

    /// Check if a project (not a session) is currently selected
    pub(super) fn is_project_selected(&self) -> bool {
        self.ui_state.selected_session_id.is_none() && self.ui_state.selected_project_id.is_some()
    }
}

/// Map a 1-based session number to its index in the flat list_items vec.
/// Returns None if the number is out of range.
pub(super) fn session_number_to_list_index(
    items: &[SessionListItem],
    number: usize,
) -> Option<usize> {
    let mut count = 0usize;
    for (idx, item) in items.iter().enumerate() {
        if matches!(item, SessionListItem::Worktree { .. }) {
            count += 1;
            if count == number {
                return Some(idx);
            }
        }
    }
    None
}
