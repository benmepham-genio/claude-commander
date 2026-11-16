//! Selection and navigation: update selection, scroll dispatch, session number jumping.

use super::*;

impl App {
    /// The backend that owns session `id`, by scanning cached snapshots. Session
    /// ids are globally unique (UUIDs), so at most one backend matches; defaults
    /// to the local backend when none does (e.g. a stale ref).
    pub(super) fn backend_of_session(&self, id: SessionId) -> BackendId {
        self.backends
            .iter()
            .find(|h| h.view.snapshot.sessions.iter().any(|s| s.session_id == id))
            .map(|h| h.id)
            .unwrap_or(LOCAL_BACKEND_ID)
    }

    /// The backend that owns project `id`, by scanning cached snapshots.
    /// Defaults to the local backend when none matches.
    pub(super) fn backend_of_project(&self, id: ProjectId) -> BackendId {
        self.backends
            .iter()
            .find(|h| h.view.snapshot.projects.iter().any(|p| p.id == id))
            .map(|h| h.id)
            .unwrap_or(LOCAL_BACKEND_ID)
    }

    /// Whether backend `id`'s cached connection is `Connected`. A missing
    /// backend (stale id) counts as connected so callers don't wrongly gate.
    fn backend_is_connected(&self, id: BackendId) -> bool {
        self.backend(id)
            .map(|h| {
                matches!(
                    h.view.connection,
                    claude_commander_core::backend::ConnectionState::Connected
                )
            })
            .unwrap_or(true)
    }

    /// Capabilities of backend `id`. A missing backend (stale id) falls back to
    /// the local (all-on) set so callers don't wrongly gate.
    fn backend_capabilities(
        &self,
        id: BackendId,
    ) -> claude_commander_core::backend::BackendCapabilities {
        self.backend(id)
            .map(|h| h.backend.capabilities())
            .unwrap_or(claude_commander_core::backend::BackendCapabilities::LOCAL)
    }

    /// Move the selection down one row (board row / list item).
    pub(super) fn nav_down(&mut self) {
        if self.ui_state.view_mode.is_board() {
            self.ui_state.board_state.next_row();
        } else {
            self.ui_state.list_state.next();
        }
    }

    /// Move the selection up one row.
    pub(super) fn nav_up(&mut self) {
        if self.ui_state.view_mode.is_board() {
            self.ui_state.board_state.previous_row();
        } else {
            self.ui_state.list_state.previous();
        }
    }

    /// Move right: board switches column; a list view is one-dimensional (no-op).
    pub(super) fn nav_right(&mut self) {
        if self.ui_state.view_mode.is_board() {
            self.ui_state.board_state.next_column();
        }
    }

    /// Move left: board switches column; a list view is one-dimensional (no-op).
    pub(super) fn nav_left(&mut self) {
        if self.ui_state.view_mode.is_board() {
            self.ui_state.board_state.previous_column();
        }
    }

    /// Next group: board switches column; list jumps to the next group header.
    pub(super) fn nav_next_group(&mut self) {
        if self.ui_state.view_mode.is_board() {
            self.ui_state.board_state.next_column();
        } else {
            self.ui_state.list_state.next_group();
        }
    }

    /// Previous group: board switches column; list jumps to the previous header.
    pub(super) fn nav_previous_group(&mut self) {
        if self.ui_state.view_mode.is_board() {
            self.ui_state.board_state.previous_column();
        } else {
            self.ui_state.list_state.previous_group();
        }
    }

    /// Move a screenful up or down: the board steps within the current column
    /// by the number of cards that column had visible on the last frame; a list
    /// view steps by its visible row count. Both keep one row of overlap.
    pub(super) fn nav_page(&mut self, down: bool) {
        if self.ui_state.view_mode.is_board() {
            let Some(col) = self.ui_state.board_state.selected_column() else {
                return;
            };
            let visible = self
                .ui_state
                .board_hit_regions
                .iter()
                .filter(|r| r.pos.col == col)
                .count();
            self.ui_state
                .board_state
                .page(page_rows(visible as u16), down);
        } else {
            let rows = page_rows(self.ui_state.main_list_height);
            self.ui_state.list_state.page(rows, down);
        }
    }

