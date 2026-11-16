//! Rendering: top bar, full-screen kanban board, status bar.

use super::*;
use crate::EmbeddedServerStatus;
use crate::hotkey::ActionButton;

/// Split a list view's content area into (session list, right pane).
///
/// Pure, and the single definition of that geometry: `render` lays the panes out
/// with it and the wheel hit-test reads the recorded right-pane rect it
/// produced, so the two can't drift. `left_pane_pct` is a percentage of
/// `content.width`, clamped so both panes keep at least one column even at an
/// extreme width.
pub(super) fn split_list_view(content: Rect, left_pane_pct: u16) -> (Rect, Rect) {
    let pct = left_pane_pct.clamp(MIN_LEFT_PANE_PCT, MAX_LEFT_PANE_PCT);
    let left_width = ((content.width as u32 * pct as u32) / 100) as u16;
    // Never starve either side: below 2 columns there is nothing to split, so the
    // list keeps the whole area and the right pane renders empty.
    let left_width = if content.width < 2 {
        content.width
    } else {
        left_width.clamp(1, content.width - 1)
    };
    let left = Rect {
        width: left_width,
        ..content
    };
    let right = Rect {
        x: content.x.saturating_add(left_width),
        width: content.width.saturating_sub(left_width),
        ..content
    };
    (left, right)
}

/// Build the footer commander chip label, or `None` when the commander is not
/// running (the chip is hidden then). When running, the label is `● Commander`,
/// refined with the live agent state (`· working` / `· waiting` / `· idle`) once
/// the background poll reports it. `Unknown`/unpolled states show the bare chip.
pub(super) fn commander_chip_label(
    running: bool,
    agent_state: Option<AgentState>,
) -> Option<String> {
    if !running {
        return None;
    }
    let suffix = match agent_state {
        Some(AgentState::Working) => " \u{00b7} working",
        Some(AgentState::WaitingForInput) => " \u{00b7} waiting",
        Some(AgentState::Idle) => " \u{00b7} idle",
        Some(AgentState::Unknown) | None => "",
    };
    Some(format!("\u{25cf} Commander{suffix}"))
}

/// Build the footer chip for the in-process HTTP server, or `None` when this run
/// was never asked to serve (the chip is hidden then).
///
/// Two shapes, because the two outcomes need different reactions: `⇅ 7878` says
/// clients can reach this machine, while `⇅ server unavailable` says they cannot.
/// The bind failure is worth a chip rather than only a startup toast — a toast is
/// invisible from inside an attached pane, and the operator may not look at the
/// terminal until long after launch.
///
/// The label is deliberately short in both cases. The status bar's left zone is
/// laid out as `Length(left_width)`, so it takes its width out of the action
/// buttons' `Fill(1)`: putting the bind error (`could not bind 127.0.0.1:7878:
/// Address already in use`) in here would evict every button on an 80-column
/// terminal. The reason goes to the log and to a startup toast instead.
pub(super) fn server_chip_label(status: Option<&EmbeddedServerStatus>) -> Option<String> {
    match status? {
        EmbeddedServerStatus::Listening { url, .. } => {
            // The port is the part that varies and the part a client needs; the
            // scheme and host would just cost columns the buttons want.
            let port = url.rsplit(':').next().unwrap_or(url);
            Some(format!("\u{21c5} {port}"))
        }
        EmbeddedServerStatus::Failed { .. } => Some("\u{21c5} server unavailable".to_string()),
    }
}

/// Split a list pane into its one-row heading bar and the rows below — the
/// shape shared by the main view's session list and the review view's file
/// list (neither is boxed; the heading bar names the pane instead).
pub(super) fn split_heading_bar(area: Rect) -> (Rect, Rect) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    (parts[0], parts[1])
}

impl App {
    /// Return the border type based on config: rounded or plain (square).
    pub(super) fn border_type(&self) -> BorderType {
        if self.config.rounded_borders {
            BorderType::Rounded
        } else {
            BorderType::Plain
        }
    }

