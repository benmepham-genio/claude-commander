//! Selection and navigation: update selection, scroll dispatch, session number jumping.

use super::*;

/// Maximum delay between two same-row left clicks for them to count as a
/// double-click that triggers `UserCommand::Select`.
pub(super) const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

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
                SessionListItem::SectionHeader { .. } => {
                    self.ui_state.selected_session_id = None;
                    self.ui_state.selected_project_id = None;
                }
                SessionListItem::Spacer => {}
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

    /// Map a mouse `(col, row)` in absolute terminal coordinates to a row in
    /// the session list. Returns `None` if the position is outside the list
    /// area or maps past the last rendered item.
    pub(super) fn list_index_at(&self, col: u16, row: u16) -> Option<usize> {
        list_index_at(
            col,
            row,
            self.ui_state.terminal_size,
            self.ui_state.left_pane_pct,
            self.ui_state.list_state.list_state.offset(),
            self.ui_state.list_items.len(),
        )
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

/// Pure mapping from absolute mouse coordinates to a list item index.
///
/// Mirrors the layout in `App::render` (see `render.rs`): the content area
/// inset by 1 on each side and 3 on the bottom (for status bar), split
/// horizontally by `left_pane_pct`, then the left column is split vertically
/// into a 1-row heading and the list below it. The list itself has no
/// border, so the list's top-left maps directly to item index `offset`.
pub(super) fn list_index_at(
    col: u16,
    row: u16,
    terminal_size: Rect,
    left_pane_pct: u16,
    offset: usize,
    item_count: usize,
) -> Option<usize> {
    if terminal_size.width == 0 || terminal_size.height == 0 {
        return None;
    }

    let content_area = Rect {
        x: terminal_size.x + 1,
        y: terminal_size.y + 1,
        width: terminal_size.width.saturating_sub(2),
        height: terminal_size.height.saturating_sub(3),
    };

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(left_pane_pct),
            Constraint::Percentage(100 - left_pane_pct),
        ])
        .split(content_area);
    let left = main_chunks[0];

    // Left pane: 1-line heading then the list. The list area starts at
    // `left.y + 1` and runs to `left.bottom()`.
    let list_y = left.y.checked_add(1)?;
    if col < left.x || col >= left.right() {
        return None;
    }
    if row < list_y || row >= left.bottom() {
        return None;
    }

    let visible = (row - list_y) as usize;
    let idx = offset.checked_add(visible)?;
    if idx >= item_count {
        return None;
    }
    Some(idx)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A typical 80x24 terminal split 30/70 with the standard 1-cell margins
    /// — used as a fixture for the `list_index_at` tests below.
    fn term() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    /// Returns the rendered list rect for the 80x24 / 30% fixture so tests
    /// can compute valid click positions without duplicating layout math.
    fn list_rect() -> Rect {
        let size = term();
        let content_area = Rect {
            x: size.x + 1,
            y: size.y + 1,
            width: size.width.saturating_sub(2),
            height: size.height.saturating_sub(3),
        };
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(content_area);
        let left = chunks[0];
        Rect::new(left.x, left.y + 1, left.width, left.height - 1)
    }

    #[test]
    fn list_index_at_first_row_with_no_offset_is_zero() {
        let lr = list_rect();
        assert_eq!(list_index_at(lr.x, lr.y, term(), 30, 0, 10), Some(0));
    }

    #[test]
    fn list_index_at_adds_scroll_offset() {
        let lr = list_rect();
        // Second visible row, list scrolled by 5 → item index 6
        assert_eq!(list_index_at(lr.x, lr.y + 1, term(), 30, 5, 20), Some(6));
    }

    #[test]
    fn list_index_at_returns_none_on_heading_row() {
        // The heading row sits at `list_rect().y - 1` (the first row of the
        // left pane, before the list). A click there must not select a row.
        let lr = list_rect();
        assert_eq!(list_index_at(lr.x, lr.y - 1, term(), 30, 0, 10), None);
    }

    #[test]
    fn list_index_at_returns_none_beyond_item_count() {
        let lr = list_rect();
        // Click far enough down that the item index exceeds item_count.
        assert_eq!(list_index_at(lr.x, lr.y, term(), 30, 0, 0), None);
        assert_eq!(list_index_at(lr.x, lr.y + 5, term(), 30, 0, 3), None);
    }

    #[test]
    fn list_index_at_returns_none_outside_left_pane() {
        let lr = list_rect();
        // Click in the right pane — should map to nothing in the list.
        let right_col = lr.right() + 1;
        assert_eq!(list_index_at(right_col, lr.y, term(), 30, 0, 10), None);
    }

    #[test]
    fn list_index_at_returns_none_below_content_area() {
        let lr = list_rect();
        // Click on the status-bar row at the bottom of the terminal.
        let below = term().bottom() - 1;
        assert!(below >= lr.bottom());
        assert_eq!(list_index_at(lr.x, below, term(), 30, 0, 100), None);
    }

    #[test]
    fn list_index_at_zero_size_terminal_returns_none() {
        let size = Rect::new(0, 0, 0, 0);
        assert_eq!(list_index_at(0, 0, size, 30, 0, 10), None);
    }
}
