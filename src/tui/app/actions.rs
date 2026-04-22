//! User actions: session selection, creation, deletion, editor/PR/shell interactions.

use super::*;

/// Maximum number of rows rendered in a scrollable list modal at once.
///
/// Shared between the render layer and the input handler so the scroll
/// offset and the visible window agree. Used by both the quick-switch
/// palette and the path-input completions list.
pub(super) const LIST_MAX_VISIBLE: usize = 10;

/// Return the `scroll` offset that keeps `selected_idx` inside a visible
/// window of `visible_rows` rows, starting from the caller's current
/// scroll position. Handles all four cases (above window, below window,
/// wrap-around onto either end, and no-op when already in view) in a single
/// pure function so it can be unit-tested independently.
pub(super) fn adjust_list_scroll(selected_idx: usize, scroll: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 {
        return 0;
    }
    if selected_idx < scroll {
        selected_idx
    } else if selected_idx >= scroll + visible_rows {
        selected_idx + 1 - visible_rows
    } else {
        scroll
    }
}

impl App {
    /// Open `Modal::PathInput` at the current working directory with its
    /// subdirectory list already populated.
    ///
    /// The initial value is `cwd/` (trailing slash appended) so
    /// `list_matching_dirs` returns the children of cwd rather than its
    /// siblings — which is what users almost always want for Add Project /
    /// Scan Directory.
    pub(super) fn open_path_input(
        &mut self,
        title: String,
        prompt: String,
        on_submit: InputAction,
    ) {
        let mut value = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if !value.ends_with('/') {
            value.push('/');
        }
        let mut completer = PathCompleter::new();
        completer.refilter(&value);
        self.ui_state.modal = Modal::PathInput {
            title,
            prompt,
            value,
            on_submit,
            completer,
            scroll: 0,
        };
    }

    /// Check if the selected session is in Creating state
    pub(super) fn selected_session_is_creating(&self) -> bool {
        self.ui_state.list_items.iter().any(|item| {
            matches!(
                item,
                SessionListItem::Worktree { id, status, .. }
                if self.ui_state.selected_session_id == Some(*id)
                    && *status == SessionStatus::Creating
            )
        })
    }

    /// Handle selection (attach to session)
    pub(super) async fn handle_select(&mut self) {
        info!(
            "handle_select called, selected_session_id: {:?}",
            self.ui_state.selected_session_id
        );
        if self.selected_session_is_creating() {
            return;
        }
        if let Some(session_id) = self.ui_state.selected_session_id {
            info!("Getting attach command for session: {}", session_id);
            match self.session_manager.get_attach_command(&session_id).await {
                Ok(cmd) => {
                    info!("Got attach command: {}", cmd);
                    // Clear unread flag when attaching
                    let sid = session_id;
                    let _ = self
                        .store
                        .mutate(move |state| {
                            if let Some(session) = state.get_session_mut(&sid) {
                                session.unread = false;
                            }
                        })
                        .await;
                    self.ui_state.attach_command = Some(cmd);
                    self.ui_state.should_quit = true;
                    info!("Set should_quit = true");
                }
                Err(e) => {
                    info!("Failed to get attach command: {}", e);
                    self.ui_state.modal = Modal::Error {
                        message: format!("Cannot attach: {}", e),
                    };
                }
            }
        } else {
            info!("No session selected");
        }
    }

    /// Handle shell selection (attach to shell session)
    pub(super) async fn handle_select_shell(&mut self) {
        if self.selected_session_is_creating() {
            return;
        }
        if let Some(session_id) = self.ui_state.selected_session_id {
            match self
                .session_manager
                .get_shell_attach_command(&session_id)
                .await
            {
                Ok(cmd) => {
                    self.ui_state.attach_command = Some(cmd);
                    self.ui_state.should_quit = true;
                }
                Err(e) => {
                    self.ui_state.modal = Modal::Error {
                        message: format!("Cannot open shell: {}", e),
                    };
                }
            }
        } else if let Some(project_id) = self.ui_state.selected_project_id {
            match self
                .session_manager
                .get_project_shell_attach_command(&project_id)
                .await
            {
                Ok(cmd) => {
                    self.ui_state.attach_command = Some(cmd);
                    self.ui_state.should_quit = true;
                }
                Err(e) => {
                    self.ui_state.modal = Modal::Error {
                        message: format!("Cannot open shell: {}", e),
                    };
                }
            }
        }
    }