    /// Per-project (border, session-title) colours, keyed by project id and
    /// assigned by the board's name-sorted project order (same determinism the
    /// old tree list used). Fed to [`BoardWidget::project_colors`].
    pub(super) fn project_color_map(&self) -> HashMap<ProjectId, (Color, Color)> {
        self.ui_state
            .board
            .projects
            .iter()
            .enumerate()
            .map(|(i, e)| (e.project_id, self.theme.project_color(i)))
            .collect()
    }

    /// Recompute the per-project colour cache from the current board order and
    /// theme. Shared by `refresh_list_items` (board order can change) and
    /// `reload_theme` (the palette can change), so the cache never lags behind
    /// either input.
    pub(super) fn rebuild_project_colors(&mut self) {
        self.ui_state.project_colors = self.project_color_map();
    }

    /// Rebuild the active theme from the current config — the active
    /// workspace's theme once there are workspaces to be in
    /// ([`Self::theme_workspace`]), else the usual `[theme]` — then refresh the
    /// derived project-colour cache. Call after any mutation of
    /// `self.config.theme` or `self.config.workspace_themes` (settings apply,
    /// config hot-reload, a workspace rename or delete): the cache is otherwise
    /// only rebuilt in `refresh_list_items`, so card border/title colours would
    /// show the old theme until an unrelated tick refreshed them. A workspace
    /// *switch* needs no call: `refresh_list_items` notices the change
    /// ([`Self::sync_workspace_theme`]).
    pub(super) fn reload_theme(&mut self) {
        let workspace = self.theme_workspace();
        self.theme = match &workspace {
            Some(ws) => crate::theme::theme_for_workspace(&self.config, ws.as_deref()),
            None => Theme::from_overrides(&self.config.theme),
        };
        self.ui_state.theme_workspace = workspace;
        self.rebuild_project_colors();
    }

    /// Render the UI
    pub(super) fn render(&mut self, frame: &mut Frame) {
        let size = frame.area();
        self.ui_state.terminal_size = size;

        // A full-screen modal (review / conversation) draws over the whole
        // frame. When we leave one, its cells would otherwise linger under the
        // board, so force a full in-memory repaint on the transition. We use the
        // `Clear` widget (a pure cell reset) rather than `Terminal::clear()`,
        // which since ratatui 0.30 reads the cursor from stdin — a blocking read
        // that races the background input reader and kills the loop.
        // `open_review` rather than `Modal::ReviewDiff`: a review suspended
        // under the session switcher is still on screen, and still full-screen.
        let is_fullscreen = self.open_review().is_some()
            || matches!(self.ui_state.modal, Modal::Conversation { .. });
        let leaving_fullscreen = self.ui_state.prev_fullscreen && !is_fullscreen;
        self.ui_state.prev_fullscreen = is_fullscreen;
        if std::mem::take(&mut self.ui_state.force_clear) || leaving_fullscreen {
            frame.render_widget(Clear, size);
        }

        // The review-diff view is a full-screen takeover: it owns the whole
        // frame (including the bottom row, where it draws its own status bar).
        // It keeps that frame while the session switcher is open over it — the
        // palette suspends the review rather than replacing it, so it is drawn
        // underneath and the diff doesn't blink away to the board and back.
        if self.open_review().is_some() {
            self.ui_state.review_body_rect = Some(super::review::review_body_inner_rect(size));
            let (file_pane, file_list) = super::review::review_file_list_rects(size);
            self.ui_state.review_file_pane_rect = Some(file_pane);
            self.ui_state.review_file_list_rect = Some(file_list);
            let review_buttons = match self.open_review() {
                Some(state) => self.render_review_modal(frame, size, state),
                None => Vec::new(),
            };
            self.ui_state.review_buttons = review_buttons;
            // The suspended case: draw the palette on top of the diff.
            // `render_modal` then publishes the palette's own `modal_list_rect`
            // and clears `review_body_rect` (it is `None` for any non-review
            // modal), so no mouse handler maps a click onto the covered diff.
            // `review_file_list_rect` and `review_buttons` are left as drawn,
            // and stay unreachable only because every handler that reads them
            // gates on `Modal::ReviewDiff` first — check that before adding a
            // new one.
            if matches!(self.ui_state.modal, Modal::QuickSwitch { .. }) {
                self.render_modal(frame, size);
            }
            return;
        }

        // The conversation overlay is also a full-screen takeover.
        if let Modal::Conversation { input, scroll } = &self.ui_state.modal {
            self.render_conversation_modal(frame, size, input, *scroll);
            return;
        }

        // Content area = everything above the 1-row status bar.
        let content = Rect {
            x: size.x,
            y: size.y,
            width: size.width,
            height: size.height.saturating_sub(1),
        };

        if self.ui_state.view_mode.is_board() {
            // Board: 1-line top bar, then the board.
            let top_bar = Rect {
                height: 1,
                ..content
            };
            let board_area = Rect {
                y: content.y.saturating_add(1),
                height: content.height.saturating_sub(1),
                ..content
            };
            // The board is a full-screen takeover with no right pane; drop the
            // rect so a stale wheel event can't scroll an invisible pane.
            self.ui_state.right_pane_rect = None;
            self.render_top_bar(frame, top_bar);
            self.render_board(frame, board_area);
            self.render_modal(frame, board_area);
        } else {
            // List view: session list on the left, live pane on the right.
            let (left, right) = split_list_view(content, self.ui_state.left_pane_pct);
            self.ui_state.right_pane_rect = Some(right);
            self.render_session_list(frame, left);
            self.render_right_pane(frame, right);
            self.render_modal(frame, content);
        }

        // Render status bar at the very bottom of the screen. It returns the
        // clickable action-button regions it drew, recorded for hit-testing.
        self.ui_state.action_buttons = self.render_status_bar(frame, size);
    }

