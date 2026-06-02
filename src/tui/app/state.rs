//! State management: state updates, session sync, list refresh, selection persistence.

use super::*;

impl App {
    pub(super) async fn handle_state_update(&mut self, update: StateUpdate) {
        match update {
            StateUpdate::ContentUpdated { session_id, .. } => {
                debug!("Content updated for session {}", session_id);
            }
            StateUpdate::StatusChanged { session_id } => {
                debug!("Status changed for session {}", session_id);
                self.refresh_list_items().await;
            }
            StateUpdate::SessionAdded { session_id } => {
                debug!("Session added: {}", session_id);
                self.refresh_list_items().await;
            }
            StateUpdate::SessionRemoved { session_id } => {
                debug!("Session removed: {}", session_id);
                self.refresh_list_items().await;
            }
            StateUpdate::PreviewReady {
                session_id,
                project_id,
                preview_content,
                diff_info,
                shell_content,
            } => {
                let elapsed = self
                    .ui_state
                    .preview_update_spawned_at
                    .map(|t| t.elapsed())
                    .unwrap_or_default();
                self.ui_state.preview_update_spawned_at = None;

                // Only apply if selection hasn't changed since the fetch started
                if session_id == self.ui_state.selected_session_id
                    && project_id == self.ui_state.selected_project_id
                {
                    debug!(
                        "Applying PreviewReady (preview_len={} diff_lines={} elapsed={:?})",
                        preview_content.len(),
                        diff_info.line_count,
                        elapsed
                    );
                    self.ui_state.preview_content = preview_content;
                    self.ui_state.diff_info = diff_info;
                    self.ui_state.shell_content = shell_content;
                } else {
                    debug!(
                        "Discarding stale PreviewReady (selection changed, elapsed={:?})",
                        elapsed
                    );
                }
            }
            StateUpdate::PrStatusReady { results } => {
                let sections = self.config.sections.clone();
                let now = chrono::Utc::now();
                let _ = self
                    .service
                    .store()
                    .mutate(move |state| {
                        for (session_id, result) in &results {
                            let Some(session) = state.get_session_mut(session_id) else {
                                continue;
                            };
                            match result {
                                PrCheckResult::Found(info) => {
                                    session.pr_number = Some(info.number);
                                    session.pr_url = Some(info.url.clone());
                                    session.pr_state = Some(info.state);
                                    session.pr_draft = info.is_draft;
                                    session.pr_labels = info.labels.clone();
                                    session.pr_merged = info.merged();
                                    session.review_decision = info.review_decision;
                                    session.pr_reviewers = info.reviewers.clone();
                                    session.pr_base_branch = info.base_ref_name.clone();
                                }
                                PrCheckResult::NotFound => {
                                    // Authoritative "no PR" — clear cached fields so
                                    // stale data (e.g. after a PR was deleted) doesn't
                                    // linger.
                                    session.pr_number = None;
                                    session.pr_url = None;
                                    session.pr_state = None;
                                    session.pr_draft = false;
                                    session.pr_labels.clear();
                                    session.pr_merged = false;
                                    session.review_decision = None;
                                    session.pr_reviewers.clear();
                                    session.pr_base_branch = None;
                                }
                                PrCheckResult::FetchFailed => {
                                    // Transient error (gh missing, network, auth) —
                                    // preserve cached PR state including `pr_base_branch`
                                    // so the PR-stack topology doesn't flicker off and
                                    // sessions don't collapse to a flat list.
                                }
                            }
                        }
                        for session in state.sessions.values_mut() {
                            crate::session::apply_assignment(session, &sections, now);
                        }
                    })
                    .await;

                // Update tmux status bars for running sessions with PR info.
                // Snapshot under the lock, then release before async tmux I/O.
                let status_bar_updates: Vec<_> = {
                    let state = self.service.store().read().await;
                    state
                        .sessions
                        .values()
                        .filter(|s| s.status == SessionStatus::Running)
                        .map(|s| {
                            let info = self.service.session_manager().status_bar_info(s, &state);
                            (s.tmux_session_name.clone(), info)
                        })
                        .collect()
                };
                for (tmux_name, info) in &status_bar_updates {
                    self.service
                        .session_manager()
                        .tmux
                        .configure_status_bar(tmux_name, info)
                        .await;
                }

                self.refresh_list_items().await;
            }
            StateUpdate::EnrichedPrReady { session_id, info } => {
                // Only apply if the session is still selected
                if self.ui_state.selected_session_id == Some(session_id) {
                    self.ui_state.enriched_pr = info.map(|pr| (session_id, pr));
                } else {
                    debug!("Discarding stale EnrichedPrReady for {}", session_id);
                }
            }
            StateUpdate::AiSummaryReady {
                session_id,
                result,
                diff_hash: hash,
            } => match result {
                Ok(text) => {
                    self.ui_state.ai_summaries.insert(
                        session_id,
                        AiSummary::Ready {
                            text,
                            diff_hash: hash,
                        },
                    );
                }
                Err(msg) => {
                    self.ui_state
                        .ai_summaries
                        .insert(session_id, AiSummary::Error(msg));
                }
            },
            StateUpdate::SessionCreated { session_id } => {
                debug!("Session created: {}", session_id);
                self.ui_state.modal = Modal::None;
                self.ui_state.status_message = Some((
                    format!("Created session {}", session_id),
                    Instant::now() + Duration::from_secs(3),
                ));
                self.reconcile_one_section_assignment(session_id).await;
                self.refresh_list_items().await;
                // Select the newly created session
                if let Some(idx) = self.ui_state.list_items.iter().position(|item| {
                    matches!(item, SessionListItem::Worktree { id, .. } if *id == session_id)
                }) {
                    self.ui_state.list_state.select(Some(idx));
                }
                self.update_selection();
                self.spawn_preview_update();
            }
            StateUpdate::SessionCreateFailed {
                session_id,
                message,
            } => {
                debug!("Session creation failed: {}", message);
                let _ = self
                    .service
                    .session_manager()
                    .remove_creating_session(&session_id)
                    .await;
                self.refresh_list_items().await;
                self.ui_state.modal = Modal::Error { message };
            }
            StateUpdate::AgentStatesUpdated { states } => {
                let unread_ids = detect_unread_transitions(&self.ui_state.agent_states, &states);
                if !unread_ids.is_empty() {
                    let _ = self
                        .service
                        .store()
                        .mutate(move |state| {
                            for sid in &unread_ids {
                                if let Some(session) = state.get_session_mut(sid) {
                                    session.unread = true;
                                }
                            }
                        })
                        .await;
                }
                self.ui_state.agent_states = states;
                self.refresh_list_items().await;
            }
            StateUpdate::CheckoutFetchComplete {
                project_id: updated_project,
                branches,
            } => {
                // Only apply if the Checkout modal is still open for the
                // same project. Re-build the entry list and re-run the
                // current filter so the highlighted branch stays sensible.
                if let Modal::CheckoutBranch {
                    project_id,
                    all_branches,
                    fetching,
                    ..
                } = &mut self.ui_state.modal
                    && *project_id == updated_project
                {
                    *fetching = false;

                    // Reconstruct BranchEntry list from (name, is_remote) pairs,
                    // mirroring `load_branch_entries`'s dedup behavior.
                    let mut local_names: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
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
                        let local = name
                            .split_once('/')
                            .map(|(_, rest)| rest.to_string())
                            .unwrap_or_else(|| name.clone());
                        if local_names.contains(&local) {
                            continue;
                        }
                        entries.push(BranchEntry {
                            local_name: local,
                            display_name: name.clone(),
                            is_remote: true,
                        });
                    }

                    *all_branches = entries;
                    self.refilter_checkout_branches();
                }
            }
            StateUpdate::ExternalChange => {
                debug!("External state change detected, refreshing UI");
                self.refresh_list_items().await;
            }
            StateUpdate::Error { message } => {
                self.ui_state.modal = Modal::Error { message };
            }
            StateUpdate::CascadeFinished { result } => {
                self.handle_cascade_finished(result).await;
            }
            StateUpdate::PushStackFinished { result } => {
                self.handle_push_stack_finished(result).await;
            }
            StateUpdate::ProjectPullFinished {
                project_id,
                outcome,
            } => {
                self.ui_state.project_pull_in_flight.remove(&project_id);
                self.ui_state
                    .last_project_pull
                    .insert(project_id, Instant::now());
                match outcome {
                    PullOutcome::Advanced => {
                        debug!("project pull: {} advanced", project_id);
                        self.ui_state.project_pull_blocked.remove(&project_id);
                        self.refresh_list_items().await;
                    }
                    PullOutcome::UpToDate => {
                        self.ui_state.project_pull_blocked.remove(&project_id);
                    }
                    PullOutcome::Blocked(reason) => {
                        debug!("project pull: {} blocked ({})", project_id, reason.as_str());
                        self.ui_state
                            .project_pull_blocked
                            .insert(project_id, reason);
                    }
                    PullOutcome::SoftFail => {
                        // Leave any existing blocked state alone: a fetch
                        // failure doesn't tell us anything new about the
                        // branch relation.
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) async fn cleanup_stale_creating_sessions(&self) {
        let creating_ids: Vec<SessionId> = {
            let state = self.service.store().read().await;
            state
                .sessions
                .values()
                .filter(|s| s.status == SessionStatus::Creating)
                .map(|s| s.id)
                .collect()
        };

        if !creating_ids.is_empty() {
            warn!(
                "Cleaning up {} stale Creating session(s) from previous run",
                creating_ids.len()
            );
            let _ = self
                .service
                .store()
                .mutate(move |state| {
                    for sid in &creating_ids {
                        state.remove_session(sid);
                    }
                })
                .await;
        }
    }

    /// Reset any sessions left in transient stack-operation states to `Running`.
    ///
    /// Both `Merging` and `Pushing` are transient — they're only valid while
    /// a cascade-merge or push-stack step is actively running. If the process
    /// died mid-op the git state is whatever it was, but the session-level
    /// status is stale and must be cleared so the UI doesn't show a spinner
    /// forever. `CascadePaused` is deliberately not touched: it's the durable
    /// signal that a conflict is outstanding, and pairs with the persisted
    /// `cascade_paused_at`.
    pub(super) async fn cleanup_stale_merging_sessions(&self) {
        let stale_ids: Vec<SessionId> = {
            let state = self.service.store().read().await;
            state
                .sessions
                .values()
                .filter(|s| matches!(s.status, SessionStatus::Merging | SessionStatus::Pushing))
                .map(|s| s.id)
                .collect()
        };

        if !stale_ids.is_empty() {
            warn!(
                "Resetting {} stale Merging/Pushing session(s) to Running",
                stale_ids.len()
            );
            let _ = self
                .service
                .store()
                .mutate(move |state| {
                    for sid in &stale_ids {
                        if let Some(session) = state.get_session_mut(sid) {
                            session.set_status(SessionStatus::Running);
                        }
                    }
                })
                .await;
        }
    }

    /// Sync app state with actual tmux session state
    ///
    /// This method checks all active sessions and updates their status
    /// if the corresponding tmux session no longer exists or the pane is dead.
    pub(super) async fn sync_session_states(&self) {
        let session_ids: Vec<(SessionId, String)> = {
            let state = self.service.store().read().await;
            state
                .sessions
                .values()
                .filter(|s| s.status.is_active() && s.status != SessionStatus::Creating)
                .map(|s| (s.id, s.tmux_session_name.clone()))
                .collect()
        };

        for (session_id, tmux_name) in session_ids {
            let should_mark_stopped = if let Ok(exists) = self
                .service
                .session_manager()
                .tmux
                .session_exists(&tmux_name)
                .await
            {
                if !exists {
                    true
                } else {
                    // Session exists, but check if pane is dead (program exited)
                    self.service
                        .session_manager()
                        .tmux
                        .is_pane_dead(&tmux_name)
                        .await
                        .unwrap_or(false)
                }
            } else {
                false
            };

            if should_mark_stopped {
                // Kill the tmux session if it exists but pane is dead
                let _ = self
                    .service
                    .session_manager()
                    .tmux
                    .kill_session(&tmux_name)
                    .await;

                let _ = self
                    .service
                    .store()
                    .mutate(move |state| {
                        if let Some(session) = state.get_session_mut(&session_id) {
                            session.set_status(SessionStatus::Stopped);
                        }
                    })
                    .await;
            }
        }

        // Sync unmanaged worktrees for all projects
        let project_ids: Vec<ProjectId> = {
            let state = self.service.store().read().await;
            state.projects.keys().copied().collect()
        };
        for project_id in project_ids {
            if let Err(e) = self
                .service
                .session_manager()
                .sync_worktrees(&project_id)
                .await
            {
                debug!("Failed to sync worktrees for project {}: {}", project_id, e);
            }
        }
    }

    /// Run `apply_assignment` over every session against the current
    /// `[[sections]]` config. Used at startup to reconcile state.json with
    /// possibly-changed config.
    pub(super) async fn reconcile_section_assignments(&mut self) {
        let sections = self.config.sections.clone();
        let now = chrono::Utc::now();
        let _ = self
            .service
            .store()
            .mutate(move |state| {
                if sections.is_empty()
                    && state.sessions.values().all(|s| s.current_section.is_none())
                {
                    return;
                }
                for session in state.sessions.values_mut() {
                    crate::session::apply_assignment(session, &sections, now);
                }
            })
            .await;
    }

    /// Run `apply_assignment` for a single session — used after creating a
    /// session, where the rest of the session set is already reconciled.
    pub(super) async fn reconcile_one_section_assignment(&mut self, session_id: SessionId) {
        if self.config.sections.is_empty() {
            return;
        }
        let sections = self.config.sections.clone();
        let now = chrono::Utc::now();
        let _ = self
            .service
            .store()
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&session_id) {
                    crate::session::apply_assignment(session, &sections, now);
                }
            })
            .await;
    }