    /// Resolve the shell toggle pair for a given tmux session name.
    ///
    /// If the current session is a Claude session, returns the shell session name
    /// (creating it if needed). If the current session is already a shell session
    /// (ends with "-sh"), returns the Claude session name.
    pub(super) async fn resolve_shell_toggle_pair(
        &mut self,
        current_tmux_name: &str,
    ) -> crate::error::Result<String> {
        if current_tmux_name.ends_with("-sh") {
            // We're in a shell session — the Claude session is the name without "-sh"
            let claude_name = current_tmux_name.trim_end_matches("-sh").to_string();
            // Verify the Claude session exists
            if self
                .session_manager
                .tmux
                .session_exists(&claude_name)
                .await?
            {
                return Ok(claude_name);
            }
            return Err(crate::error::Error::Session(
                crate::error::SessionError::TmuxSessionNotFound(claude_name),
            ));
        }

        // We're in a Claude session — find the matching session ID and ensure shell exists
        let session_id = {
            let state = self.store.read().await;
            state
                .sessions
                .values()
                .find(|s| s.tmux_session_name == current_tmux_name)
                .map(|s| s.id)
        };

        if let Some(session_id) = session_id {
            let shell_name = self
                .session_manager
                .ensure_shell_session(&session_id)
                .await?;
            return Ok(shell_name);
        }

        // Try project-level shell
        let project_id = {
            let state = self.store.read().await;
            state
                .projects
                .values()
                .find(|p| p.shell_tmux_session_name.as_deref() == Some(current_tmux_name))
                .map(|p| p.id)
        };

        if let Some(project_id) = project_id {
            let shell_name = self
                .session_manager
                .ensure_project_shell_session(&project_id)
                .await?;
            return Ok(shell_name);
        }

        Err(crate::error::Error::Session(
            crate::error::SessionError::TmuxSessionNotFound(format!(
                "No session found for tmux name: {}",
                current_tmux_name
            )),
        ))
    }

    /// Open the editor for the worktree associated with a given tmux session
    /// name. Used when the user presses Ctrl+. while attached to a tmux
    /// session — the tmux session itself is not affected, we simply launch
    /// the configured editor pointing at the session's worktree. This runs
    /// while we are *between* attaches, so the TUI is torn down and raw mode
    /// is already disabled.
    pub(super) async fn open_editor_for_tmux_session(&mut self, tmux_session_name: &str) {
        // Shell sessions are named `<claude_name>-sh`; the worktree is owned
        // by the underlying Claude session.
        let lookup_name = tmux_session_name
            .strip_suffix("-sh")
            .unwrap_or(tmux_session_name)
            .to_string();

        let path = {
            let state = self.store.read().await;
            state
                .sessions
                .values()
                .find(|s| s.tmux_session_name == lookup_name)
                .map(|s| s.worktree_path.clone())
        };

        let Some(path) = path else {
            warn!(
                "OpenEditor: no session found for tmux name '{}'",
                tmux_session_name
            );
            return;
        };

        let Some(editor) = self.config.resolve_editor() else {
            warn!("OpenEditor: no editor configured");
            return;
        };

        if self.config.is_gui_editor(&editor) {
            // GUI editor: spawn detached and return — tmux session is
            // untouched and we'll re-attach immediately.
            info!(
                "OpenEditor: launching GUI editor '{}' at {}",
                editor,
                path.display()
            );
            if let Err(e) = std::process::Command::new(&editor).arg(&path).spawn() {
                warn!("Failed to launch GUI editor '{}': {}", editor, e);
            }
        } else {
            // Terminal editor: run foreground, inheriting stdio. Raw mode is
            // already off (attach_to_session disabled it on exit) so the
            // editor gets a cooked terminal. When it returns we loop back
            // into attach_to_session with the same tmux session name.
            info!(
                "OpenEditor: launching terminal editor '{}' at {}",
                editor,
                path.display()
            );
            if let Err(e) = std::process::Command::new(&editor).arg(&path).status() {
                warn!("Failed to launch terminal editor '{}': {}", editor, e);
            }
        }
    }

    /// Handle open in editor command
    pub(super) async fn handle_open_in_editor(&mut self) {
        if self.selected_session_is_creating() {
            return;
        }
        let path = {
            let state = self.store.read().await;
            if let Some(session_id) = self.ui_state.selected_session_id {
                state
                    .sessions
                    .get(&session_id)
                    .map(|s| s.worktree_path.clone())
            } else if let Some(project_id) = self.ui_state.selected_project_id {
                state.projects.get(&project_id).map(|p| p.repo_path.clone())
            } else {
                None
            }
        };

        let Some(path) = path else {
            return;
        };

        let Some(editor) = self.config.resolve_editor() else {
            self.ui_state.modal = Modal::Error {
                message: "No editor configured. Set 'editor' in config.toml or \
                          set $VISUAL / $EDITOR."
                    .to_string(),
            };
            return;
        };

        if self.config.is_gui_editor(&editor) {
            // GUI editor: spawn detached, TUI stays up
            if let Err(e) = std::process::Command::new(&editor).arg(&path).spawn() {
                self.ui_state.modal = Modal::Error {
                    message: format!("Failed to launch '{}': {}", editor, e),
                };
            }
        } else {
            // Terminal editor: tear down TUI, run foreground, restore
            self.ui_state.editor_command = Some((editor, path));
            self.ui_state.should_quit = true;
        }
    }