    /// Jump to the first row (board column top / list top).
    pub(super) fn nav_first(&mut self) {
        if self.ui_state.view_mode.is_board() {
            self.ui_state.board_state.select_first();
        } else {
            self.ui_state.list_state.select_first();
        }
    }

    /// Jump to the last row (board column bottom / list bottom).
    pub(super) fn nav_last(&mut self) {
        if self.ui_state.view_mode.is_board() {
            self.ui_state.board_state.select_last();
        } else {
            self.ui_state.list_state.select_last();
        }
    }

    /// Update the tracked session/project selection from the active view's cursor.
    ///
    /// On the board a sidebar row yields a project only; a card row yields both
    /// session and project; a landed-but-empty column (or no selection) clears
    /// both. This preserves every action gate — `RemoveProject` (project, no
    /// session) and project-shell attach both fall out of the sidebar case
    /// unchanged.
    pub(super) fn update_selection(&mut self) {
        let previous = (
            self.ui_state.selected_session_id,
            self.ui_state.selected_project_id,
        );
        // Read the raw (session, project) ids from whichever view is active.
        let (session, project) = if self.ui_state.view_mode.is_board() {
            match self.ui_state.board_state.selected() {
                Some(pos) => self.ui_state.board.ids_at(pos),
                None => (None, None),
            }
        } else {
            self.ui_state
                .list_state
                .selected()
                .and_then(|idx| self.ui_state.list_items.get(idx))
                .map(|item| match item {
                    SessionListItem::Worktree { id, project_id, .. } => {
                        (Some(*id), Some(*project_id))
                    }
                    SessionListItem::Project { id, .. } => (None, Some(*id)),
                    // A recent-session row resolves to the same session as its
                    // real row below; the backend is re-derived from the id.
                    SessionListItem::RecentSession {
                        session,
                        project_id,
                        ..
                    } => (Some(session.id), Some(*project_id)),
                    // Section/server headers, spacers and the recents header
                    // select nothing.
                    _ => (None, None),
                })
                .unwrap_or((None, None))
        };
        // Backend-qualify the selection so actions route to the owning machine,
        // and cache connection/capabilities for the sync `is_command_available`.
        match (session, project) {
            (Some(sid), pid) => {
                let backend = self.backend_of_session(sid);
                self.ui_state.selected_session_id = Some(SessionRef::new(backend, sid));
                self.ui_state.selected_project_id = pid.map(|p| (backend, p));
                self.ui_state.selected_backend_connected = self.backend_is_connected(backend);
                self.ui_state.selected_backend_capabilities = self.backend_capabilities(backend);
            }
            (None, Some(pid)) => {
                let backend = self.backend_of_project(pid);
                self.ui_state.selected_session_id = None;
                self.ui_state.selected_project_id = Some((backend, pid));
                self.ui_state.selected_backend_connected = self.backend_is_connected(backend);
                self.ui_state.selected_backend_capabilities = self.backend_capabilities(backend);
            }
            (None, None) => {
                self.ui_state.selected_session_id = None;
                self.ui_state.selected_project_id = None;
                self.ui_state.selected_backend_connected = true;
                self.ui_state.selected_backend_capabilities =
                    claude_commander_core::backend::BackendCapabilities::LOCAL;
            }
        }

        if previous
            != (
                self.ui_state.selected_session_id,
                self.ui_state.selected_project_id,
            )
        {
            if let Some(task) = self.ui_state.preview_task.take() {
                task.abort();
            }
            self.ui_state.preview_update_spawned_at = None;
            self.ui_state.preview_content.clear();
            self.ui_state.shell_content.clear();
            self.ui_state.diff_info =
                std::sync::Arc::new(claude_commander_core::git::DiffInfo::empty());
        }
        // Fetch info-modal data if applicable (gated on the Info modal being
        // open — `spawn_info_fetch` is a no-op otherwise).
        self.spawn_info_fetch();
    }