    pub(super) async fn refresh_list_items(&mut self) {
        // Guard: if a section view is active but sections were removed from
        // config (hot-reload), fall back to ProjectGrouped.
        if matches!(
            self.ui_state.view_mode,
            ViewMode::SectionGrouped | ViewMode::SectionStacks
        ) && self.config.sections.is_empty()
        {
            self.ui_state.view_mode = ViewMode::ProjectGrouped;
        }

        let state = self.service.store().read().await;

        let items = match self.ui_state.view_mode {
            ViewMode::ProjectGrouped => {
                build_project_grouped_items(&state, &self.ui_state.agent_states)
            }
            ViewMode::SectionGrouped => build_section_grouped_items(
                &state,
                &self.config.sections,
                &self.ui_state.agent_states,
                &self.ui_state.collapsed_sections,
            ),
            ViewMode::SectionStacks => build_stacked_section_items(
                &state,
                &self.config.sections,
                &self.ui_state.agent_states,
                &self.ui_state.collapsed_sections,
            ),
        };

        let selectable: Vec<bool> = items.iter().map(|i| i.is_selectable()).collect();
        self.ui_state.list_items = items;
        self.ui_state.cascade_paused = state.cascade_paused_at.is_some();
        if matches!(self.ui_state.view_mode, ViewMode::ProjectGrouped) {
            self.ui_state
                .list_state
                .set_item_count(self.ui_state.list_items.len());
        } else {
            self.ui_state.list_state.set_selectable(selectable);
        }

        // Pre-compute stack chain for the selected session
        self.ui_state.stack_chain.clear();
        if let Some(session_id) = self.ui_state.selected_session_id
            && let Some(session) = state.sessions.get(&session_id)
        {
            let project_sessions: Vec<&WorktreeSession> = state
                .projects
                .get(&session.project_id)
                .map(|p| {
                    p.worktrees
                        .iter()
                        .filter_map(|sid| state.sessions.get(sid))
                        .collect()
                })
                .unwrap_or_default();
            // Walk up to the stack base
            let mut base = session_id;
            for _ in 0..project_sessions.len() {
                let base_session = project_sessions.iter().find(|s| s.id == base);
                match base_session
                    .and_then(|s| crate::session::resolve_stack_parent(s, &project_sessions))
                {
                    Some(parent) => base = parent,
                    None => break,
                }
            }
            let chain = crate::session::stack_chain_from_base(base, &project_sessions);
            if chain.len() > 1 {
                for &sid in &chain {
                    if let Some(s) = state.sessions.get(&sid) {
                        self.ui_state.stack_chain.push(StackChainEntry {
                            title: s.title.clone(),
                            status: s.status,
                            is_current: sid == session_id,
                        });
                    }
                }
            }
        }
    }