    /// Render the 1-line top bar: app title on the left, session/project counts
    /// on the right, styled like the status bar.
    fn render_top_bar(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let style = self.theme.status_bar();

        let sessions = self.ui_state.board.worktree_count();
        let projects = self.ui_state.board.projects.len();

        // Name the workspace once there is more than one to be in.
        let title_text = match self.visible_active_workspace() {
            Some(ws) => format!(" Claude Commander \u{00b7} {}", ws.label),
            None => " Claude Commander".to_string(),
        };
        // When a project filter is active, name it and how to clear it; the
        // session count then reflects the filtered card count.
        let counts_text = match self.ui_state.board_filter.and_then(|pid| {
            self.ui_state
                .board
                .projects
                .iter()
                .find(|p| p.project_id == pid)
                .map(|p| p.name.clone())
        }) {
            Some(name) => format!(
                "filtered: {name} (Esc to clear) \u{00b7} {sessions} session{} ",
                if sessions == 1 { "" } else { "s" },
            ),
            None => format!(
                "{sessions} session{} \u{00b7} {projects} project{} ",
                if sessions == 1 { "" } else { "s" },
                if projects == 1 { "" } else { "s" },
            ),
        };

        // Render the title and the right-aligned counts as a single Line/
        // Paragraph. Drawing them as two overlapping Paragraphs stripped the
        // title's accent fg, because ratatui's `Paragraph::render` calls
        // `buf.set_style(area, style)` first — so the second (counts) paragraph
        // reset the title cells to the plain status-bar style. One paragraph
        // fills the bar background once, then paints each span's own style.
        let pad = (area.width as usize)
            .saturating_sub(title_text.chars().count() + counts_text.chars().count());
        let line = Line::from(vec![
            Span::styled(
                title_text,
                style
                    .fg(self.theme.status_bar_accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".repeat(pad), style),
            Span::styled(counts_text, style),
        ]);
        frame.render_widget(Paragraph::new(line).style(style), area);
    }

    /// Render the full-width tree-list (list view modes): a 1-line heading bar
    /// naming the active view, then the indented session tree. No right pane —
    /// session detail is the `i` Info modal.
    pub(super) fn render_session_list(&mut self, frame: &mut Frame, area: Rect) {
        let (heading_area, body) = split_heading_bar(area);

        let heading_style = self.theme.status_bar();
        let heading = Paragraph::new(Line::styled(
            self.ui_state.view_mode.heading_label(),
            heading_style,
        ))
        .style(heading_style);
        frame.render_widget(heading, heading_area);

        let recents_len = self
            .ui_state
            .recents_len
            .min(self.ui_state.list_items.len());
        let global_sel = self.ui_state.list_state.list_state.selected();

        // With a recents block present, pin it in a fixed panel at the top of
        // the body and scroll the rest of the list independently below it. One
        // global selection index spans both; each rendered slice highlights it
        // only when it falls inside that slice.
        if recents_len > 0 && recents_len < self.ui_state.list_items.len() {
            let rec_h = (recents_len as u16).min(body.height.saturating_sub(1));
            let sub = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(rec_h), Constraint::Min(0)])
                .split(body);

            // Number + colour for recent rows, computed over the FULL list so
            // they mirror the real rows in the scrolling list below.
            let display_info =
                crate::widgets::worktree_display_info(&self.ui_state.list_items, &self.theme);

            // Whether the *real* rows show the `(program)` suffix — decided over
            // the scrolling list below, then mirrored onto the recents slice so
            // recents rows match (the slice alone can't see the real programs).
            let show_program = self.config.show_session_program
                && crate::widgets::list_has_mixed_programs(
                    &self.ui_state.list_items[recents_len..],
                );

            let mut rec_state = ratatui::widgets::ListState::default();
            if let Some(s) = global_sel
                && s < recents_len
            {
                rec_state.select(Some(s));
            }
            let recents_tree = TreeList::new(&self.ui_state.list_items[..recents_len], &self.theme)
                .tick(self.ui_state.tick_count)
                .highlight_style(self.theme.selection().add_modifier(Modifier::BOLD))
                .review_labels(&self.config.pr_review_labels)
                .invert_pr_label_color(self.config.invert_pr_label_color)
                .show_program_override(show_program)
                .comment_sessions(self.ui_state.sessions_with_comments.clone())
                .recent_display_info(display_info);
            // Record the recents-panel rect for mouse hit-testing.
            self.ui_state.recents_rect = Some(sub[0]);
            frame.render_stateful_widget(recents_tree, sub[0], &mut rec_state);

            self.render_main_list(frame, sub[1], recents_len, global_sel);
        } else {
            self.ui_state.recents_rect = None;
            self.render_main_list(frame, body, 0, global_sel);
        }
    }