    /// Map a mouse `(col, row)` to a sidebar server heading's backend, if the
    /// click landed on one. Headings aren't selectable rows — a hit opens that
    /// server's Settings → Programs tab directly.
    pub(super) fn board_heading_at(
        &self,
        col: u16,
        row: u16,
    ) -> Option<claude_commander_core::backend::BackendId> {
        self.ui_state
            .board_heading_regions
            .iter()
            .find(|(rect, _)| {
                col >= rect.x
                    && col < rect.x + rect.width
                    && row >= rect.y
                    && row < rect.y + rect.height
            })
            .map(|(_, backend)| *backend)
    }

    /// Map a mouse `(col, row)` in absolute terminal coordinates to the board
    /// position of the row under it, using the hit regions recorded on the last
    /// render frame. Returns `None` when the click is not on any row.
    pub(super) fn board_pos_at(&self, col: u16, row: u16) -> Option<BoardPos> {
        self.ui_state
            .board_hit_regions
            .iter()
            .find(|r| {
                col >= r.rect.x
                    && col < r.rect.x + r.rect.width
                    && row >= r.rect.y
                    && row < r.rect.y + r.rect.height
            })
            .map(|r| r.pos)
    }

    /// Map a mouse `(col, row)` to a card action button, using the button hit
    /// regions from the last render frame. Checked *before* [`board_pos_at`] in
    /// click handling so a click on a button wins over the row region it sits
    /// within. Returns the card's board position and which button was hit.
    pub(super) fn board_button_at(
        &self,
        col: u16,
        row: u16,
    ) -> Option<(BoardPos, crate::widgets::board::CardButton)> {
        self.ui_state
            .board_button_regions
            .iter()
            .find(|r| {
                col >= r.rect.x
                    && col < r.rect.x + r.rect.width
                    && row >= r.rect.y
                    && row < r.rect.y + r.rect.height
            })
            .map(|r| (r.pos, r.button))
    }

    /// Whether the selection is a project row rather than a session.
    pub(super) fn is_project_selected(&self) -> bool {
        self.ui_state.is_project_selected()
    }

    /// Scroll state of the right pane's active tab, so wheel handling doesn't
    /// need to know which tab is showing.
    pub(super) fn active_pane_state(&mut self) -> &mut PreviewState {
        match self
            .ui_state
            .right_pane_view
            .effective(self.is_project_selected())
        {
            RightPaneView::Preview => &mut self.ui_state.preview_state,
            RightPaneView::Info => &mut self.ui_state.info_state,
            RightPaneView::Shell => &mut self.ui_state.shell_state,
        }
    }

    /// Move the list/pane divider by `delta` percentage points, clamped to
    /// [`MIN_LEFT_PANE_PCT`]..=[`MAX_LEFT_PANE_PCT`], and persist the result.
    /// A no-op in board view (no divider) and when already at the clamp, so a
    /// held key can't spam `tui.json` writes.
    pub(super) async fn resize_left_pane(&mut self, delta: i16) {
        if self.ui_state.view_mode.is_board() {
            return;
        }
        let next = (self.ui_state.left_pane_pct as i16 + delta)
            .clamp(MIN_LEFT_PANE_PCT as i16, MAX_LEFT_PANE_PCT as i16) as u16;
        if next == self.ui_state.left_pane_pct {
            return;
        }
        self.ui_state.left_pane_pct = next;
        self.tui_prefs.set_left_pane_pct(next).await;
    }

    /// Whether an absolute terminal column falls in the right pane, using the
    /// rect recorded at render time (so it matches what is actually on screen).
    /// Always false in board view, which records no right pane.
    pub(super) fn x_in_right_pane(&self, x: u16) -> bool {
        self.ui_state
            .right_pane_rect
            .is_some_and(|r| x >= r.x && x < r.right())
    }