    /// Save current selection to persisted state
    pub(super) async fn save_selection(&self) {
        let session_id = self.ui_state.selected_session_id;
        let project_id = self.ui_state.selected_project_id;
        let _ = self
            .service
            .store()
            .mutate(move |state| {
                state.last_selected_session = session_id;
                state.last_selected_project = project_id;
            })
            .await;
    }

    /// Save left pane width to persisted state
    pub(super) async fn save_left_pane_pct(&self) {
        let pct = self.ui_state.left_pane_pct;
        let _ = self
            .service
            .store()
            .mutate(move |state| {
                state.left_pane_pct = Some(pct);
            })
            .await;
    }

    /// Restore selection and UI preferences from persisted state
    pub(super) async fn restore_selection(&mut self) {
        let (last_session, last_project, left_pane_pct) = {
            let state = self.service.store().read().await;
            (
                state.last_selected_session,
                state.last_selected_project,
                state.left_pane_pct,
            )
        };

        if let Some(pct) = left_pane_pct {
            self.ui_state.left_pane_pct = pct.clamp(MIN_LEFT_PANE_PCT, MAX_LEFT_PANE_PCT);
        }

        // Try to find the last selected session or project in the list
        let target_idx = self.ui_state.list_items.iter().position(|item| match item {
            SessionListItem::Worktree { id, .. } => last_session.is_some_and(|s| s == *id),
            SessionListItem::Project { id, .. } => {
                last_session.is_none() && last_project.is_some_and(|p| p == *id)
            }
            SessionListItem::SectionHeader { .. } | SessionListItem::Spacer => false,
        });

        if let Some(idx) = target_idx {
            self.ui_state.list_state.select(Some(idx));
        } else if !self.ui_state.list_items.is_empty() {
            self.ui_state.list_state.select(Some(0));
        }
    }
}