    /// Handle "open PR in browser" — looks up the selected session's
    /// `pr_url` and launches the OS default handler (`open` on macOS,
    /// `xdg-open` on Linux, `cmd /c start` on Windows).
    pub(super) async fn handle_open_pull_request(&mut self) {
        let Some(session_id) = self.ui_state.selected_session_id else {
            return;
        };
        let pr_url = {
            let state = self.store.read().await;
            state
                .sessions
                .get(&session_id)
                .and_then(|s| s.pr_url.clone())
        };
        let Some(url) = pr_url else {
            self.ui_state.status_message = Some((
                "No PR associated with this session".to_string(),
                Instant::now() + Duration::from_secs(3),
            ));
            return;
        };

        let result = if cfg!(target_os = "macos") {
            std::process::Command::new("open").arg(&url).spawn()
        } else if cfg!(target_os = "windows") {
            std::process::Command::new("cmd")
                .args(["/c", "start", "", &url])
                .spawn()
        } else {
            std::process::Command::new("xdg-open").arg(&url).spawn()
        };

        if let Err(e) = result {
            self.ui_state.modal = Modal::Error {
                message: format!("Failed to open PR in browser: {}", e),
            };
        }
    }

    /// Handle new session command
    pub(super) fn handle_new_session(&mut self) {
        if let Some(project_id) = self.ui_state.selected_project_id {
            self.ui_state.modal = Modal::Input {
                title: "New Session".to_string(),
                prompt: "Enter session name:".to_string(),
                value: String::new(),
                on_submit: InputAction::CreateSession { project_id },
            };
        } else {
            self.ui_state.status_message = Some((
                "Select a project first (use N to add one)".to_string(),
                Instant::now() + Duration::from_secs(3),
            ));
        }
    }