    /// Route a wheel notch by the column it happened over.
    ///
    /// In a list view: over the session list it steps the selection, and over the
    /// right pane it scrolls that pane's content (three lines a notch, which also
    /// breaks follow-the-tail until the user wheels back to the bottom). On the
    /// board it moves the selection within the hovered column — one notch, one
    /// row.
    pub(super) fn scroll_pane_at(&mut self, x: u16, direction: ScrollDirection) {
        if !self.ui_state.view_mode.is_board() {
            if self.x_in_right_pane(x) {
                const LINES_PER_TICK: u16 = 3;
                match direction {
                    ScrollDirection::Up => self.active_pane_state().scroll_up(LINES_PER_TICK),
                    ScrollDirection::Down => self.active_pane_state().scroll_down(LINES_PER_TICK),
                }
            } else {
                match direction {
                    ScrollDirection::Up => self.nav_up(),
                    ScrollDirection::Down => self.nav_down(),
                }
            }
            return;
        }
        let Some(addr_col) = self
            .ui_state
            .board_column_rects
            .as_ref()
            .and_then(|rects| crate::widgets::board::layout::column_at_x(rects, x))
        else {
            return;
        };
        // Land in the hovered column (keeping the row where valid), then step.
        let row = self
            .ui_state
            .board_state
            .selected()
            .map(|p| p.row)
            .unwrap_or(0);
        self.ui_state
            .board_state
            .select_nearest(BoardPos { col: addr_col, row });
        match direction {
            ScrollDirection::Up => self.ui_state.board_state.previous_row(),
            ScrollDirection::Down => self.ui_state.board_state.next_row(),
        }
        self.update_selection();
    }

    /// Jump the selection to the session with the given 1-based number and
    /// refresh dependent state. Does nothing if the number is out of range.
    /// Numbering is column-major — the Nth `Worktree` row (see
    /// [`Board::pos_of_session_number`]).
    pub(super) fn jump_to_session_number(&mut self, number: usize) {
        if self.ui_state.view_mode.is_board() {
            if let Some(pos) = self.ui_state.board.pos_of_session_number(number) {
                self.ui_state.board_state.select(Some(pos));
                self.update_selection();
                self.ui_state.preview_update_spawned_at = None;
                self.spawn_preview_update();
            }
        } else if let Some(idx) = session_number_to_list_index(&self.ui_state.list_items, number) {
            self.ui_state.list_state.select(Some(idx));
            self.update_selection();
            self.ui_state.preview_update_spawned_at = None;
            self.spawn_preview_update();
        }
    }

    /// The section a new session should land in, given the current cursor.
    ///
    /// On the board it is the selected column's name (`None` for the sidebar).
    /// In the section list views it is the section header at or above the
    /// cursor (other list views render no headers, so `None`). The implicit
    /// "In Progress" catch-all stamps no override — sessions land there by
    /// default anyway.
    pub(super) fn target_section(&self) -> Option<String> {
        if self.ui_state.view_mode.is_board() {
            let pos = self.ui_state.board_state.selected()?;
            if pos.col == 0 {
                return None;
            }
            let column = self.ui_state.board.columns.get(pos.col - 1)?;
            (column.name != claude_commander_core::session::IN_PROGRESS)
                .then(|| column.name.clone())
        } else {
            let idx = self.ui_state.list_state.selected()?;
            section_at(&self.ui_state.list_items, idx)
        }
    }

    /// Move the board cursor to the card row for `session_id` and sync
    /// selection state. No-op (returns `false`) if the session has no row on the
    /// board — e.g. it was deleted.
    pub(super) fn select_session_in_tree(&mut self, session_id: SessionId) -> bool {
        if self.ui_state.view_mode.is_board() {
            match self.ui_state.board.position_of(session_id) {
                Some(pos) => {
                    self.ui_state.board_state.select(Some(pos));
                    self.update_selection();
                    true
                }
                None => false,
            }
        } else {
            match worktree_list_index(&self.ui_state.list_items, session_id) {
                Some(idx) => {
                    self.ui_state.list_state.select(Some(idx));
                    self.update_selection();
                    true
                }
                None => false,
            }
        }
    }