/// Compute the display order of a project's sessions, grouping each stack
/// directly under its base.
///
/// Returns `(session_id, stacked_child)` pairs in the order they should appear.
/// Root-list sessions (unstacked + stack bases) are sorted newest-first by
/// `created_at`; stacked children follow their root in parent→child (stack
/// position) order at the single deeper indent.
fn build_session_order(sessions: &[&WorktreeSession]) -> Vec<(SessionId, bool)> {
    let mut root_sessions: Vec<&WorktreeSession> = Vec::new();
    let mut children_by_parent: HashMap<SessionId, Vec<&WorktreeSession>> = HashMap::new();
    for s in sessions {
        match crate::session::resolve_stack_parent(s, sessions) {
            Some(parent_id) => {
                children_by_parent.entry(parent_id).or_default().push(s);
            }
            None => {
                root_sessions.push(s);
            }
        }
    }

    root_sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    for children in children_by_parent.values_mut() {
        children.sort_by_key(|s| s.created_at);
    }

    let mut out = Vec::new();
    for root in root_sessions {
        out.push((root.id, false));
        // to_visit is a LIFO stack; reverse the initial children and every
        // subsequent children-of-children push so pop() yields them in
        // ascending created_at order.
        let mut to_visit: Vec<&WorktreeSession> = children_by_parent
            .get(&root.id)
            .cloned()
            .unwrap_or_default();
        to_visit.reverse();
        while let Some(next) = to_visit.pop() {
            out.push((next.id, true));
            if let Some(grandchildren) = children_by_parent.get(&next.id) {
                for gc in grandchildren.iter().rev() {
                    to_visit.push(gc);
                }
            }
        }
    }
    out
}

fn worktree_item(
    session: &crate::session::WorktreeSession,
    agent_states: &HashMap<SessionId, AgentState>,
    project_name_prefix: Option<&str>,
    stacked_child: bool,
) -> SessionListItem {
    let title = match project_name_prefix {
        Some(prefix) => format!("{}/{}", prefix, session.title),
        None => session.title.clone(),
    };
    SessionListItem::Worktree {
        id: session.id,
        project_id: session.project_id,
        title,
        branch: session.branch.clone(),
        status: session.status,
        program: session.program.clone(),
        pr_number: session.pr_number,
        pr_url: session.pr_url.clone(),
        pr_merged: session.pr_merged,
        pr_state: session.pr_state,
        pr_draft: session.pr_draft,
        pr_labels: session.pr_labels.clone(),
        worktree_path: session.worktree_path.clone(),
        created_at: session.created_at,
        agent_state: agent_states.get(&session.id).copied(),
        unread: session.unread,
        stacked_child,
    }
}

fn build_project_grouped_items(
    state: &crate::config::AppState,
    agent_states: &HashMap<SessionId, AgentState>,
) -> Vec<SessionListItem> {
    let mut items = Vec::new();
    let mut projects: Vec<_> = state.projects.values().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));

    for project in projects {
        items.push(SessionListItem::Project {
            id: project.id,
            name: project.name.clone(),
            repo_path: project.repo_path.clone(),
            main_branch: project.main_branch.clone(),
            worktree_count: project.worktrees.len(),
            nested: false,
        });

        // Use stack-aware ordering so stacked children render indented
        // directly beneath their stack base.
        let sessions: Vec<&WorktreeSession> = project
            .worktrees
            .iter()
            .filter_map(|sid| state.sessions.get(sid))
            .collect();
        for (sid, stacked_child) in build_session_order(&sessions) {
            if let Some(session) = state.sessions.get(&sid) {
                items.push(worktree_item(session, agent_states, None, stacked_child));
            }
        }
    }
    items
}

fn build_section_grouped_items(
    state: &crate::config::AppState,
    sections: &[crate::session::SectionConfig],
    agent_states: &HashMap<SessionId, AgentState>,
    collapsed_sections: &std::collections::HashSet<String>,
) -> Vec<SessionListItem> {
    let sessions: Vec<crate::session::WorktreeSession> = state.sessions.values().cloned().collect();
    let groups = crate::session::build_sections(&sessions, sections);

    let mut projects: Vec<_> = state.projects.values().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));

    let mut items = Vec::new();
    for (group_idx, group) in groups.iter().enumerate() {
        if group_idx > 0 {
            items.push(SessionListItem::Spacer);
        }
        let collapsed = collapsed_sections.contains(&group.name);
        items.push(SessionListItem::SectionHeader {
            name: group.name.clone(),
            count: group.sessions.len(),
            collapsed,
        });

        if collapsed {
            continue;
        }

        let is_in_progress = group.name == crate::session::IN_PROGRESS;
        // Preserve group sort order (oldest-first) while partitioning by project.
        let mut by_project: std::collections::HashMap<crate::session::ProjectId, Vec<SessionId>> =
            Default::default();
        let mut project_order: Vec<crate::session::ProjectId> = Vec::new();
        for sid in &group.sessions {
            if let Some(session) = state.sessions.get(sid) {
                by_project.entry(session.project_id).or_default().push(*sid);
                if !project_order.contains(&session.project_id) {
                    project_order.push(session.project_id);
                }
            }
        }

        for project in &projects {
            let project_sessions = by_project.get(&project.id);
            let count = project_sessions.map(|v| v.len()).unwrap_or(0);
            // In Progress shows every project (even empty ones); other
            // sections only show projects that have sessions in them.
            if !is_in_progress && count == 0 {
                continue;
            }
            items.push(SessionListItem::Project {
                id: project.id,
                name: project.name.clone(),
                repo_path: project.repo_path.clone(),
                main_branch: project.main_branch.clone(),
                worktree_count: count,
                nested: true,
            });
            if let Some(sids) = project_sessions {
                for sid in sids {
                    if let Some(session) = state.sessions.get(sid) {
                        items.push(worktree_item(session, agent_states, None, false));
                    }
                }
            }
        }
    }
    items
}

