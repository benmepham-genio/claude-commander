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
                    .store
                    .mutate(move |state| {
                        for (session_id, pr_info) in &results {
                            if let Some(session) = state.get_session_mut(session_id) {
                                session.pr_number = pr_info.as_ref().map(|p| p.number);
                                session.pr_url = pr_info.as_ref().map(|p| p.url.clone());
                                session.pr_state = pr_info.as_ref().map(|p| p.state);
                                session.pr_draft = pr_info.as_ref().is_some_and(|p| p.is_draft);
                                session.pr_labels = pr_info
                                    .as_ref()
                                    .map(|p| p.labels.clone())
                                    .unwrap_or_default();
                                session.pr_merged = pr_info.as_ref().is_some_and(|p| p.merged());
                                session.review_decision =
                                    pr_info.as_ref().and_then(|p| p.review_decision);
                            }
                        }
                        for session in state.sessions.values_mut() {
                            crate::session::apply_assignment(session, &sections, now);
                        }
                    })
                    .await;

                // Update tmux status bars for running sessions with PR info
                {
                    let state = self.store.read().await;
                    for session in state.sessions.values() {
                        if session.status == SessionStatus::Running {
                            let info = self.session_manager.status_bar_info(session, &state);
                            self.session_manager
                                .tmux
                                .configure_status_bar(&session.tmux_session_name, &info)
                                .await;
                        }
                    }
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
                self.reconcile_section_assignments().await;
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
                    .session_manager
                    .remove_creating_session(&session_id)
                    .await;
                self.refresh_list_items().await;
                self.ui_state.modal = Modal::Error { message };
            }
            StateUpdate::AgentStatesUpdated { states } => {
                // Detect Working → Idle transitions and mark sessions as unread
                let mut unread_ids = Vec::new();
                for (session_id, new_state) in &states {
                    if *new_state == AgentState::Idle
                        && self.ui_state.agent_states.get(session_id) == Some(&AgentState::Working)
                    {
                        unread_ids.push(*session_id);
                    }
                }
                if !unread_ids.is_empty() {
                    let _ = self
                        .store
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
            _ => {}
        }
    }

    pub(super) async fn cleanup_stale_creating_sessions(&self) {
        let creating_ids: Vec<SessionId> = {
            let state = self.store.read().await;
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
                .store
                .mutate(move |state| {
                    for sid in &creating_ids {
                        state.remove_session(sid);
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
            let state = self.store.read().await;
            state
                .sessions
                .values()
                .filter(|s| s.status.is_active() && s.status != SessionStatus::Creating)
                .map(|s| (s.id, s.tmux_session_name.clone()))
                .collect()
        };

        for (session_id, tmux_name) in session_ids {
            let should_mark_stopped =
                if let Ok(exists) = self.session_manager.tmux.session_exists(&tmux_name).await {
                    if !exists {
                        true
                    } else {
                        // Session exists, but check if pane is dead (program exited)
                        self.session_manager
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
                let _ = self.session_manager.tmux.kill_session(&tmux_name).await;

                let _ = self
                    .store
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
            let state = self.store.read().await;
            state.projects.keys().copied().collect()
        };
        for project_id in project_ids {
            if let Err(e) = self.session_manager.sync_worktrees(&project_id).await {
                debug!("Failed to sync worktrees for project {}: {}", project_id, e);
            }
        }
    }

    /// Run `apply_assignment` over every session against the current
    /// `[[sections]]` config. Used at startup (reconcile state.json with
    /// possibly-changed config) and after creating a new session.
    pub(super) async fn reconcile_section_assignments(&mut self) {
        let sections = self.config.sections.clone();
        if sections.is_empty()
            && self
                .store
                .read()
                .await
                .sessions
                .values()
                .all(|s| s.current_section.is_none())
        {
            return;
        }
        let now = chrono::Utc::now();
        let _ = self
            .store
            .mutate(move |state| {
                for session in state.sessions.values_mut() {
                    crate::session::apply_assignment(session, &sections, now);
                }
            })
            .await;
    }

    pub(super) async fn refresh_list_items(&mut self) {
        let state = self.store.read().await;

        let items = if self.config.sections.is_empty() {
            build_project_grouped_items(&state, &self.ui_state.agent_states)
        } else {
            build_section_grouped_items(&state, &self.config.sections, &self.ui_state.agent_states)
        };

        let selectable: Vec<bool> = items.iter().map(|i| i.is_selectable()).collect();
        self.ui_state.list_items = items;
        if self.config.sections.is_empty() {
            self.ui_state
                .list_state
                .set_item_count(self.ui_state.list_items.len());
        } else {
            self.ui_state.list_state.set_selectable(selectable);
        }
    }

    /// Save current selection to persisted state
    pub(super) async fn save_selection(&self) {
        let session_id = self.ui_state.selected_session_id;
        let project_id = self.ui_state.selected_project_id;
        let _ = self
            .store
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
            .store
            .mutate(move |state| {
                state.left_pane_pct = Some(pct);
            })
            .await;
    }

    /// Restore selection and UI preferences from persisted state
    pub(super) async fn restore_selection(&mut self) {
        let (last_session, last_project, left_pane_pct) = {
            let state = self.store.read().await;
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
            SessionListItem::SectionHeader { .. } => false,
        });

        if let Some(idx) = target_idx {
            self.ui_state.list_state.select(Some(idx));
        } else if !self.ui_state.list_items.is_empty() {
            self.ui_state.list_state.select(Some(0));
        }
    }
}

fn worktree_item(
    session: &crate::session::WorktreeSession,
    agent_states: &HashMap<SessionId, AgentState>,
    project_name_prefix: Option<&str>,
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
        });

        let mut sessions: Vec<_> = project
            .worktrees
            .iter()
            .filter_map(|sid| state.sessions.get(sid))
            .collect();
        sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        for session in sessions {
            items.push(worktree_item(session, agent_states, None));
        }
    }
    items
}

fn build_section_grouped_items(
    state: &crate::config::AppState,
    sections: &[crate::session::SectionConfig],
    agent_states: &HashMap<SessionId, AgentState>,
) -> Vec<SessionListItem> {
    let sessions: Vec<crate::session::WorktreeSession> = state.sessions.values().cloned().collect();
    let groups = crate::session::build_sections(&sessions, sections);

    let mut items = Vec::new();
    for group in groups {
        items.push(SessionListItem::SectionHeader {
            name: group.name.clone(),
            count: group.sessions.len(),
        });
        for sid in group.sessions {
            if let Some(session) = state.sessions.get(&sid) {
                let project_name = state
                    .projects
                    .get(&session.project_id)
                    .map(|p| p.name.as_str());
                items.push(worktree_item(session, agent_states, project_name));
            }
        }
    }
    items
}