    /// Render the scrolling session list (everything after the pinned recents
    /// block). `offset` is the number of leading `list_items` that belong to
    /// the recents panel — `0` when there is none — so the global selection
    /// index is translated into this slice and the persisted scroll offset is
    /// kept across frames.
    fn render_main_list(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        offset: usize,
        global_sel: Option<usize>,
    ) {
        // Record the main-list rect (and the recents offset) for mouse
        // hit-testing (cleared in board view).
        self.ui_state.list_rect = Some(area);
        let blocked: std::collections::HashMap<ProjectId, &str> = self
            .ui_state
            .project_pull_blocked
            .iter()
            .map(|(id, r)| (*id, r.as_str()))
            .collect();

        let tree_list = TreeList::new(&self.ui_state.list_items[offset..], &self.theme)
            .tick(self.ui_state.tick_count)
            .highlight_style(self.theme.selection().add_modifier(Modifier::BOLD))
            .review_labels(&self.config.pr_review_labels)
            .invert_pr_label_color(self.config.invert_pr_label_color)
            .show_session_program(self.config.show_session_program)
            .pull_blocked_projects(blocked)
            .comment_sessions(self.ui_state.sessions_with_comments.clone());

        let mut main_state = ratatui::widgets::ListState::default();
        *main_state.offset_mut() = self.ui_state.main_list_offset;
        match global_sel {
            Some(s) if s >= offset => main_state.select(Some(s - offset)),
            _ => main_state.select(None),
        }
        frame.render_stateful_widget(tree_list, area, &mut main_state);
        // Persist the (possibly ratatui-adjusted) scroll offset for the next
        // frame and for mouse hit-testing, plus the visible row count that
        // sizes a page jump.
        self.ui_state.main_list_offset = main_state.offset();
        self.ui_state.main_list_height = area.height;
    }