/// Like `build_section_grouped_items` but treats each PR stack as one unit:
/// the whole stack lands in the section chosen by its newest leaf (with
/// `section_override` walked closest-to-leaf-first), and stack indentation is
/// preserved via `stacked_child: true` on non-root members.
fn build_stacked_section_items(
    state: &crate::config::AppState,
    sections: &[crate::session::SectionConfig],
    agent_states: &HashMap<SessionId, AgentState>,
    collapsed_sections: &std::collections::HashSet<String>,
) -> Vec<SessionListItem> {
    use chrono::{DateTime, Utc};

    #[derive(Clone)]
    struct GroupRender {
        sort_key: DateTime<Utc>,
        // Tiebreaker for groups whose leaves share an entered_section_at —
        // common when one apply_assignment pass stamps multiple sessions
        // with the same `now`. Without a stable tiebreaker, HashMap-
        // randomised insertion order leaks into the sort and the UI
        // appears to churn on every refresh.
        leaf_id: SessionId,
        order: Vec<(SessionId, bool)>,
    }

    // Stable project order. Ties on `name` (unusual but possible) fall
    // back to project id so we never depend on HashMap iteration order.
    let mut projects: Vec<_> = state.projects.values().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));

    // section_name → project_id → Vec<GroupRender>
    let mut by_section: std::collections::HashMap<
        String,
        std::collections::HashMap<crate::session::ProjectId, Vec<GroupRender>>,
    > = std::collections::HashMap::new();
    let valid_section =
        |name: &str| name == crate::session::IN_PROGRESS || sections.iter().any(|s| s.name == name);

    for project in &projects {
        // Sort by (created_at, id) so any downstream max_by_key on this
        // slice (e.g. fan-out children with identical created_at in
        // `stack_top`) picks a deterministic winner.
        let mut project_sessions: Vec<&WorktreeSession> = state
            .sessions
            .values()
            .filter(|s| s.project_id == project.id)
            .collect();
        if project_sessions.is_empty() {
            continue;
        }
        project_sessions.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));

        // Bucket every session by its stack root (returns self for
        // unstacked). Track first-encounter root order so we iterate
        // groups deterministically below without leaking
        // HashMap-iteration order into the output.
        let mut groups: std::collections::HashMap<SessionId, Vec<&WorktreeSession>> =
            std::collections::HashMap::new();
        let mut group_roots: Vec<SessionId> = Vec::new();
        for s in &project_sessions {
            let root_id = crate::session::stack_root(s.id, &project_sessions);
            if !groups.contains_key(&root_id) {
                group_roots.push(root_id);
            }
            groups.entry(root_id).or_default().push(s);
        }

        for root_id in group_roots {
            let members = groups.remove(&root_id).unwrap_or_default();
            // Pick the leaf in the whole subgraph; its section drives placement.
            let leaf_id = crate::session::stack_top(root_id, &project_sessions);
            let Some(leaf) = members.iter().find(|s| s.id == leaf_id).copied() else {
                continue;
            };

            // Walk leaf → root. First section_override encountered wins;
            // overrides on off-path siblings are not considered.
            let mut effective: Option<String> = None;
            let mut cursor = leaf.id;
            for _ in 0..project_sessions.len() {
                let Some(cur) = project_sessions.iter().find(|s| s.id == cursor).copied() else {
                    break;
                };
                if let Some(ovr) = &cur.section_override {
                    effective = Some(ovr.clone());
                    break;
                }
                match crate::session::resolve_stack_parent(cur, &project_sessions) {
                    Some(parent) => cursor = parent,
                    None => break,
                }
            }
            let section_name = effective
                .or_else(|| leaf.current_section.clone())
                .filter(|n| valid_section(n))
                .unwrap_or_else(|| crate::session::IN_PROGRESS.to_string());

            // Order within the group: stack-aware (root first, children
            // indented). build_session_order resolves parents only against
            // the slice it's given, so passing just the group's members
            // keeps the root flat and descendants indented even when the
            // subgraph fans out.
            let order = build_session_order(&members);

            by_section
                .entry(section_name)
                .or_default()
                .entry(project.id)
                .or_default()
                .push(GroupRender {
                    sort_key: leaf.entered_section_at,
                    leaf_id,
                    order,
                });
        }
    }

    // Emit IN_PROGRESS first, then user sections in declared order — same
    // overall layout the plain section view uses.
    let section_order: Vec<String> = std::iter::once(crate::session::IN_PROGRESS.to_string())
        .chain(sections.iter().map(|s| s.name.clone()))
        .collect();

    let mut items = Vec::new();
    for (idx, section_name) in section_order.iter().enumerate() {
        if idx > 0 {
            items.push(SessionListItem::Spacer);
        }

        let project_groups = by_section.get(section_name);
        let total_count: usize = project_groups
            .map(|m| {
                m.values()
                    .flat_map(|g| g.iter())
                    .map(|g| g.order.len())
                    .sum()
            })
            .unwrap_or(0);

        let collapsed = collapsed_sections.contains(section_name);
        items.push(SessionListItem::SectionHeader {
            name: section_name.clone(),
            count: total_count,
            collapsed,
        });
        if collapsed {
            continue;
        }

        let is_in_progress = section_name == crate::session::IN_PROGRESS;
        for project in &projects {
            let mut groups_in_proj = project_groups
                .and_then(|m| m.get(&project.id))
                .cloned()
                .unwrap_or_default();
            let project_count: usize = groups_in_proj.iter().map(|g| g.order.len()).sum();
            if !is_in_progress && project_count == 0 {
                continue;
            }
            items.push(SessionListItem::Project {
                id: project.id,
                name: project.name.clone(),
                repo_path: project.repo_path.clone(),
                main_branch: project.main_branch.clone(),
                worktree_count: project_count,
                nested: true,
            });

            // Stacks within a section sort by their leaf's
            // entered_section_at, with leaf_id as a stable tiebreaker so
            // batched apply_assignment calls (which stamp many sessions
            // with the same `now`) don't make the view churn.
            groups_in_proj
                .sort_by(|a, b| a.sort_key.cmp(&b.sort_key).then(a.leaf_id.cmp(&b.leaf_id)));
            for group in groups_in_proj {
                for (sid, stacked_child) in group.order {
                    if let Some(session) = state.sessions.get(&sid) {
                        items.push(worktree_item(session, agent_states, None, stacked_child));
                    }
                }
            }
        }
    }
    items
}

fn detect_unread_transitions(
    prev: &HashMap<SessionId, AgentState>,
    new: &HashMap<SessionId, AgentState>,
) -> Vec<SessionId> {
    let mut ids = Vec::new();
    for (session_id, new_state) in new {
        if *new_state == AgentState::Idle && prev.get(session_id) == Some(&AgentState::Working) {
            ids.push(*session_id);
        }
    }
    ids
}