    /// Move the cursor onto `session_id`, expanding whatever the current view
    /// is hiding it behind. Returns false if it still has no row.
    ///
    /// The palette lists every session regardless of what the view shows, so a
    /// jump must not assume the target already has one: a collapsed section's
    /// sessions have *no* row at all (`build_section_grouped_items` skips the
    /// whole group), so [`Self::select_session_in_tree`] fails, the selection
    /// stays where it was, and the attach that follows lands on the previously
    /// selected session — the wrong one, silently.
    ///
    /// Only the section the target turns out to be in is left expanded; the
    /// others are re-collapsed, so a jump doesn't discard the rest of the
    /// user's layout.
    pub(super) async fn reveal_session_in_tree(&mut self, session_id: SessionId) -> bool {
        if self.select_session_in_tree(session_id) {
            return true;
        }
        // Collapsing is a list-view affordance; the board draws every column.
        if self.ui_state.view_mode.is_board() || self.ui_state.collapsed_sections.is_empty() {
            return false;
        }
        // A *list* rebuild keeps the cursor's row index, not the session it was
        // on: `set_selectable` clears the selection when that index is now out
        // of range or lands on a non-selectable row, and `update_selection`
        // re-derives `selected_session_id` from whatever row it does land on.
        // There is no list-side re-anchor by session id — `restore_selection`
        // has one but runs once at startup, and `rebuild_board_view`'s applies
        // to the board, which this function returned early for above. So
        // expanding, which inserts rows above the cursor, can silently drop the
        // selection, and the previous one has to be carried across by hand.
        let was_selected = self.ui_state.selected_session_id;
        let collapsed = std::mem::take(&mut self.ui_state.collapsed_sections);
        self.refresh_list_items().await;
        if !self.select_session_in_tree(session_id) {
            // Something other than a collapse is hiding it; put the layout and
            // the cursor back as they were.
            self.ui_state.collapsed_sections = collapsed;
            self.refresh_list_items().await;
            if let Some(prev) = was_selected {
                self.select_session_in_tree(prev.id);
            }
            return false;
        }
        // Which section had to open: the header above the cursor we just moved
        // onto. `section_header_at`, not `target_section` — the latter answers
        // `None` for the In Progress catch-all, which is an ordinary member of
        // `collapsed_sections` and the common case here. A `None` (a view with
        // no headers at all) can't have hidden the row in the first place, so
        // it leaves everything expanded rather than guessing.
        let keep = self
            .ui_state
            .list_state
            .selected()
            .and_then(|idx| section_header_at(&self.ui_state.list_items, idx));
        if let Some(keep) = keep {
            self.ui_state.collapsed_sections =
                collapsed.into_iter().filter(|n| *n != keep).collect();
            self.refresh_list_items().await;
            // Load-bearing for the same reason: re-collapsing the other
            // sections shifts every row below each of them, so the cursor this
            // rebuild leaves behind is wherever the old index now points. Put
            // it on the target explicitly.
            self.select_session_in_tree(session_id);
        }
        true
    }

    /// Move the board cursor to the sidebar row for `project_id` and sync
    /// selection state. No-op if the project has no sidebar entry. Used after
    /// adding a project (which has no sessions yet, so it appears only in the
    /// sidebar, never as a card).
    pub(super) fn select_project_in_sidebar(&mut self, project_id: ProjectId) {
        if let Some(row) = self.ui_state.board.sidebar_row_of(project_id) {
            self.ui_state
                .board_state
                .select(Some(BoardPos { col: 0, row }));
            self.update_selection();
        }
    }

    /// Resolve a tmux session name (primary or paired shell) to its session and
    /// focus it on the board. Used on the way out of an attach so the board
    /// lands on the session the user just left — which, after the in-session
    /// switcher, may differ from the one they entered. Prefers the attached
    /// `backend`'s view before scanning the rest, since tmux session names can
    /// collide across machines. No-op if the session no longer exists.
    pub(super) async fn focus_session_in_tree(&mut self, backend: BackendId, tmux_name: &str) {
        // A shell pane's tmux session is named `<primary>-sh`; match either.
        let primary = tmux_name.strip_suffix("-sh").unwrap_or(tmux_name);
        let matches = |s: &claude_commander_core::api::SessionInfo| {
            s.tmux_session_name == primary || s.tmux_session_name == tmux_name
        };
        let session_id = self
            .view_for(backend)
            .snapshot
            .sessions
            .iter()
            .find(|s| matches(s))
            .map(|s| s.session_id)
            .or_else(|| {
                self.backends.iter().find_map(|h| {
                    h.view
                        .snapshot
                        .sessions
                        .iter()
                        .find(|s| matches(s))
                        .map(|s| s.session_id)
                })
            });
        if let Some(id) = session_id
            && self.select_session_in_tree(id)
        {
            self.ui_state.preview_update_spawned_at = None;
            self.spawn_preview_update();
        }
    }
}