    /// Render the full-screen kanban board (or an empty-board hint), recording
    /// the hit regions and column rectangles for mouse handling.
    fn render_board(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        // No list rows are drawn in board view; drop any list hit-test rects
        // recorded by a previous list-view frame so a stale click can't map.
        self.ui_state.list_rect = None;
        self.ui_state.recents_rect = None;

        // Zero projects: nothing to lay out. Show a centred hint pointing at the
        // add-project key (resolved from the live binding, with a fallback when
        // it is unbound). With more than one server configured the board renders
        // anyway — its sidebar headings carry per-server connection health,
        // which must stay visible even before any projects exist.
        if self.ui_state.board.projects.is_empty() && self.ui_state.board.servers.len() <= 1 {
            let key = self
                .config
                .keybindings
                .keys_for(BindableAction::NewProject)
                .first()
                .map(|k| k.to_string());
            let hint = match key {
                Some(k) => format!("Press {k} to add a project"),
                None => "Add a project to get started".to_string(),
            };
            let para = Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(self.theme.text_secondary),
            )))
            .alignment(Alignment::Center);
            let mid = Rect {
                x: area.x,
                y: area.y + area.height / 2,
                width: area.width,
                height: 1,
            };
            frame.render_widget(para, mid);
            self.ui_state.board_hit_regions.clear();
            self.ui_state.board_button_regions.clear();
            self.ui_state.board_heading_regions.clear();
            self.ui_state.board_column_rects = None;
            return;
        }

        let selected = self.ui_state.board_state.selected();

        let widget = BoardWidget::new(&self.ui_state.board, &self.theme)
            .tick(self.ui_state.tick_count)
            .review_labels(&self.config.pr_review_labels)
            .invert_pr_label_color(self.config.invert_pr_label_color)
            .show_session_program(self.config.show_session_program)
            .mixed_programs(self.ui_state.has_mixed_programs)
            .comment_sessions(&self.ui_state.sessions_with_comments)
            .pull_blocked_projects(&self.ui_state.project_pull_blocked)
            .project_colors(&self.ui_state.project_colors)
            .session_numbers(&self.ui_state.session_numbers)
            .selected(selected)
            .rounded(self.config.rounded_borders);

        let out = widget.render(area, frame.buffer_mut(), &mut self.ui_state.board_state);
        self.ui_state.board_hit_regions = out.hit_regions;
        self.ui_state.board_button_regions = out.button_regions;
        self.ui_state.board_heading_regions = out.heading_regions;
        self.ui_state.board_column_rects = Some(out.rects);
    }

    /// Render the list views' right-hand pane: a live capture of the selected
    /// session's agent pane or shell, or its Info view, with a tab header.
    ///
    /// The pane is passive — keys always drive the session list — so the live
    /// captures render dimmed when `dim_unfocused_preview` is set, keeping the
    /// list visually dominant. Info is exempt: it is static, already styled for
    /// legibility, and shares its lines with the modal, so dimming it would only
    /// make the same text harder to read. Capture content arrives from
    /// `spawn_preview_update`; its scroll follows the tail until the user wheels
    /// away from the bottom.
    fn render_right_pane(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let on_project = self.is_project_selected();
        let view = self.ui_state.right_pane_view.effective(on_project);
        let (tabs, active) = self.ui_state.right_pane_view.tabs(on_project);

        // A `Paragraph` only paints the cells its text covers, so switching to a
        // shorter body would leave the previous tab's glyphs behind. Reset the
        // pane whenever what it shows changes — driven by the *effective* view,
        // so this also covers a selection moving between a session and a project
        // row (which swaps Preview for Shell without any key being pressed).
        if self.ui_state.last_pane_view != Some(view) {
            self.ui_state.last_pane_view = Some(view);
            frame.render_widget(Clear, area);
        }

        let block = Block::default()
            .title(self.build_pane_tabs(tabs, active))
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(self.theme.border_unfocused());

        if view == RightPaneView::Info {
            // InfoView draws no block of its own, so give it the inner area and
            // render the tab-header block around it.
            let inner = block.inner(area);
            frame.render_widget(block, area);

            // Build the lines once: they size the scroll metrics *and* render.
            // The content borrows `self`, so clamp against a local copy of the
            // offset here and record the metrics after that borrow is released.
            let info = InfoView::new(self.build_info_content(), &self.theme);
            let lines = info.build_lines();
            let total = lines.len();
            let max_scroll = total.saturating_sub(inner.height as usize) as u16;
            let scroll = self.ui_state.info_state.scroll_offset.min(max_scroll);
            frame.render_widget(info.with_prebuilt_lines(lines).scroll(scroll), inner);

            self.ui_state.info_state.set_metrics(total, inner.height);
            return;
        }

        let dim_opacity = self
            .config
            .dim_unfocused_preview
            .then_some(self.config.dim_unfocused_opacity);

        // Borders take one row top and bottom.
        let inner_height = area.height.saturating_sub(2);
        // Only the two capture tabs reach here — Info returned above.
        let (content, state) = if view == RightPaneView::Preview {
            (
                &self.ui_state.preview_content,
                &mut self.ui_state.preview_state,
            )
        } else {
            (&self.ui_state.shell_content, &mut self.ui_state.shell_state)
        };
        state.set_content(content, inner_height);
        let scroll = state.scroll_offset;
        let parsed = state.paragraph(dim_opacity);

        frame.render_widget(
            Preview::new(content)
                .with_paragraph(parsed)
                .block(block)
                .scroll(scroll),
            area,
        );
    }

    /// Build a styled tab-title line for the right pane's header.
    fn build_pane_tabs(&self, tabs: &[&str], active: usize) -> Line<'static> {
        pane_tabs(&self.theme, tabs, active)
    }

    /// Build the Info content for the current selection — session detail, or a
    /// project's path/branch/pull status when a project row is selected.
    ///
    /// Shared by both surfaces that render `InfoView`: the right pane's Info tab
    /// and the `i` Info modal, so the two can never disagree.
    pub(super) fn build_info_content(&self) -> InfoContent<'_> {
        let Some(sref) = self.ui_state.selected_session_id else {
            return self.build_project_info_content();
        };
        let session_id = sref.id;

        // Compute display string for the generate-summary hotkey (None = AI off)
        let summary_key_hint = if self.config.ai_summary_enabled {
            self.config
                .keybindings
                .keys_for(BindableAction::GenerateSummary)
                .first()
                .map(|k| k.to_string())
        } else {
            None
        };

        // Read session fields from the owning backend's snapshot (always
        // populated), not the board — the board is only built in board view.
        let Some(session) = self.session(sref) else {
            return InfoContent::Empty;
        };
        let title = session.title.clone();
        let branch = session.branch.clone();
        let status = session.status;
        let program = session.program.clone();
        let pr_number = session.pr_number;
        let pr_url = session.pr_url.clone();
        let pr_merged = session.pr_merged;
        let worktree_path = session.worktree_path.clone();
        let created_at = session.created_at.format("%Y-%m-%d %H:%M UTC").to_string();

        let enriched_pr = self
            .ui_state
            .enriched_pr
            .as_ref()
            .and_then(|(sid, pr)| if *sid == session_id { Some(pr) } else { None });
        let review_label = if self
            .view_for(sref.backend)
            .snapshot
            .server
            .effective_code_host()
            .provider
            == claude_commander_protocol::hosting::CodeHostProvider::Gitlab
        {
            "MR"
        } else {
            "PR"
        };

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
            review_label,
            enriched_pr,
            ai_summary,
            summary_key_hint,
            stack_chain: &self.ui_state.stack_chain,
        })
    }

    /// Info content for a selected project row: its path, main branch, and any
    /// reason the background branch pull is currently held back. `Empty` when
    /// nothing (or something that is neither) is selected.
    fn build_project_info_content(&self) -> InfoContent<'_> {
        let Some((_backend, project_id)) = self.ui_state.selected_project_id else {
            return InfoContent::Empty;
        };
        let Some(project) = self.project(project_id) else {
            return InfoContent::Empty;
        };
        InfoContent::Project(InfoProjectData {
            name: project.name.clone(),
            repo_path: project.repo_path.display().to_string(),
            main_branch: project.main_branch.clone(),
            pull_blocked: self
                .ui_state
                .project_pull_blocked
                .get(&project_id)
                .map(|r| r.as_str().to_string()),
        })
    }

    /// Render status bar. Returns the clickable action-button regions drawn in
    /// the middle zone (empty when a toast / restart message occupies the bar).
    pub(super) fn render_status_bar(&self, frame: &mut Frame, area: Rect) -> Vec<ActionButton> {
        if area.height < 2 {
            return Vec::new();
        }

        let status_area = Rect {
            x: area.x,
            y: area.bottom().saturating_sub(1),
            width: area.width,
            height: 1,
        };

        let base_style = self.theme.status_bar();
        let accent = base_style.fg(self.theme.status_bar_accent);
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

        let restart_needed = self.service.restart_required();

        // Count sessions across every backend's snapshot so the bar is correct
        // in the list views too (the board is only built in board view) —
        // within the active workspace, like the views themselves.
        let filter = self.workspace_filter();
        let session_count: usize = self
            .backends
            .iter()
            .map(|h| filter.scope(&h.view.snapshot).sessions.len())
            .sum();

        let sessions_span = Span::styled(
            format!(" Sessions: {session_count}"),
            base_style.add_modifier(Modifier::BOLD),
        );

        let help_hint = Span::styled("? help ", base_style);

        // A toast or restart notice claims the bar; otherwise it hosts the
        // context-aware action buttons.
        let show_buttons = toast.is_none() && !restart_needed;

        // Build the left status zone: session count, then any transient message.
        let mut left_spans = vec![sessions_span];
        if let Some(msg) = toast {
            left_spans.push(sep.clone());
            left_spans.push(Span::styled(msg, base_style));
            if restart_needed {
                left_spans.push(sep.clone());
                left_spans.push(Span::styled("Restart to apply config changes", base_style));
            }
        } else if restart_needed {
            left_spans.push(sep.clone());
            left_spans.push(Span::styled("Restart to apply config changes", base_style));
        }

        // The commander chip reflects live system state, so it shows in every
        // branch (toast / restart / default) — spliced right after the session
        // count. The label folds in the live agent state; absence of the chip
        // means the commander isn't running.
        let commander_agent_state = self
            .ui_state
            .agent_states
            .get(&claude_commander_protocol::session::COMMANDER_SENTINEL_ID)
            .copied();
        let commander_chip_shown =
            match commander_chip_label(self.ui_state.commander_running, commander_agent_state) {
                Some(label) => {
                    left_spans.splice(
                        1..1,
                        [
                            Span::styled(" \u{2502} ", base_style),
                            Span::styled(
                                label,
                                base_style.fg(self.theme.on_status_bar(self.theme.status_running)),
                            ),
                        ],
                    );
                    true
                }
                None => false,
            };

        // Same reasoning as the commander chip, and spliced after it so the two
        // keep a stable order regardless of which is present.
        if let Some(label) = server_chip_label(self.ui_state.embedded_server.as_ref()) {
            let colour = match self.ui_state.embedded_server {
                Some(EmbeddedServerStatus::Failed { .. }) => self.theme.modal_error,
                _ => self.theme.status_running,
            };
            let at = left_spans
                .len()
                .min(1 + usize::from(commander_chip_shown) * 2);
            left_spans.splice(
                at..at,
                [
                    Span::styled(" \u{2502} ", base_style),
                    Span::styled(label, base_style.fg(self.theme.on_status_bar(colour))),
                ],
            );
        }

        // The workspace chip and the other workspaces' waiting hints lead the
        // bar, ahead of everything spliced above (whose indices assume the
        // session count is first, so this goes in last). Absent until a second
        // workspace exists.
        let workspace_spans = self.workspace_status_spans(base_style);
        if !workspace_spans.is_empty() {
            let mut lead = vec![Span::styled(" ", base_style)];
            lead.extend(workspace_spans);
            // `Sessions` leads with its own space, so the separator doesn't.
            lead.push(Span::styled(" \u{2502}", base_style));
            left_spans.splice(0..0, lead);
        }

        // A trailing separator visually detaches the buttons from the status.
        if show_buttons {
            left_spans.push(sep);
        }

        let left_line = Line::from(left_spans);
        let left_width = left_line.width() as u16;

        // Split the status area into left status | middle buttons | help hint.
        let help_width = 8u16; // "? help " + padding
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(left_width),
                Constraint::Fill(1),
                Constraint::Length(help_width),
            ])
            .split(status_area);

        frame.render_widget(Paragraph::new(left_line).style(base_style), chunks[0]);

        let buttons = if show_buttons {
            let actions = self.status_bar_actions();
            self.render_action_bar(frame, &actions, chunks[1], base_style, accent)
        } else {
            Vec::new()
        };

        let right_line = Line::from(vec![help_hint]).alignment(Alignment::Right);
        frame.render_widget(Paragraph::new(right_line).style(base_style), chunks[2]);

        buttons
    }

    /// The ordered, context-aware set of actions surfaced as buttons in the
    /// status bar, filtered to those currently invokable.
    fn status_bar_actions(&self) -> Vec<BindableAction> {
        use BindableAction::*;
        const ACTIONS: &[BindableAction] = &[
            NewSession,
            NewStackedSession,
            DeleteSession,
            OpenReviewDiff,
            OpenInfo,
            OpenInEditor,
            NewProject,
        ];
        ACTIONS
            .iter()
            .copied()
            .filter(|&a| self.ui_state.is_command_available(a))
            .collect()
    }

    /// Render a horizontal row of bracketed, clickable action buttons into
    /// `area` (a 1-row rect), separated by `" │ "`. Returns each button's
    /// rect + action for hit-testing. A button that would overflow `area` is
    /// dropped whole (never half-clipped), along with all lower-priority
    /// buttons after it.
    pub(super) fn render_action_bar(
        &self,
        frame: &mut Frame,
        actions: &[BindableAction],
        area: Rect,
        base: Style,
        accent: Style,
    ) -> Vec<ActionButton> {
        const SEP: &str = " \u{2502} ";
        const SEP_WIDTH: u16 = 3; // " │ "

        // Segment + style each label once, then let `layout_buttons` decide
        // which fit. Kept buttons are always a prefix, so the rendered spans
        // (built from the same prefix) stay in sync with the recorded rects.
        let rendered: Vec<(BindableAction, Vec<Span<'static>>)> = actions
            .iter()
            .map(|&action| {
                let seg = crate::hotkey::segment_label(
                    action.button_label(),
                    &self.config.keybindings,
                    action,
                );
                (action, crate::hotkey::hotkey_spans(&seg, base, accent))
            })
            .collect();

        let widths: Vec<(BindableAction, u16)> = rendered
            .iter()
            .map(|(action, spans)| (*action, spans.iter().map(|s| s.width() as u16).sum()))
            .collect();

        let buttons = crate::hotkey::layout_buttons(&widths, area, SEP_WIDTH);

        let mut spans: Vec<Span> = Vec::new();
        for (i, (_, btn_spans)) in rendered.iter().take(buttons.len()).enumerate() {
            if i != 0 {
                spans.push(Span::styled(SEP, base));
            }
            spans.extend(btn_spans.iter().cloned());
        }

        frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
        buttons
    }
}

/// Build a styled tab-title line for the right pane's header. The active tab is
/// bold accent, the rest secondary, separated by ` · `.
///
/// A free function taking `&Theme` rather than an `App` method so the render
/// snapshots in [`crate::render_tests`] can frame the preview/info panes with the
/// real tab strip instead of a copy of it — the old snapshot file carried its own
/// duplicate, which is exactly the kind of thing that drifts. `App::build_pane_tabs`
/// delegates here.
pub(crate) fn pane_tabs(theme: &Theme, tabs: &[&str], active: usize) -> Line<'static> {
    let active_style = Style::default()
        .fg(theme.text_accent)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(theme.text_secondary);

    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" \u{00b7} ", inactive_style));
        }
        spans.push(Span::styled(
            tab.to_string(),
            if i == active {
                active_style
            } else {
                inactive_style
            },
        ));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}