#[cfg(test)]
mod unread_transition_tests {
    use super::*;
    use crate::session::SessionId;
    use std::collections::HashMap;

    #[test]
    fn working_to_idle_marks_unread() {
        let sid = SessionId::new();
        let prev = HashMap::from([(sid, AgentState::Working)]);
        let new = HashMap::from([(sid, AgentState::Idle)]);
        assert_eq!(detect_unread_transitions(&prev, &new), vec![sid]);
    }

    #[test]
    fn idle_to_idle_no_transition() {
        let sid = SessionId::new();
        let prev = HashMap::from([(sid, AgentState::Idle)]);
        let new = HashMap::from([(sid, AgentState::Idle)]);
        assert!(detect_unread_transitions(&prev, &new).is_empty());
    }

    #[test]
    fn empty_cache_no_transition() {
        let sid = SessionId::new();
        let prev = HashMap::new();
        let new = HashMap::from([(sid, AgentState::Idle)]);
        assert!(
            detect_unread_transitions(&prev, &new).is_empty(),
            "cleared cache after attach must not trigger false unread"
        );
    }

    #[test]
    fn empty_cache_working_no_transition() {
        let sid = SessionId::new();
        let prev = HashMap::new();
        let new = HashMap::from([(sid, AgentState::Working)]);
        assert!(detect_unread_transitions(&prev, &new).is_empty());
    }
}

#[cfg(test)]
mod stack_order_tests {
    use super::*;
    use crate::session::{ProjectId, WorktreeSession};
    use chrono::{Duration as ChronoDuration, Utc};
    use std::path::PathBuf;