/// The list index of the `number`-th (1-based) `Worktree` row, counting only
/// worktree rows (headers/spacers don't advance the count). Column-major
/// numbering matches the board's `pos_of_session_number`.
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

/// The list index of the `Worktree` row for `session_id`, if present.
pub(super) fn worktree_list_index(
    items: &[SessionListItem],
    session_id: SessionId,
) -> Option<usize> {
    items
        .iter()
        .position(|item| matches!(item, SessionListItem::Worktree { id, .. } if *id == session_id))
}

/// How many rows a page jump moves the cursor, given the visible row count of
/// the scrolling area (list rows, or cards in the current board column): one
/// screenful less a row of overlap, so the row you were on stays visible at the
/// edge of the new view. Never zero — before the first render has recorded a
/// height it degrades to a single row.
pub(super) fn page_rows(visible: u16) -> usize {
    visible.saturating_sub(1).max(1) as usize
}

/// Name of the section containing the list row at `idx` — the nearest
/// `SectionHeader` at or above it. Returns `None` when the row sits above any
/// header (non-sectioned view modes render no headers) or under the implicit
/// "In Progress" catch-all, where new sessions land by default anyway.
pub(super) fn section_at(items: &[SessionListItem], idx: usize) -> Option<String> {
    section_header_at(items, idx).filter(|name| name != claude_commander_core::session::IN_PROGRESS)
}

/// The name of the nearest section header at or above `idx`, catch-all
/// included.
///
/// [`section_at`] is the *new-session* question ("which section should this row
/// stamp a new session into"), and answers `None` for the In Progress catch-all
/// because a session there needs no override. Anything asking "which section is
/// this row displayed under" — collapse/expand, which treats In Progress as an
/// ordinary member of `collapsed_sections` — must use this one instead.
pub(super) fn section_header_at(items: &[SessionListItem], idx: usize) -> Option<String> {
    items.get(..=idx)?.iter().rev().find_map(|item| match item {
        SessionListItem::SectionHeader { name, .. } => Some(name.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str) -> SessionListItem {
        SessionListItem::SectionHeader {
            name: name.to_string(),
            count: 1,
            collapsed: false,
            max_sessions: None,
        }
    }

    fn project_row() -> SessionListItem {
        SessionListItem::Project {
            id: claude_commander_core::session::ProjectId::new(),
            name: "proj".into(),
            repo_path: std::path::PathBuf::from("/dev/null/unused"),
            main_branch: "main".into(),
            worktree_count: 1,
            nested: true,
        }
    }

    #[test]
    fn page_rows_keeps_one_row_of_overlap() {
        assert_eq!(page_rows(20), 19);
        // A one-row list still moves, and so does a height not yet recorded.
        assert_eq!(page_rows(1), 1);
        assert_eq!(page_rows(0), 1);
    }

    #[test]
    fn section_at_finds_nearest_header_above() {
        let items = vec![
            header("Awaiting Review"),
            project_row(),
            SessionListItem::Spacer,
            header("Self Review"),
            project_row(),
        ];
        assert_eq!(section_at(&items, 4).as_deref(), Some("Self Review"));
        assert_eq!(section_at(&items, 1).as_deref(), Some("Awaiting Review"));
    }

    #[test]
    fn section_at_on_header_row_returns_that_section() {
        let items = vec![header("Self Review"), project_row()];
        assert_eq!(section_at(&items, 0).as_deref(), Some("Self Review"));
    }

    #[test]
    fn section_at_in_progress_catchall_is_none() {
        let items = vec![claude_commander_core::session::IN_PROGRESS, "Self Review"]
            .into_iter()
            .map(header)
            .collect::<Vec<_>>();
        assert_eq!(section_at(&items, 0), None);
    }

    #[test]
    fn section_at_without_headers_is_none() {
        // Non-sectioned view modes render no SectionHeader rows at all.
        let items = vec![project_row(), project_row()];
        assert_eq!(section_at(&items, 1), None);
    }

    #[test]
    fn section_at_out_of_bounds_is_none() {
        let items = vec![header("Self Review")];
        assert_eq!(section_at(&items, 5), None);
    }
}