    /// Open the Checkout Branch modal.
    ///
    /// Loads the current list of branches synchronously via gix and kicks
    /// off `git fetch origin` in a background task so the list can be
    /// refreshed once remote changes are pulled in.
    pub(super) async fn handle_checkout_branch(&mut self) {
        let Some(project_id) = self.ui_state.selected_project_id else {
            self.ui_state.status_message = Some((
                "Select a project first (use N to add one)".to_string(),
                Instant::now() + Duration::from_secs(3),
            ));
            return;
        };

        let repo_path = {
            let state = self.store.read().await;
            match state.get_project(&project_id) {
                Some(p) => p.repo_path.clone(),
                None => {
                    self.ui_state.modal = Modal::Error {
                        message: "Project not found".to_string(),
                    };
                    return;
                }
            }
        };

        let all_branches = match load_branch_entries(&repo_path) {
            Ok(b) => b,
            Err(e) => {
                self.ui_state.modal = Modal::Error {
                    message: format!("Failed to list branches: {}", e),
                };
                return;
            }
        };

        let filtered = all_branches.clone();
        self.ui_state.modal = Modal::CheckoutBranch {
            project_id,
            query: String::new(),
            all_branches,
            filtered,
            selected_idx: 0,
            scroll: 0,
            fetching: true,
        };

        // Spawn `git fetch origin` in the background; when it finishes,
        // post a CheckoutFetchComplete state update so the modal (if still
        // open) can refresh its list.
        let tx = self.event_loop.sender();
        let repo_path_bg = repo_path.clone();
        tokio::spawn(async move {
            let _ = tokio::process::Command::new("git")
                .current_dir(&repo_path_bg)
                .args(["fetch", "origin"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await;

            // Re-list branches after the fetch. Run the sync gix call in a
            // blocking task so we don't stall the async runtime.
            let branches = tokio::task::spawn_blocking(move || {
                crate::git::GitBackend::open(&repo_path_bg)
                    .and_then(|b| b.list_branches())
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();

            let _ = tx
                .send(AppEvent::StateUpdate(StateUpdate::CheckoutFetchComplete {
                    project_id,
                    branches,
                }))
                .await;
        });
    }

    /// Case-insensitive filter over the Checkout modal's branch list.
    pub(super) fn refilter_checkout_branches(&mut self) {
        if let Modal::CheckoutBranch {
            query,
            all_branches,
            filtered,
            selected_idx,
            scroll,
            ..
        } = &mut self.ui_state.modal
        {
            let q = query.to_lowercase();
            *filtered = if q.is_empty() {
                all_branches.clone()
            } else {
                all_branches
                    .iter()
                    .filter(|b| {
                        b.local_name.to_lowercase().contains(&q)
                            || b.display_name.to_lowercase().contains(&q)
                    })
                    .cloned()
                    .collect()
            };

            if *selected_idx >= filtered.len() {
                *selected_idx = filtered.len().saturating_sub(1);
            }
            if *scroll > *selected_idx {
                *scroll = *selected_idx;
            }
        }
    }

    /// Start creating a worktree session from an existing branch.
    ///
    /// `branch_name` is the local branch name (remote tracking refs should
    /// already have had their `origin/` prefix stripped before calling).
    /// The session title is derived from the branch name so the worktree
    /// directory uses the same naming as a manually-named new session.
    pub(super) async fn start_checkout_session(
        &mut self,
        project_id: ProjectId,
        branch_name: String,
    ) {
        let branch_name = branch_name.trim().to_string();
        if branch_name.is_empty() {
            return;
        }

        // Use the branch name verbatim as the session title. This keeps
        // `display_branch` from rendering a redundant `[branch]` annotation
        // in the list (it short-circuits on exact title == branch match)
        // and the worktree directory still comes out sensibly because
        // `sanitize_name` handles slashes and special chars.
        let title = branch_name.clone();

        let session_id = match self
            .session_manager
            .prepare_session(&project_id, title, None, Some(branch_name.clone()))
            .await
        {
            Ok(id) => id,
            Err(e) => {
                self.ui_state.modal = Modal::Error {
                    message: format!("Failed to create session: {}", e),
                };
                return;
            }
        };

        // Refresh list and select the new placeholder
        self.refresh_list_items().await;
        if let Some(idx) = self.ui_state.list_items.iter().position(
            |item| matches!(item, SessionListItem::Worktree { id, .. } if *id == session_id),
        ) {
            self.ui_state.list_state.select(Some(idx));
        }
        self.update_selection();

        // Spawn background task for heavy work (same pattern as NewSession)
        let session_manager = self.session_manager.clone();
        let tx = self.event_loop.sender();
        tokio::spawn(async move {
            match session_manager.finalize_session(&session_id).await {
                Ok(sid) => {
                    let _ = tx
                        .send(AppEvent::StateUpdate(StateUpdate::SessionCreated {
                            session_id: sid,
                        }))
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(AppEvent::StateUpdate(StateUpdate::SessionCreateFailed {
                            session_id,
                            message: format!("Failed to create session: {}", e),
                        }))
                        .await;
                }
            }
        });
    }

    /// Open the quick-switch palette in the given mode.
    pub(super) async fn open_quick_switch_with_mode(&mut self, mode: PaletteMode) {
        let matches = self.build_palette_items(mode, "").await;
        self.ui_state.modal = Modal::QuickSwitch {
            mode,
            query: String::new(),
            matches,
            selected_idx: 0,
            scroll: 0,
        };
    }

    /// Gather session matches for a query (empty query = all sessions).
    ///
    /// Non-empty queries are ranked by fuzzy score (best match first);
    /// empty queries fall back to alphabetical title order.
    pub(super) async fn gather_quick_switch_matches(&self, query: &str) -> Vec<QuickSwitchMatch> {
        let state = self.store.read().await;
        let mut scored: Vec<(i64, QuickSwitchMatch)> = Vec::new();

        for session in state.sessions.values() {
            if session.status == SessionStatus::Creating {
                continue;
            }
            let Some(score) = session.fuzzy_score(query) else {
                continue;
            };
            let project_name = state
                .get_project(&session.project_id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            scored.push((
                score,
                QuickSwitchMatch {
                    session_id: session.id,
                    title: session.title.clone(),
                    branch: session.branch.clone(),
                    project_name,
                    status: session.status,
                },
            ));
        }

        if query.is_empty() {
            scored.sort_by(|a, b| a.1.title.cmp(&b.1.title));
        } else {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.title.cmp(&b.1.title)));
        }
        scored.into_iter().map(|(_, m)| m).collect()
    }

    /// Compute the *effective* palette mode — a leading `>` in a Unified
    /// query upgrades it to CommandOnly without mutating the stored mode,
    /// so backspacing past the `>` naturally restores the unified view.
    pub(super) fn effective_palette_mode(mode: PaletteMode, query: &str) -> PaletteMode {
        if mode == PaletteMode::Unified && query.starts_with('>') {
            PaletteMode::CommandOnly
        } else {
            mode
        }
    }

    /// Strip the command-only `>` prefix (plus any following whitespace) when
    /// the effective mode was derived from that prefix.
    pub(super) fn palette_filter_query(mode: PaletteMode, query: &str) -> &str {
        match (mode, query.strip_prefix('>')) {
            (PaletteMode::CommandOnly, Some(rest)) => rest.trim_start(),
            _ => query,
        }
    }

    /// Build the set of command rows matching `filter_query`.
    ///
    /// Commands without a keybinding are intentionally still included — the
    /// palette is the primary access surface going forward, and some commands
    /// are expected to shed their hotkeys over time.
    pub(super) fn gather_command_entries(&self, filter_query: &str) -> Vec<CommandEntry> {
        self.ui_state
            .gather_command_entries(&self.config.keybindings, filter_query)
    }

    /// Build the mixed session+command list for the palette.
    ///
    /// Sessions first (when the effective mode is Unified), then commands.
    async fn build_palette_items(&self, mode: PaletteMode, query: &str) -> Vec<QuickSwitchItem> {
        let eff_mode = Self::effective_palette_mode(mode, query);
        let eff_query = Self::palette_filter_query(eff_mode, query);
        let mut out: Vec<QuickSwitchItem> = Vec::new();
        if let PaletteMode::SectionPicker { session_id } = eff_mode {
            return self.gather_section_picker_items(session_id, eff_query);
        }
        if eff_mode == PaletteMode::Unified {
            for m in self.gather_quick_switch_matches(eff_query).await {
                out.push(QuickSwitchItem::Session(m));
            }
        }
        for c in self.gather_command_entries(eff_query) {
            out.push(QuickSwitchItem::Command(c));
        }
        out
    }

    /// Build the section-picker rows for the move-to-section palette mode.
    /// Always includes an "Auto" entry first to clear any existing override.
    fn gather_section_picker_items(
        &self,
        session_id: SessionId,
        filter_query: &str,
    ) -> Vec<QuickSwitchItem> {
        let q = filter_query.to_lowercase();
        let mut out: Vec<QuickSwitchItem> = Vec::new();
        let auto_label = "Auto (clear override)".to_string();
        if q.is_empty() || auto_label.to_lowercase().contains(&q) {
            out.push(QuickSwitchItem::SectionMove {
                session_id,
                target: None,
                label: auto_label,
            });
        }
        for section in &self.config.sections {
            if !q.is_empty() && !section.name.to_lowercase().contains(&q) {
                continue;
            }
            out.push(QuickSwitchItem::SectionMove {
                session_id,
                target: Some(section.name.clone()),
                label: section.name.clone(),
            });
        }
        out
    }

    /// Re-filter the quick-switch matches based on the current query.
    /// Rebuilds from list_items so backspace can widen results.
    pub(super) fn refilter_quick_switch(&mut self) {
        // Snapshot the inputs we need so the closure borrow on self doesn't
        // conflict with the `&mut self.ui_state.modal` below.
        let (mode, query) = match &self.ui_state.modal {
            Modal::QuickSwitch { mode, query, .. } => (*mode, query.clone()),
            _ => return,
        };

        let eff_mode = Self::effective_palette_mode(mode, &query);
        let eff_query = Self::palette_filter_query(eff_mode, &query);

        // Build the session rows synchronously from list_items so the refilter
        // can run without awaiting the store lock on every keystroke.
        let mut scored_sessions: Vec<(i64, QuickSwitchMatch)> = Vec::new();
        if eff_mode == PaletteMode::Unified {
            // Build project name lookup from list items
            let mut project_names: std::collections::HashMap<SessionId, String> =
                std::collections::HashMap::new();
            let mut current_project_name = String::new();
            for item in &self.ui_state.list_items {
                match item {
                    SessionListItem::Project { name, .. } => {
                        current_project_name = name.clone();
                    }
                    SessionListItem::Worktree { id, .. } => {
                        project_names.insert(*id, current_project_name.clone());
                    }
                    SessionListItem::SectionHeader { .. } | SessionListItem::Spacer => {}
                }
            }

            for item in &self.ui_state.list_items {
                if let SessionListItem::Worktree {
                    id,
                    title,
                    branch,
                    status,
                    ..
                } = item
                {
                    // Score against title and branch; best field wins.
                    let score = [title.as_str(), branch.as_str()]
                        .iter()
                        .filter_map(|s| crate::fuzzy::fuzzy_score(s, eff_query))
                        .max();
                    let Some(score) = score else { continue };
                    let project_name = project_names.get(id).cloned().unwrap_or_default();
                    scored_sessions.push((
                        score,
                        QuickSwitchMatch {
                            session_id: *id,
                            title: title.clone(),
                            branch: branch.clone(),
                            project_name,
                            status: *status,
                        },
                    ));
                }
            }

            if eff_query.is_empty() {
                // Preserve tree order for empty queries.
            } else {
                scored_sessions
                    .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.title.cmp(&b.1.title)));
            }
        }
        let session_items: Vec<QuickSwitchItem> = scored_sessions
            .into_iter()
            .map(|(_, m)| QuickSwitchItem::Session(m))
            .collect();

        let command_items: Vec<QuickSwitchItem> = self
            .gather_command_entries(eff_query)
            .into_iter()
            .map(QuickSwitchItem::Command)
            .collect();

        if let Modal::QuickSwitch {
            matches,
            selected_idx,
            scroll,
            ..
        } = &mut self.ui_state.modal
        {
            *matches = session_items;
            matches.extend(command_items);

            if *selected_idx >= matches.len() {
                *selected_idx = matches.len().saturating_sub(1);
            }
            // Refilter collapses to a fresh window: reset to the top then
            // adjust so the (now-clamped) selection is still visible.
            *scroll = 0;
            *scroll = adjust_list_scroll(*selected_idx, *scroll, LIST_MAX_VISIBLE);
        }
    }

    /// Handle remove project - show confirmation (only when a project row is selected)
    pub(super) fn handle_remove_project(&mut self) {
        if self.ui_state.selected_session_id.is_none()
            && let Some(project_id) = self.ui_state.selected_project_id
        {
            self.ui_state.modal = Modal::Confirm {
                title: "Remove Project".to_string(),
                message: "Are you sure you want to remove this project?\nThis will kill all sessions and remove all worktrees.".to_string(),
                on_confirm: ConfirmAction::RemoveProject { project_id },
            };
        }
    }

    /// Handle restart session - show confirmation
    pub(super) fn handle_restart_session(&mut self) {
        if let Some(session_id) = self.ui_state.selected_session_id {
            let message = if self.config.resume_session {
                "This will kill the current tmux session and start a fresh one.\nClaude will pick up where it left off via /resume.".to_string()
            } else {
                "This will kill the current tmux session and start a fresh one.\nIf you want to pick up where you left off, you can use /resume.".to_string()
            };
            self.ui_state.modal = Modal::Confirm {
                title: "Restart Session".to_string(),
                message,
                on_confirm: ConfirmAction::RestartSession { session_id },
            };
        }
    }

    /// Handle delete session - show confirmation
    pub(super) fn handle_delete_session(&mut self) {
        if self.selected_session_is_creating() {
            return;
        }
        if let Some(session_id) = self.ui_state.selected_session_id {
            self.ui_state.modal = Modal::Confirm {
                title: "Delete Session".to_string(),
                message: "Are you sure you want to delete this session?\nThis will kill the tmux session and remove the worktree.".to_string(),
                on_confirm: ConfirmAction::DeleteSession { session_id },
            };
        }
    }

    /// Open the "Move to section" palette for the selected session.
    /// The palette lists "Auto" plus one entry per configured `[[sections]]`;
    /// selecting "Auto" clears any override.
    pub(super) async fn handle_move_to_section(&mut self) {
        if self.config.sections.is_empty() {
            self.ui_state.status_message = Some((
                "No [[sections]] configured".to_string(),
                Instant::now() + Duration::from_secs(3),
            ));
            return;
        }
        let Some(session_id) = self.ui_state.selected_session_id else {
            return;
        };
        let mode = PaletteMode::SectionPicker { session_id };
        let matches = self.gather_section_picker_items(session_id, "");
        self.ui_state.modal = Modal::QuickSwitch {
            mode,
            query: String::new(),
            matches,
            selected_idx: 0,
            scroll: 0,
        };
    }

    /// Apply a manual section move chosen in the picker palette.
    /// `target = Some(name)` sets the override; `target = None` clears it.
    pub(super) async fn apply_section_move(
        &mut self,
        session_id: SessionId,
        target: Option<String>,
    ) {
        let sections = self.config.sections.clone();
        let now = chrono::Utc::now();
        let _ = self
            .store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&session_id) {
                    session.section_override = target;
                    crate::session::apply_assignment(session, &sections, now);
                }
            })
            .await;
        self.refresh_list_items().await;
    }

    /// Handle rename session - show input modal pre-filled with current title.
    /// Only the displayed title is changed; the underlying worktree, branch,
    /// and tmux session keep their original names.
    pub(super) async fn handle_rename_session(&mut self) {
        let Some(session_id) = self.ui_state.selected_session_id else {
            return;
        };
        let current_title = {
            let state = self.store.read().await;
            match state.get_session(&session_id) {
                Some(s) => s.title.clone(),
                None => return,
            }
        };
        self.ui_state.modal = Modal::Input {
            title: "Rename Session".to_string(),
            prompt: "Enter new session name:".to_string(),
            value: current_title,
            on_submit: InputAction::RenameSession { session_id },
        };
    }

    /// Handle input modal submission
    pub(super) async fn handle_input_submit(&mut self, action: InputAction, value: String) {
        match action {
            InputAction::CreateSession { project_id } => {
                if value.trim().is_empty() {
                    self.ui_state.status_message = Some((
                        "Session name cannot be empty".to_string(),
                        Instant::now() + Duration::from_secs(3),
                    ));
                    return;
                }

                // Insert placeholder session immediately (no blocking modal)
                self.ui_state.modal = Modal::None;
                let session_id = match self
                    .session_manager
                    .prepare_session(&project_id, value, None, None)
                    .await
                {
                    Ok(id) => id,
                    Err(e) => {
                        self.ui_state.modal = Modal::Error {
                            message: format!("Failed to create session: {}", e),
                        };
                        return;
                    }
                };

                // Refresh list and select the new placeholder
                self.refresh_list_items().await;
                if let Some(idx) = self.ui_state.list_items.iter().position(|item| {
                    matches!(item, SessionListItem::Worktree { id, .. } if *id == session_id)
                }) {
                    self.ui_state.list_state.select(Some(idx));
                }
                self.update_selection();

                // Spawn background task for heavy work
                let session_manager = self.session_manager.clone();
                let tx = self.event_loop.sender();
                tokio::spawn(async move {
                    match session_manager.finalize_session(&session_id).await {
                        Ok(sid) => {
                            let _ = tx
                                .send(AppEvent::StateUpdate(StateUpdate::SessionCreated {
                                    session_id: sid,
                                }))
                                .await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(AppEvent::StateUpdate(StateUpdate::SessionCreateFailed {
                                    session_id,
                                    message: format!("Failed to create session: {}", e),
                                }))
                                .await;
                        }
                    }
                });
            }
            InputAction::AddProject => {
                let expanded = crate::tui::path_completer::expand_tilde(value.trim());
                let path = PathBuf::from(expanded);
                if !path.exists() {
                    self.ui_state.modal = Modal::Error {
                        message: format!("Path does not exist: {}", path.display()),
                    };
                    return;
                }

                match self.session_manager.add_project(path).await {
                    Ok(project_id) => {
                        self.ui_state.status_message = Some((
                            format!("Added project {}", project_id),
                            Instant::now() + Duration::from_secs(3),
                        ));
                        self.refresh_list_items().await;
                        // Select the newly added project
                        if let Some(idx) = self.ui_state.list_items.iter().position(|item| {
                            matches!(item, SessionListItem::Project { id, .. } if *id == project_id)
                        }) {
                            self.ui_state.list_state.select(Some(idx));
                        }
                    }
                    Err(e) => {
                        self.ui_state.modal = Modal::Error {
                            message: format!("Failed to add project: {}", e),
                        };
                    }
                }
            }
            InputAction::RenameSession { session_id } => {
                let new_title = value.trim().to_string();
                if new_title.is_empty() {
                    self.ui_state.status_message = Some((
                        "Session name cannot be empty".to_string(),
                        Instant::now() + Duration::from_secs(3),
                    ));
                    return;
                }
                let _ = self
                    .store
                    .mutate(move |state| {
                        if let Some(session) = state.get_session_mut(&session_id) {
                            session.title = new_title;
                        }
                    })
                    .await;
                self.refresh_list_items().await;
            }
            InputAction::ScanDirectory => {
                let expanded = crate::tui::path_completer::expand_tilde(value.trim());
                let path = PathBuf::from(expanded);
                if !path.exists() {
                    self.ui_state.modal = Modal::Error {
                        message: format!("Path does not exist: {}", path.display()),
                    };
                    return;
                }
                if !path.is_dir() {
                    self.ui_state.modal = Modal::Error {
                        message: format!("Not a directory: {}", path.display()),
                    };
                    return;
                }

                // If the path itself is a git repo, just add it directly
                if path.join(".git").exists() {
                    match self.session_manager.add_project(path).await {
                        Ok(project_id) => {
                            self.ui_state.status_message = Some((
                                format!("Added project {}", project_id),
                                Instant::now() + Duration::from_secs(3),
                            ));
                            self.refresh_list_items().await;
                            if let Some(idx) =
                                self.ui_state.list_items.iter().position(|item| {
                                    matches!(item, SessionListItem::Project { id, .. } if *id == project_id)
                                })
                            {
                                self.ui_state.list_state.select(Some(idx));
                            }
                        }
                        Err(e) => {
                            self.ui_state.modal = Modal::Error {
                                message: format!("Failed to add project: {}", e),
                            };
                        }
                    }
                    return;
                }

                // Show loading modal
                self.ui_state.modal = Modal::Loading {
                    title: "Scanning".to_string(),
                    message: format!("Scanning {} for git repos…", path.display()),
                };

                match self.session_manager.scan_directory(&path).await {
                    Ok(result) => {
                        if result.added == 0 && result.skipped == 0 {
                            self.ui_state.modal = Modal::Error {
                                message: format!("No git repositories found in {}", path.display()),
                            };
                        } else {
                            self.ui_state.modal = Modal::None;
                            self.ui_state.status_message = Some((
                                format!(
                                    "Added {} project{} ({} already existed)",
                                    result.added,
                                    if result.added == 1 { "" } else { "s" },
                                    result.skipped,
                                ),
                                Instant::now() + Duration::from_secs(5),
                            ));
                            self.refresh_list_items().await;
                        }
                    }
                    Err(e) => {
                        self.ui_state.modal = Modal::Error {
                            message: format!("Scan failed: {}", e),
                        };
                    }
                }
            }
        }
    }

    /// Handle confirmation
    pub(super) async fn handle_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::DeleteSession { session_id } => {
                // 1. Capture session data before removal
                let cleanup_data = {
                    let state = self.store.read().await;
                    state.get_session(&session_id).map(|s| {
                        let repo_path = state
                            .get_project(&s.project_id)
                            .map(|p| p.repo_path.clone());
                        (
                            s.tmux_session_name.clone(),
                            s.shell_tmux_session_name.clone(),
                            s.worktree_path.clone(),
                            repo_path,
                        )
                    })
                };

                // 2. Remove from state immediately so the UI updates
                if let Err(e) = self
                    .store
                    .mutate(move |state| {
                        state.remove_session(&session_id);
                    })
                    .await
                {
                    self.ui_state.modal = Modal::Error {
                        message: format!("Failed to save state: {}", e),
                    };
                    return;
                }
                self.ui_state.selected_session_id = None;
                self.ui_state.status_message = Some((
                    "Session deleted".to_string(),
                    Instant::now() + Duration::from_secs(3),
                ));
                self.refresh_list_items().await;

                // 3. Spawn background cleanup (kill tmux + remove worktree)
                if let Some((tmux_name, shell_tmux_name, worktree_path, repo_path)) = cleanup_data {
                    let tmux = self.session_manager.tmux.clone();
                    let tx = self.event_loop.sender();
                    tokio::spawn(async move {
                        background::cleanup_session_tmux(
                            &tmux,
                            &tmux_name,
                            shell_tmux_name.as_deref(),
                            repo_path
                                .as_ref()
                                .map(|rp| (worktree_path.as_path(), rp.as_path())),
                            &tx,
                        )
                        .await;
                    });
                }
            }
            ConfirmAction::RestartSession { session_id } => {
                match self.session_manager.restart_session(&session_id).await {
                    Ok(_) => {
                        self.ui_state.status_message = Some((
                            "Session restarted".to_string(),
                            Instant::now() + Duration::from_secs(3),
                        ));
                        self.refresh_list_items().await;
                    }
                    Err(e) => {
                        self.ui_state.modal = Modal::Error {
                            message: format!("Failed to restart: {}", e),
                        };
                    }
                }
            }
            ConfirmAction::RemoveProject { project_id } => {
                // 1. Capture project and session data before removal
                let cleanup_data = {
                    let state = self.store.read().await;
                    state.get_project(&project_id).map(|project| {
                        let repo_path = project.repo_path.clone();
                        let shell_tmux = project.shell_tmux_session_name.clone();
                        let sessions: Vec<_> = project
                            .worktrees
                            .iter()
                            .filter_map(|sid| {
                                state.get_session(sid).map(|s| {
                                    (
                                        s.tmux_session_name.clone(),
                                        s.shell_tmux_session_name.clone(),
                                        s.worktree_path.clone(),
                                    )
                                })
                            })
                            .collect();
                        (repo_path, shell_tmux, sessions)
                    })
                };

                // 2. Remove from state immediately so the UI updates
                if let Err(e) = self
                    .store
                    .mutate(move |state| {
                        state.remove_project(&project_id);
                    })
                    .await
                {
                    self.ui_state.modal = Modal::Error {
                        message: format!("Failed to save state: {}", e),
                    };
                    return;
                }
                self.ui_state.selected_project_id = None;
                self.ui_state.status_message = Some((
                    "Project removed".to_string(),
                    Instant::now() + Duration::from_secs(3),
                ));
                self.refresh_list_items().await;

                // 3. Spawn background cleanup (kill all tmux sessions + remove worktrees)
                if let Some((repo_path, shell_tmux, sessions)) = cleanup_data {
                    let tmux = self.session_manager.tmux.clone();
                    let tx = self.event_loop.sender();
                    tokio::spawn(async move {
                        // Kill project shell tmux session
                        if let Some(ref shell_name) = shell_tmux {
                            let _ = tmux.kill_session(shell_name).await;
                        }
                        // Kill all session tmux sessions + remove worktrees
                        for (tmux_name, shell_tmux_name, worktree_path) in &sessions {
                            background::cleanup_session_tmux(
                                &tmux,
                                tmux_name,
                                shell_tmux_name.as_deref(),
                                Some((worktree_path.as_path(), repo_path.as_path())),
                                &tx,
                            )
                            .await;
                        }
                    });
                }
            }
        }
    }
}

/// Load the branch list for a repo path and convert each entry into
/// a `BranchEntry` suitable for the Checkout modal.
///
/// For branches that exist both locally and as remote tracking refs
/// we keep only the local entry — it's what we'd check out anyway.
pub(super) fn load_branch_entries(repo_path: &std::path::Path) -> Result<Vec<BranchEntry>> {
    let backend = crate::git::GitBackend::open(repo_path)?;
    let branches = backend.list_branches()?;

    let mut local_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entries: Vec<BranchEntry> = Vec::new();

    for (name, is_remote) in &branches {
        if !is_remote {
            local_names.insert(name.clone());
            entries.push(BranchEntry {
                local_name: name.clone(),
                display_name: name.clone(),
                is_remote: false,
            });
        }
    }

    for (name, is_remote) in &branches {
        if !is_remote {
            continue;
        }
        // "origin/foo" → local candidate "foo"
        let local = name
            .split_once('/')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_else(|| name.clone());
        if local_names.contains(&local) {
            // Already represented by the local branch — don't double-list.
            continue;
        }
        entries.push(BranchEntry {
            local_name: local,
            display_name: name.clone(),
            is_remote: true,
        });
    }

    Ok(entries)
}