    fn make_session(title: &str, branch: &str, created_offset_secs: i64) -> WorktreeSession {
        let mut s = WorktreeSession::new(
            ProjectId::new(),
            title,
            branch,
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        s.created_at = Utc::now() + ChronoDuration::seconds(created_offset_secs);
        s
    }

    #[test]
    fn ordering_unstacked_only_sorts_newest_first() {
        let a = make_session("a", "a", 0);
        let b = make_session("b", "b", 10);
        let c = make_session("c", "c", 20);
        let order = build_session_order(&[&a, &b, &c]);
        assert_eq!(
            order,
            vec![(c.id, false), (b.id, false), (a.id, false)],
            "newer sessions should appear first at the root level"
        );
    }

    #[test]
    fn ordering_single_stack_emits_base_then_children_in_stack_order() {
        // base (oldest) ← child1 ← child2; all stacked, base at root indent.
        let base = make_session("base", "base-br", 0);
        let mut child1 = make_session("c1", "c1-br", 5);
        child1.stack_parent_session_id = Some(base.id);
        let mut child2 = make_session("c2", "c2-br", 10);
        child2.stack_parent_session_id = Some(child1.id);

        let order = build_session_order(&[&base, &child1, &child2]);
        assert_eq!(
            order,
            vec![(base.id, false), (child1.id, true), (child2.id, true)]
        );
    }

    #[test]
    fn ordering_two_independent_stacks_interleave_by_base_created_at() {
        // Two stacks; their bases sort by created_at among root rows. Each
        // stack's children appear directly beneath its base.
        let base_a = make_session("base-a", "base-a", 0);
        let mut child_a = make_session("child-a", "child-a", 1);
        child_a.stack_parent_session_id = Some(base_a.id);

        let base_b = make_session("base-b", "base-b", 20);
        let mut child_b = make_session("child-b", "child-b", 21);
        child_b.stack_parent_session_id = Some(base_b.id);

        let order = build_session_order(&[&base_a, &child_a, &base_b, &child_b]);
        assert_eq!(
            order,
            vec![
                (base_b.id, false), // newer base first at root level
                (child_b.id, true),
                (base_a.id, false),
                (child_a.id, true),
            ]
        );
    }

    #[test]
    fn ordering_mixed_stack_and_unstacked_interleaves_correctly() {
        let base = make_session("base", "base", 0);
        let mut child = make_session("child", "child", 5);
        child.stack_parent_session_id = Some(base.id);
        let solo = make_session("solo", "solo", 10);
        let order = build_session_order(&[&base, &child, &solo]);
        assert_eq!(
            order,
            vec![
                (solo.id, false), // newest root first
                (base.id, false),
                (child.id, true), // follows its base
            ]
        );
    }

    #[test]
    fn ordering_orphan_stack_parent_is_treated_as_root() {
        let mut orphan = make_session("orphan", "orphan", 0);
        orphan.stack_parent_session_id = Some(SessionId::new()); // dangling
        let order = build_session_order(&[&orphan]);
        assert_eq!(order, vec![(orphan.id, false)]);
    }

    #[test]
    fn ordering_sibling_children_of_same_base_both_indent() {
        // Two sessions sharing one base — both should render as stacked
        // children, in created_at order, both indented.
        let base = make_session("base", "base", 0);
        let mut c1 = make_session("c1", "c1", 5);
        c1.stack_parent_session_id = Some(base.id);
        let mut c2 = make_session("c2", "c2", 10);
        c2.stack_parent_session_id = Some(base.id);
        let order = build_session_order(&[&base, &c1, &c2]);
        assert_eq!(order, vec![(base.id, false), (c1.id, true), (c2.id, true)]);
    }

    #[test]
    fn ordering_pr_base_matching_session_forms_stack() {
        // No local link — GitHub PR info alone should form the stack.
        let base = make_session("base", "base-br", 0);
        let mut child = make_session("child", "child-br", 5);
        child.pr_base_branch = Some("base-br".to_string());
        let order = build_session_order(&[&base, &child]);
        assert_eq!(order, vec![(base.id, false), (child.id, true)]);
    }

    fn make_session_in_section(
        title: &str,
        branch: &str,
        created_offset_secs: i64,
        current_section: &str,
    ) -> WorktreeSession {
        let mut s = make_session(title, branch, created_offset_secs);
        s.current_section = Some(current_section.to_string());
        // Stamp section-entry time to mirror the created offset so the leaf's
        // entered_section_at uniquely identifies the group's sort position.
        s.entered_section_at = Utc::now() + ChronoDuration::seconds(created_offset_secs);
        s
    }

    fn section_named(name: &str) -> crate::session::SectionConfig {
        crate::session::SectionConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn appstate_from(sessions: Vec<WorktreeSession>) -> crate::config::AppState {
        let mut state = crate::config::AppState::default();
        // Group sessions by their project_id so projects with multiple
        // worktrees stay linked correctly.
        let mut project_titles: std::collections::HashMap<ProjectId, String> = Default::default();
        for s in &sessions {
            project_titles
                .entry(s.project_id)
                .or_insert_with(|| format!("p-{}", &s.project_id.to_string()));
        }
        for (pid, name) in project_titles {
            let mut project = crate::session::Project::new(&name, PathBuf::from("/tmp"), "main");
            project.id = pid;
            state.projects.insert(pid, project);
        }
        for s in sessions {
            let pid = s.project_id;
            state.projects.get_mut(&pid).unwrap().add_worktree(s.id);
            state.sessions.insert(s.id, s);
        }
        state
    }

    #[test]
    fn stacked_sections_group_whole_stack_under_leaf_section() {
        // base.current_section = "Review" (older), child.current_section = "Open" (leaf).
        // In the new stacked-section view the whole stack should appear under
        // "Open" (the leaf's section), with `child` indented beneath `base`.
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        let mut child = make_session_in_section("child", "child", 10, "Open");
        child.project_id = project_id;
        child.stack_parent_session_id = Some(base.id);

        let state = appstate_from(vec![base.clone(), child.clone()]);
        let sections = vec![section_named("Open"), section_named("Review")];
        let agent_states = HashMap::new();
        let collapsed = std::collections::HashSet::new();

        let items = build_stacked_section_items(&state, &sections, &agent_states, &collapsed);

        // Walk items: find the "Open" header, then base+child should follow.
        let found_open = items.iter().any(
            |item| matches!(item, SessionListItem::SectionHeader { name, .. } if name == "Open"),
        );
        assert!(
            found_open,
            "Open section header should be present: {items:?}"
        );

        // After the Open header, expect: Project → base (stacked_child:false) → child (stacked_child:true)
        let after = items
            .iter()
            .skip_while(
                |i| !matches!(i, SessionListItem::SectionHeader { name, .. } if name == "Open"),
            )
            .skip(1)
            .collect::<Vec<_>>();
        let session_rows: Vec<_> = after
            .iter()
            .filter_map(|i| match i {
                SessionListItem::Worktree {
                    id, stacked_child, ..
                } => Some((*id, *stacked_child)),
                _ => None,
            })
            .take_while(|_| true)
            .collect();
        assert_eq!(
            session_rows,
            vec![(base.id, false), (child.id, true)],
            "stack should render under leaf's section with indentation preserved"
        );

        // And there should be no Worktree row under "Review".
        let review_rows: Vec<_> = items
            .iter()
            .skip_while(
                |i| !matches!(i, SessionListItem::SectionHeader { name, .. } if name == "Review"),
            )
            .skip(1)
            .take_while(|i| {
                !matches!(
                    i,
                    SessionListItem::SectionHeader { .. } | SessionListItem::Spacer
                )
            })
            .filter(|i| matches!(i, SessionListItem::Worktree { .. }))
            .collect();
        assert!(
            review_rows.is_empty(),
            "stack-base should have moved out of Review section: {review_rows:?}"
        );
    }

    #[test]
    fn stacked_sections_override_closest_to_leaf_wins() {
        // base has section_override = "Pinned"; child (leaf) has no override.
        // Whole stack lands in "Pinned".
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        base.section_override = Some("Pinned".to_string());
        let mut child = make_session_in_section("child", "child", 10, "Open");
        child.project_id = project_id;
        child.stack_parent_session_id = Some(base.id);

        let state = appstate_from(vec![base.clone(), child.clone()]);
        let sections = vec![
            section_named("Open"),
            section_named("Review"),
            section_named("Pinned"),
        ];
        let items = build_stacked_section_items(
            &state,
            &sections,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );

        let pinned_rows: Vec<_> = items
            .iter()
            .skip_while(
                |i| !matches!(i, SessionListItem::SectionHeader { name, .. } if name == "Pinned"),
            )
            .skip(1)
            .take_while(|i| {
                !matches!(
                    i,
                    SessionListItem::SectionHeader { .. } | SessionListItem::Spacer
                )
            })
            .filter_map(|i| match i {
                SessionListItem::Worktree {
                    id, stacked_child, ..
                } => Some((*id, *stacked_child)),
                _ => None,
            })
            .collect();
        assert_eq!(
            pinned_rows,
            vec![(base.id, false), (child.id, true)],
            "whole stack should land in the closest-to-leaf overridden section"
        );
    }

    #[test]
    fn stacked_sections_fan_out_groups_share_root_and_use_newest_leaf() {
        // Fan-out: base has two children, B (older) and C (newer).
        // C is the newest leaf in the subgraph, so the whole stack (base+B+C)
        // appears under C's current_section.
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        let mut b = make_session_in_section("b", "b", 5, "Review");
        b.project_id = project_id;
        b.stack_parent_session_id = Some(base.id);
        let mut c = make_session_in_section("c", "c", 20, "Open");
        c.project_id = project_id;
        c.stack_parent_session_id = Some(base.id);

        let state = appstate_from(vec![base.clone(), b.clone(), c.clone()]);
        let sections = vec![section_named("Open"), section_named("Review")];
        let items = build_stacked_section_items(
            &state,
            &sections,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );

        let open_session_ids: Vec<_> = items
            .iter()
            .skip_while(
                |i| !matches!(i, SessionListItem::SectionHeader { name, .. } if name == "Open"),
            )
            .skip(1)
            .take_while(|i| {
                !matches!(
                    i,
                    SessionListItem::SectionHeader { .. } | SessionListItem::Spacer
                )
            })
            .filter_map(|i| match i {
                SessionListItem::Worktree { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert!(
            open_session_ids.contains(&base.id)
                && open_session_ids.contains(&b.id)
                && open_session_ids.contains(&c.id),
            "all three subgraph members should appear under Open: {open_session_ids:?}"
        );

        let review_session_count = items
            .iter()
            .skip_while(
                |i| !matches!(i, SessionListItem::SectionHeader { name, .. } if name == "Review"),
            )
            .skip(1)
            .take_while(|i| {
                !matches!(
                    i,
                    SessionListItem::SectionHeader { .. } | SessionListItem::Spacer
                )
            })
            .filter(|i| matches!(i, SessionListItem::Worktree { .. }))
            .count();
        assert_eq!(
            review_session_count, 0,
            "Review should be empty because B follows the stack into Open"
        );
    }

    #[test]
    fn stacked_sections_sibling_override_off_leaf_path_is_ignored() {
        // base ← B (override "Pinned"), base ← C (newer leaf, no override).
        // The chosen leaf is C; walking C → base, B is off-path so its
        // override doesn't count. Stack goes to C's current_section.
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        let mut b = make_session_in_section("b", "b", 5, "Review");
        b.project_id = project_id;
        b.stack_parent_session_id = Some(base.id);
        b.section_override = Some("Pinned".to_string());
        let mut c = make_session_in_section("c", "c", 20, "Open");
        c.project_id = project_id;
        c.stack_parent_session_id = Some(base.id);

        let state = appstate_from(vec![base.clone(), b.clone(), c.clone()]);
        let sections = vec![
            section_named("Open"),
            section_named("Review"),
            section_named("Pinned"),
        ];
        let items = build_stacked_section_items(
            &state,
            &sections,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );

        let pinned_count = items
            .iter()
            .skip_while(
                |i| !matches!(i, SessionListItem::SectionHeader { name, .. } if name == "Pinned"),
            )
            .skip(1)
            .take_while(|i| {
                !matches!(
                    i,
                    SessionListItem::SectionHeader { .. } | SessionListItem::Spacer
                )
            })
            .filter(|i| matches!(i, SessionListItem::Worktree { .. }))
            .count();
        assert_eq!(
            pinned_count, 0,
            "off-leaf-path overrides should not pull the stack into their section"
        );

        let open_count = items
            .iter()
            .skip_while(
                |i| !matches!(i, SessionListItem::SectionHeader { name, .. } if name == "Open"),
            )
            .skip(1)
            .take_while(|i| {
                !matches!(
                    i,
                    SessionListItem::SectionHeader { .. } | SessionListItem::Spacer
                )
            })
            .filter(|i| matches!(i, SessionListItem::Worktree { .. }))
            .count();
        assert_eq!(
            open_count, 3,
            "all three subgraph members should appear under Open: {items:?}"
        );
    }

    #[test]
    fn stacked_sections_output_stable_across_repeated_calls_with_tied_timestamps() {
        // Two stacks whose leaves both entered the section at the same
        // instant (apply_assignment uses one `now` for the whole batch).
        // Output must be deterministic between calls or the UI churns on
        // every refresh.
        let project_id = ProjectId::new();
        let same_ts = Utc::now();
        let mk = |title: &str, branch: &str| {
            let mut s = make_session_in_section(title, branch, 0, "Open");
            s.project_id = project_id;
            s.entered_section_at = same_ts;
            s
        };
        let mut base_a = mk("base-a", "base-a");
        base_a.created_at = same_ts;
        let mut child_a = mk("child-a", "child-a");
        child_a.created_at = same_ts;
        child_a.stack_parent_session_id = Some(base_a.id);
        let mut base_b = mk("base-b", "base-b");
        base_b.created_at = same_ts;
        let mut child_b = mk("child-b", "child-b");
        child_b.created_at = same_ts;
        child_b.stack_parent_session_id = Some(base_b.id);

        let state = appstate_from(vec![
            base_a.clone(),
            child_a.clone(),
            base_b.clone(),
            child_b.clone(),
        ]);
        let sections = vec![section_named("Open")];
        let collapsed = std::collections::HashSet::new();

        let first = build_stacked_section_items(&state, &sections, &HashMap::new(), &collapsed);
        // Call many times — every iteration constructs fresh internal
        // HashMaps with a new RandomState, so any non-determinism shows up
        // here. 32 calls is well past the birthday-paradox threshold.
        for _ in 0..32 {
            let again = build_stacked_section_items(&state, &sections, &HashMap::new(), &collapsed);
            assert_eq!(
                again, first,
                "build_stacked_section_items must produce identical output on every call"
            );
        }
    }

    #[test]
    fn stacked_sections_sort_groups_by_leaf_entered_section_at() {
        // Two unstacked sessions both in "Open"; the newer one's
        // entered_section_at should sort it... wait — build_sections sorts
        // oldest-first within a section. The newer leaf has a later
        // entered_section_at and therefore appears AFTER the older one.
        let project_id = ProjectId::new();
        let mut older = make_session_in_section("older", "older", 0, "Open");
        older.project_id = project_id;
        let mut newer = make_session_in_section("newer", "newer", 20, "Open");
        newer.project_id = project_id;

        let state = appstate_from(vec![older.clone(), newer.clone()]);
        let sections = vec![section_named("Open")];
        let items = build_stacked_section_items(
            &state,
            &sections,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );

        let open_session_order: Vec<_> = items
            .iter()
            .skip_while(
                |i| !matches!(i, SessionListItem::SectionHeader { name, .. } if name == "Open"),
            )
            .skip(1)
            .take_while(|i| {
                !matches!(
                    i,
                    SessionListItem::SectionHeader { .. } | SessionListItem::Spacer
                )
            })
            .filter_map(|i| match i {
                SessionListItem::Worktree { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(
            open_session_order,
            vec![older.id, newer.id],
            "Open section should be sorted by entered_section_at (oldest first)"
        );
    }

    #[test]
    fn ordering_pr_base_matching_main_pops_child_to_root() {
        // When the PR retargets main (e.g. after the prior stack member was
        // merged), the child becomes a stack root — both base and ex-child are
        // root-level siblings.
        let base = make_session("base", "base-br", 0);
        let mut child = make_session("child", "child-br", 5);
        child.pr_base_branch = Some("main".to_string());
        // Local link still hanging around — PR data wins.
        child.stack_parent_session_id = Some(base.id);
        let order = build_session_order(&[&base, &child]);
        assert_eq!(
            order,
            vec![(child.id, false), (base.id, false)],
            "child with PR targeting main should pop to the root list"
        );
    }
}
