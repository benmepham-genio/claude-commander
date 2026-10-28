//! State management: state updates, session sync, list refresh, selection persistence.

use super::*;
use claude_commander_core::api::{ProjectInfo, SessionInfo, Snapshot};
use std::collections::BTreeMap;
impl App {
    pub(super) async fn handle_state_update(&mut self, update: StateUpdate) {
        match update {
            StateUpdate::BackendChanged {
                backend_id,
                snapshot,
                states,
            } => {
                let states = *states;
                let is_local = backend_id == claude_commander_core::backend::LOCAL_BACKEND_ID.0;
                // Diff the OLD agent states (before we overwrite them) against
                // the fresh ones: if the session whose review is open just went
                // Working→Idle, it likely acted on applied comments — refresh the
                // review view in place.
                //
                // The local backend diffs against `ui_state.agent_states` (the
                // rendered map it maintains); a remote backend diffs against its
                // own per-backend `view.agent_states` captured here before the
                // fold below overwrites them. Either way `spawn_review_refresh`
                // routes to the session's owning backend.
                let review_refresh = if is_local {
                    self.review_refresh_on_transition(&self.ui_state.agent_states, &states.states)
                } else {
                    self.backends
                        .iter()
                        .find(|h| h.id.0 == backend_id)
                        .map(|h| h.view.agent_states.states.clone())
                        .and_then(|old| self.review_refresh_on_transition(&old, &states.states))
                };

                if let Some(handle) = self.backends.iter_mut().find(|h| h.id.0 == backend_id) {
                    handle.view.snapshot = *snapshot;
                    handle.view.agent_states = states.clone();
                    // The local backend's connection derives from the snapshot's
                    // tmux health; a remote backend's is owned by its
                    // connection-watch task, so a fold must not touch it.
                    if let Some(conn) =
                        super::connection_from_snapshot(is_local, &handle.view.snapshot)
                    {
                        handle.view.connection = conn;
                    }
                }

                // The local backend drives the rendered agent-state map, the
                // commander chip, and the project-pull badges (folded out of the
                // snapshot the poll loops maintain). Single-backend this phase;
                // Phase E merges every backend's states into one tree.
                if is_local {
                    self.ui_state.agent_states = states.states;
                    self.ui_state.commander_running = states.commander_running;
                }
                // Pull badges union every backend's snapshot (project ids are
                // globally unique), so a remote's blocked pull must be re-folded
                // on any backend change — not only local ones.
                self.apply_project_pull_badges();
                if let Some((sid, title, prev_hash)) = review_refresh {
                    self.spawn_review_refresh(sid, title, prev_hash, false);
                }
                // Re-derive the session-list pending-comment (`*`) markers from
                // every backend's cached snapshot. The startup call runs before
                // remote snapshots exist (bootstrap skips remotes), so without
                // this a remote's pending markers would never render and a
                // cross-frontend marker change would never propagate.
                self.refresh_comment_indicators();
                self.refresh_list_items().await;
            }
            StateUpdate::BackendConnection { backend_id, state } => {
                if let Some(handle) = self.backends.iter_mut().find(|h| h.id.0 == backend_id) {
                    handle.view.connection = state;
                }
                // Re-render the tree so the server header reflects the new
                // health (and command gating re-evaluates).
                self.refresh_list_items().await;
            }
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
                spawned_at,
                session_id,
                project_id,
                preview_content,
                shell_content,
                diff_info,
            } => {
                // Release the in-flight guard iff this result owns it. Keyed on
                // the spawn token, not the selection: a result whose guard has
                // since been replaced must not clear the newer fetch's guard,
                // and a result for a selection that moved on *without* a
                // respawn still has to release its own, or the next fetch is
                // blocked until the 5s backstop.
                if self.ui_state.preview_update_spawned_at == Some(spawned_at) {
                    self.ui_state.preview_update_spawned_at = None;
                }
                // Only paint if the same thing is still selected — otherwise the
                // pane would briefly show another session's output. Session and
                // project ids are unique across backends, so comparing ids is
                // enough.
                let still_selected = self.ui_state.selected_session_id.map(|r| r.id) == session_id
                    && self.ui_state.selected_project_id.map(|(_, p)| p) == project_id;
                if still_selected {
                    self.ui_state.preview_content = preview_content;
                    self.ui_state.shell_content = shell_content;
                    self.ui_state.diff_info = diff_info;
                } else {
                    debug!("Discarding stale PreviewReady (selection changed)");
                }
            }
            StateUpdate::EnrichedPrReady {
                spawned_at,
                session_id,
                info,
            } => {
                // Same generation-token rule as `PreviewReady` above.
                if self.ui_state.enriched_pr_fetch_spawned_at == Some(spawned_at) {
                    self.ui_state.enriched_pr_fetch_spawned_at = None;
                }
                // Only apply if the session is still selected
                if self.ui_state.selected_session_id.map(|r| r.id) == Some(session_id) {
                    // An empty result caches nothing, so record the attempt
                    // separately — otherwise an open Info surface would refetch
                    // (spawning `gh`) every few seconds for the whole session.
                    self.ui_state.enriched_pr_unavailable = info.is_none().then_some(session_id);
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
            StateUpdate::SessionCreated {
                session_id,
                backend_id,
            } => {
                debug!("Session created: {}", session_id);
                let backend_id = BackendId(backend_id);
                self.ui_state.modal = Modal::None;
                self.ui_state.status_message = Some((
                    format!("Created session {}", session_id),
                    Instant::now() + Duration::from_secs(3),
                ));
                // Reconcile the section on (and refresh the view of) the OWNING
                // backend — not always the local one — so the new row is present
                // in that backend's cached view before we try to select it.
                // Selecting before the view carried the session was the bug that
                // left a remote create half-landed (no reconcile, no selection).
                self.reconcile_one_section_assignment(backend_id, session_id)
                    .await;
                // Materialise LFS content in the background (worktree was
                // created with smudging skipped). Inserts into lfs_pull_in_flight
                // before the refresh below so the marker shows on first paint.
                // Self-guards to local sessions (remote worktrees are
                // server-side; the id won't resolve in the local view).
                self.spawn_lfs_pull(session_id).await;
                self.refresh_list_items().await;
                // Select the newly created session
                self.select_session_in_tree(session_id);
                self.spawn_preview_update();
            }
            StateUpdate::SessionCreateFailed { message } => {
                debug!("Session creation failed: {}", message);
                // The backend already removed its half-created session; the
                // change feed refreshes the tree. Just surface the error.
                self.ui_state.modal = Modal::Error { message };
            }
            StateUpdate::RemoteServerProbed {
                nonce,
                server,
                result,
            } => {
                // Only meaningful while the add-server Loading modal from the
                // SAME flow is up: the nonce rejects a stale probe landing
                // while some unrelated Loading modal happens to be shown.
                if nonce != self.probe_nonce
                    || !matches!(self.ui_state.modal, Modal::Loading { .. })
                {
                    return;
                }
                match result {
                    Ok(tmux_ok) => {
                        let name = server.name.clone();
                        match self.add_remote_server_to_config(server) {
                            Ok(()) => {
                                self.ui_state.modal = Modal::None;
                                let msg = if tmux_ok {
                                    format!("Added remote server \"{name}\"")
                                } else {
                                    format!(
                                        "Added remote server \"{name}\" (warning: tmux unavailable on server)"
                                    )
                                };
                                self.ui_state.status_message =
                                    Some((msg, Instant::now() + Duration::from_secs(4)));
                                self.refresh_list_items().await;
                            }
                            Err(e) => {
                                self.ui_state.modal = Modal::Error {
                                    message: format!("Failed to save server: {e}"),
                                };
                            }
                        }
                    }
                    Err(e) => {
                        let name = server.name.clone();
                        self.ui_state.modal = Modal::Confirm {
                            title: "Connection Test Failed".to_string(),
                            message: format!("{e}\n\nSave \"{name}\" anyway?"),
                            on_confirm: ConfirmAction::AddRemoteServerAnyway { server },
                        };
                    }
                }
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
                    *all_branches = super::actions::branch_entries_from_pairs(branches);
                    self.refilter_checkout_branches();
                }
            }
            StateUpdate::CheckoutBranchesLoaded {
                project_id: updated_project,
                branches,
            } => {
                // The initial (no-fetch) listing: fill the list but leave the
                // `fetching` spinner up, since the fetch-refresh is still running.
                if let Modal::CheckoutBranch {
                    project_id,
                    all_branches,
                    ..
                } = &mut self.ui_state.modal
                    && *project_id == updated_project
                {
                    *all_branches = super::actions::branch_entries_from_pairs(branches);
                    self.refilter_checkout_branches();
                }
            }
            StateUpdate::ProjectAdded {
                backend_id,
                project_id,
            } => {
                debug!("Project added: {}", project_id);
                self.ui_state.status_message = Some((
                    "Added project".to_string(),
                    Instant::now() + Duration::from_secs(4),
                ));
                self.refresh_backend_view(BackendId(backend_id)).await;
                self.refresh_list_items().await;
            }
            StateUpdate::GithubReposLoaded {
                backend_id,
                generation,
                result,
            } => {
                // Drop a superseded listing: Ctrl-R can start a second fetch
                // while the first is still running, and the earlier response
                // must not clobber the newer one's state. Also ignored once the
                // picker has closed (the generation is reset on each open).
                if generation != self.ui_state.repo_picker.generation
                    || self.ui_state.repo_picker.backend.0 != backend_id
                {
                    debug!("Discarding stale GitHub repo listing (gen {generation})");
                    return;
                }
                match result {
                    Ok(repos) => {
                        self.ui_state.repo_picker.repos = repos;
                        self.ui_state.repo_picker.fetch = super::RepoFetch::Ready;
                    }
                    Err(message) => {
                        // Not a dead end: the picker stays open so a URL can
                        // still be pasted. The reason goes to the status bar and
                        // the picker's title reflects the state.
                        self.ui_state.repo_picker.repos.clear();
                        self.ui_state.status_message = Some((
                            format!("Could not list GitHub repos: {message}"),
                            Instant::now() + Duration::from_secs(8),
                        ));
                        self.ui_state.repo_picker.fetch = super::RepoFetch::Failed(message);
                    }
                }
                // Rebuild the rows from the new listing if the picker is still up.
                self.refilter_quick_switch();
            }
            StateUpdate::CloneJobUpdated {
                backend_id,
                source,
                result,
            } => {
                self.apply_clone_job_update(BackendId(backend_id), source, result)
                    .await;
            }
            StateUpdate::RestartFinished {
                backend_id,
                kind,
                result,
            } => {
                let backend_id = BackendId(backend_id);
                match result {
                    Ok(()) => {
                        self.ui_state.status_message = Some((
                            kind.success_toast().to_string(),
                            Instant::now() + Duration::from_secs(3),
                        ));
                        // Refresh off the event loop; BackendChanged folds the
                        // fresh view in and re-renders (no post-refresh
                        // selection depends on it here).
                        self.spawn_backend_view_refresh(backend_id);
                    }
                    Err(e) => {
                        self.ui_state.modal = Modal::Error {
                            message: format!("{}: {e}", kind.error_prefix()),
                        };
                    }
                }
            }
            StateUpdate::SessionMutationApplied {
                backend_id,
                session_id,
            } => {
                self.refresh_backend_view(BackendId(backend_id)).await;
                self.refresh_list_items().await;
                // The session may have moved position (rename re-sorts, a section
                // move relocates it); keep it selected and refresh the Info
                // modal's diff if it is open.
                if self.select_session_in_tree(session_id) {
                    self.ui_state.preview_update_spawned_at = None;
                    self.spawn_preview_update();
                }
            }
            StateUpdate::NewSessionProgramsLoaded {
                project_id,
                picker,
                sections,
            } => {
                // Patch the pickers only if a New Session (plain or stacked) modal
                // for the same project is still open; the user may have dismissed
                // it or moved on to another project.
                if let Modal::Input {
                    program_picker,
                    section_picker,
                    on_submit,
                    ..
                } = &mut self.ui_state.modal
                    && program_picker.is_some()
                    && matches!(
                        on_submit,
                        InputAction::CreateSession { project_id: pid, .. }
                        | InputAction::CreateStackedSession { project_id: pid, .. }
                            if *pid == project_id
                    )
                {
                    // The remote may have offered no programs — keep the local
                    // fallback then; always adopt the remote's section list.
                    if let Some(picker) = picker {
                        *program_picker = Some(picker);
                    }
                    if section_picker.is_some() {
                        // Preserve the section baked into the pending action (the
                        // cursor-derived default at open time) so a remote session
                        // still lands where the cursor was, rather than resetting
                        // to the catch-all.
                        let default = match on_submit {
                            InputAction::CreateSession { section, .. } => section.clone(),
                            _ => None,
                        };
                        *section_picker =
                            Some(super::SectionPicker::new(sections, default.as_deref()));
                    }
                }
            }
            StateUpdate::ProgramChoicesLoaded {
                session_id,
                choices,
            } => {
                // Apply only if the change-program palette is still open for the
                // same session; the user may have dismissed it or moved on.
                if let Modal::QuickSwitch {
                    mode: PaletteMode::ProgramPicker { session_id: sid },
                    ..
                } = &self.ui_state.modal
                    && *sid == session_id
                {
                    self.ui_state.program_picker_choices = choices;
                    self.refilter_quick_switch();
                }
            }
            StateUpdate::ServerProgramsLoaded {
                backend,
                generation,
                result,
            } => {
                // Apply only if the Settings → Programs tab is still open for the
                // same target and this is the latest load (a superseded target's
                // response is dropped).
                if let Modal::Settings(settings) = &mut self.ui_state.modal
                    && settings.tab == crate::app::SettingsTab::Programs
                    && settings.programs_state.target == backend
                    && settings.programs_state.load_gen == generation
                {
                    let prog = &mut settings.programs_state;
                    prog.loading = false;
                    match result {
                        Ok(entries) => {
                            prog.entries = entries;
                            prog.load_error = None;
                            if prog.selected >= prog.entries.len() {
                                prog.selected = prog.entries.len().saturating_sub(1);
                            }
                        }
                        Err(message) => {
                            prog.entries.clear();
                            prog.selected = 0;
                            prog.load_error = Some(message);
                        }
                    }
                }
            }
            StateUpdate::ServerProgramsSaveFailed { backend, message } => {
                // Show the failure in the tab if it's still open for that target;
                // don't tear down whatever modal the user has since moved to.
                if let Modal::Settings(settings) = &mut self.ui_state.modal
                    && settings.tab == crate::app::SettingsTab::Programs
                    && settings.programs_state.target == backend
                {
                    settings.programs_state.save_error = Some(format!("save failed: {message}"));
                } else {
                    warn!("failed to save programs to remote backend: {message}");
                }
            }
            StateUpdate::Error { message } => {
                self.ui_state.modal = Modal::Error { message };
            }
            StateUpdate::ReviewPrepared { prepared } => {
                // Only swap in the view if the loading spinner is still up. The
                // user can't navigate while it's shown, but another background
                // event could have replaced the modal (e.g. an error).
                if matches!(self.ui_state.modal, Modal::Loading { .. }) {
                    let ReviewPrepared {
                        session_id,
                        title,
                        base,
                        diff,
                        comments,
                        reviewed,
                        models,
                        content_hash,
                        dropped_comments,
                    } = *prepared;
                    let mut state = DiffReviewState::new(session_id, title, base, diff, comments);
                    state.content_hash = content_hash;
                    state.reviewed = reviewed.into_iter().collect();
                    state.collapse_completed_dirs(None);
                    state.select_first_unreviewed();
                    state.prime_views(models);
                    self.reset_review_images();
                    self.ensure_review_image(&state).await;
                    self.ensure_review_file_lines(&state).await;
                    self.ui_state.modal = Modal::ReviewDiff(Box::new(state));
                    if let Some(notice) =
                        claude_commander_core::comment::dropped_notice(&dropped_comments)
                    {
                        self.set_review_status(&notice);
                    }
                }
            }
            StateUpdate::ReviewOpenFailed { error } => {
                // Only act while our own loading spinner is up (a later event
                // could have replaced the modal). `None` → no changes (toast);
                // `Some` → the fetch failed (error modal).
                if matches!(self.ui_state.modal, Modal::Loading { .. }) {
                    match error {
                        Some(e) => {
                            self.ui_state.modal = Modal::Error {
                                message: format!("Failed to open review: {e}"),
                            };
                        }
                        None => {
                            self.ui_state.modal = Modal::None;
                            self.set_review_status("No changes to review");
                        }
                    }
                }
            }
            StateUpdate::ReviewImageLoaded {
                generation,
                path,
                side,
                image,
            } => {
                // Drop arrivals from a previous review: a stale fetch could
                // otherwise repopulate the cleared cache and show the wrong image
                // for a same-named path in the now-open review.
                if generation != self.review_image_gen.get() {
                    return;
                }
                // Build the render protocol on the main thread (it owns the
                // Picker) and cache it for the `&self` render path.
                let entry = match image {
                    Err(e) => ImageEntry::Failed(e),
                    Ok(img) => match &self.picker {
                        Some(picker) => {
                            let dynimg = std::sync::Arc::try_unwrap(img)
                                .unwrap_or_else(|shared| (*shared).clone());
                            ImageEntry::Ready(Box::new(picker.new_resize_protocol(dynimg)))
                        }
                        None => ImageEntry::Failed("terminal has no image support".to_string()),
                    },
                };
                self.review_images.borrow_mut().insert((path, side), entry);
            }
            StateUpdate::ReviewFileLines {
                generation,
                session_id,
                path,
                lines,
            } => {
                // Drop a stale arrival — a review since closed/reopened, or a
                // fetch spawned before the diff was refreshed (its content is
                // indexed against the old diff's line numbers).
                if generation != self.review_file_gen.get() {
                    return;
                }
                match lines {
                    Ok(lines) => {
                        // The fetch succeeded — clear the in-flight marker so a
                        // later refresh can re-fetch.
                        self.review_file_loads.borrow_mut().remove(&path);
                        if let Some(state) = self.open_review_mut()
                            && state.session_id == session_id
                        {
                            state.set_file_lines(path, lines);
                        }
                    }
                    Err(e) => {
                        // Leave the path in the in-flight set as a negative cache
                        // so we don't spawn a doomed fetch (and re-toast) on every
                        // keypress; it clears on the next open/refresh, which is
                        // when a deleted/renamed file could reappear. Only toast
                        // if this session's review is still open.
                        if self
                            .open_review()
                            .is_some_and(|state| state.session_id == session_id)
                        {
                            self.set_review_status(&format!("Expand failed: {e}"));
                        }
                    }
                }
            }
            StateUpdate::ReviewedMarksPolled {
                session_id,
                generation,
                marks,
            } => {
                self.ui_state.reviewed_poll_in_flight = false;
                if let Some(marks) = marks
                    && let Some(state) = self.open_review_mut()
                    && state.session_id == session_id
                {
                    state.fold_reviewed_marks(generation, &marks);
                }
            }
            StateUpdate::ReviewRefreshed { refreshed, manual } => {
                self.ui_state.review_refresh_in_flight = false;
                match refreshed {
                    Some(prepared) => {
                        // Fold the fresh diff in only if the same review is still
                        // open and the user isn't mid-comment (a rebuild would
                        // drop the draft); otherwise discard it.
                        if let Some(state) = self.open_review_mut()
                            && state.session_id == prepared.session_id
                            && state.comment.is_none()
                        {
                            let ReviewPrepared {
                                diff,
                                comments,
                                reviewed,
                                models,
                                content_hash,
                                dropped_comments,
                                ..
                            } = *prepared;
                            state.refresh_diff(
                                diff,
                                comments,
                                reviewed.into_iter().collect(),
                                models,
                                content_hash,
                            );
                            // The drop notice wins over "Review refreshed": the
                            // refresh is expected, losing a comment isn't.
                            if let Some(notice) =
                                claude_commander_core::comment::dropped_notice(&dropped_comments)
                            {
                                self.set_review_status(&notice);
                            } else if manual {
                                self.set_review_status("Review refreshed");
                            }
                        }
                    }
                    None if manual => self.set_review_status("Review already up to date"),
                    None => {}
                }
                // A refresh replaces the diff, so `refresh_diff` cleared the
                // per-file line cache and any in-flight fetch now carries content
                // indexed against the *old* diff. Bump the fetch generation (drops
                // those arrivals) and clear the in-flight set, then re-fetch the
                // shown file so expand controls reappear without waiting for the
                // next navigation key.
                self.invalidate_review_file_lines();
                if let Some(state) = self.open_review() {
                    self.ensure_review_file_lines(state).await;
                }
            }
            StateUpdate::CascadeFinished { backend_id, result } => {
                self.handle_cascade_finished(BackendId(backend_id), result)
                    .await;
            }
            StateUpdate::SetSessionBaseFinished {
                backend_id,
                session_id,
                result,
            } => {
                self.handle_set_session_base_finished(BackendId(backend_id), session_id, result)
                    .await;
            }
            StateUpdate::PushStackFinished { backend_id, result } => {
                self.handle_push_stack_finished(BackendId(backend_id), result)
                    .await;
            }
            StateUpdate::CascadeAbandonFinished { backend_id, result } => {
                self.handle_cascade_abandon_finished(BackendId(backend_id), result);
            }
            StateUpdate::LfsPullFinished { session_id }
                if self.ui_state.lfs_pull_in_flight.remove(&session_id) =>
            {
                debug!("lfs pull finished for {}", session_id);
                self.refresh_list_items().await;
            }
            _ => {}
        }
    }

    /// Fold the workspace snapshot's per-project pull status into the render-side
    /// `project_pull_blocked` badge map. The background pull loop maintains the
    /// status server-side; only [`PullStatus::Blocked`] surfaces a badge (an
    /// advance/up-to-date/soft-fail clears any prior one).
    fn apply_project_pull_badges(&mut self) {
        use claude_commander_core::api::PullStatus;
        // Union across every backend's snapshot: remote snapshots carry
        // `project_pull` too, and project ids are globally unique, so a blocked
        // pull on any server must surface a badge — not only the local one.
        self.ui_state.project_pull_blocked = self
            .backends
            .iter()
            .flat_map(|handle| handle.view.snapshot.project_pull.iter())
            .filter_map(|(id, status)| match status {
                PullStatus::Blocked { reason } => {
                    Some((*id, claude_commander_core::git::BlockReason::from(*reason)))
                }
                _ => None,
            })
            .collect();
    }

    /// If the review view is open (and no comment draft is in progress) for a
    /// session that just transitioned Working→Idle between `old_states` and
    /// `new_states`, return the arguments for an in-place review refresh.
    ///
    /// The local path passes `ui_state.agent_states` as `old_states` (the
    /// rendered map it maintains); a remote backend passes its per-backend
    /// `view.agent_states` from before the fold overwrote them. The viewed
    /// session's id is globally unique, so a backend whose states don't mention
    /// it never spuriously triggers a refresh for another backend's session.
    fn review_refresh_on_transition(
        &self,
        old_states: &BTreeMap<SessionId, AgentState>,
        new_states: &BTreeMap<SessionId, AgentState>,
    ) -> Option<(SessionId, String, u64)> {
        let state = self.open_review()?;
        if state.comment.is_some() {
            return None;
        }
        let sid = state.session_id;
        let was_working = old_states.get(&sid) == Some(&AgentState::Working);
        let now_idle = new_states.get(&sid) == Some(&AgentState::Idle);
        (was_working && now_idle).then(|| (sid, state.title.clone(), state.content_hash))
    }

    /// Re-run section assignment over every session against current config
    /// (after a live config change), then refresh the cached view + tree.
    pub(super) async fn reconcile_section_assignments(&mut self) {
        let _ = self.local_arc().reconcile_sections().await;
        self.refresh_local_view().await;
    }

    /// Re-run section assignment for a single freshly created session on the
    /// backend that owns it, then refresh that backend's cached view so the new
    /// (possibly re-sectioned) row is present before the caller selects it.
    pub(super) async fn reconcile_one_section_assignment(
        &mut self,
        backend_id: BackendId,
        session_id: SessionId,
    ) {
        let _ = self
            .backend_arc(backend_id)
            .reconcile_one_section(session_id)
            .await;
        self.refresh_backend_view(backend_id).await;
    }

    pub(super) async fn refresh_list_items(&mut self) {
        // The active workspace (or whether there are workspaces at all) may
        // have changed since the theme was built; the rest of this rebuild
        // then reads the right palette.
        self.sync_workspace_theme();

        // A section list mode needs configured sections; fall back to the
        // project list view if the user removed them (hot-reload). The board
        // uses baked-in defaults, so it is unaffected.
        if self.ui_state.view_mode.is_section_view() && self.config.sections.is_empty() {
            self.ui_state.view_mode = claude_commander_core::config::ViewMode::ProjectGrouped;
        }

        // Drop a board filter whose project no longer exists in any snapshot
        // (e.g. just deleted) so the columns don't filter to an absent project.
        // Board-only; harmless in list modes.
        let filter = self.workspace_filter();
        if let Some(f) = self.ui_state.board_filter
            && !self.backends.iter().any(|h| {
                h.view
                    .snapshot
                    .projects
                    .iter()
                    .any(|p| p.id == f && filter.admits(p.workspace.as_deref()))
            })
        {
            self.ui_state.board_filter = None;
        }

        // One-time stale-server toast, computed up front and shared by both
        // views (the per-row/header version annotation is recomputed in each
        // branch). Deferred so `ui_state` isn't mutated under the backends
        // borrow; only the first not-yet-warned remote mismatch is taken.
        let mut pending_version_toast: Option<(
            usize,
            String,
            claude_commander_core::backend::VersionMismatch,
        )> = None;
        for h in &self.backends {
            if h.id == claude_commander_core::backend::LOCAL_BACKEND_ID {
                continue;
            }
            if let Some(m) = claude_commander_core::backend::server_version_mismatch(
                &h.view.snapshot.server.version,
                claude_commander_core::VERSION,
            ) && pending_version_toast.is_none()
                && !self.ui_state.version_warned.contains(&h.id.0)
            {
                pending_version_toast = Some((h.id.0, h.backend.descriptor().name, m));
            }
        }

        let cascade_paused = self
            .backends
            .iter()
            .any(|h| h.view.snapshot.cascade_paused.is_some());

        if self.ui_state.view_mode.is_board() {
            self.rebuild_board_view();
        } else {
            self.rebuild_list_view();
        }
        self.ui_state.cascade_paused = cascade_paused;

        // Fire the deferred stale-server toast now the backends borrow is
        // released, into a free slot only (don't clobber a live message). Mark
        // the server warned only when actually shown, so a deferred one retries.
        if let Some((id, name, mismatch)) = pending_version_toast {
            let slot_free = self
                .ui_state
                .status_message
                .as_ref()
                .is_none_or(|(_, expiry)| Instant::now() >= *expiry);
            if slot_free {
                self.ui_state.version_warned.insert(id);
                self.ui_state.status_message = Some((
                    format!(
                        "{name} is on v{} — older than this client (v{}); some features may not work",
                        mismatch.server, mismatch.client
                    ),
                    Instant::now() + Duration::from_secs(6),
                ));
            }
        }

        self.update_selection();
        self.recompute_stack_chain();
    }

    /// Rebuild the kanban board model from every backend's cached snapshot and
    /// re-anchor the board cursor to the tracked selection.
    fn rebuild_board_view(&mut self) {
        let sections = self.config.effective_sections();
        // The board and its project sidebar show the active workspace only.
        let filter = self.workspace_filter();
        let scoped: Vec<std::borrow::Cow<'_, Snapshot>> = self
            .backends
            .iter()
            .map(|h| filter.scope(&h.view.snapshot))
            .collect();
        let inputs: Vec<claude_commander_core::session::BoardBackendInput> = self
            .backends
            .iter()
            .zip(&scoped)
            .map(|(h, snapshot)| {
                let version_warning = if h.id == claude_commander_core::backend::LOCAL_BACKEND_ID {
                    None
                } else {
                    claude_commander_core::backend::server_version_mismatch(
                        &h.view.snapshot.server.version,
                        claude_commander_core::VERSION,
                    )
                };
                claude_commander_core::session::BoardBackendInput {
                    backend: h.id,
                    name: h.backend.descriptor().name,
                    connection: h.view.connection.clone(),
                    version_warning,
                    snapshot,
                    agent_states: &h.view.agent_states.states,
                }
            })
            .collect();
        let mut board = claude_commander_core::session::build_board(
            &inputs,
            sections.as_ref(),
            self.config.in_progress_limit,
            self.ui_state.board_filter,
            self.config.hide_empty_sections,
        );
        drop(inputs);
        drop(scoped);

        // Mark rows whose LFS content is still being pulled (UI-only state).
        if !self.ui_state.lfs_pull_in_flight.is_empty() {
            for column in &mut board.columns {
                for card in &mut column.cards {
                    let SessionListItem::Worktree {
                        id, lfs_pulling, ..
                    } = &mut card.row
                    else {
                        unreachable!("board rows are always Worktree")
                    };
                    *lfs_pulling = self.ui_state.lfs_pull_in_flight.contains(id);
                }
            }
        }

        let counts = board.selectable_row_counts();
        self.ui_state.board = board;
        self.ui_state.board_state.sync(counts);

        // Per-frame render inputs cached here (recomputed on rebuild only).
        self.ui_state.session_numbers = self.ui_state.board.session_numbers();
        self.ui_state.has_mixed_programs = board_has_mixed_programs(&self.ui_state.board);
        self.rebuild_project_colors();

        // Re-anchor the cursor to the tracked selection so a background rebuild
        // that reorders the board doesn't strand the highlight on a different
        // row than actions target.
        let reanchor = if let Some(sid) = self.ui_state.selected_session_id.map(|r| r.id) {
            self.ui_state.board.position_of(sid)
        } else if let Some(pid) = self.ui_state.selected_project_id.map(|(_, p)| p)
            && self
                .ui_state
                .board_state
                .selected()
                .is_some_and(|pos| pos.col == 0)
        {
            self.ui_state
                .board
                .sidebar_row_of(pid)
                .map(|row| BoardPos { col: 0, row })
        } else {
            None
        };
        if let Some(pos) = reanchor {
            self.ui_state.board_state.select(Some(pos));
        }
    }

    /// Rebuild the flat tree-list rows for the active list view: an optional
    /// pinned recents block, then one per-server header (when more than one
    /// backend) followed by that backend's items.
    fn rebuild_list_view(&mut self) {
        let single_backend = self.backends.len() == 1;
        let mut items: Vec<SessionListItem> = Vec::new();
        // Every list block — Recent and each backend's tree — shows the
        // active workspace only.
        let filter = self.workspace_filter();
        let scoped: Vec<std::borrow::Cow<'_, Snapshot>> = self
            .backends
            .iter()
            .map(|h| filter.scope(&h.view.snapshot))
            .collect();

        // Recent-sessions block, prepended above the per-backend tree and
        // independent of any server. Each row is a shortcut to a session that
        // still appears in its normal place below; its number and project colour
        // are mirrored from the real row at render time. Sessions never attached
        // (no `last_attached_at`) are excluded. `recent_sessions_limit == 0`
        // hides the block entirely.
        let recent_limit = self.config.recent_sessions_limit as usize;
        if recent_limit > 0 {
            let mut candidates: Vec<(chrono::DateTime<chrono::Utc>, SessionListItem)> = Vec::new();
            for (handle, snapshot) in self.backends.iter().zip(&scoped) {
                let agent_states = &handle.view.agent_states.states;
                for s in &snapshot.sessions {
                    if let Some(at) = s.last_attached_at {
                        candidates.push((
                            at,
                            SessionListItem::RecentSession {
                                session: claude_commander_core::backend::SessionRef::new(
                                    handle.id,
                                    s.session_id,
                                ),
                                project_id: s.project_id,
                                title: s.title.clone(),
                                status: s.status,
                                agent_state: agent_states.get(&s.session_id).copied(),
                                unread: s.unread,
                                branch: s.branch.clone(),
                                program: s.program.clone(),
                                keep_alive: s.keep_alive,
                                // Set below by the same LFS-marking pass that
                                // marks the real Worktree rows.
                                lfs_pulling: false,
                                pr_number: s.pr_number,
                                pr_url: s.pr_url.clone(),
                                pr_merged: s.pr_merged,
                                // The DTO carries the already-effective PR state;
                                // wrapping in Some mirrors the real Worktree row.
                                pr_state: Some(s.pr_state),
                                pr_draft: s.pr_draft,
                                pr_labels: s.pr_labels.clone(),
                            },
                        ));
                    }
                }
            }
            let recents = order_recent(candidates, recent_limit);
            if !recents.is_empty() {
                items.push(SessionListItem::RecentsHeader);
                items.extend(recents);
                items.push(SessionListItem::Spacer);
            }
        }
        // Everything pushed so far is the pinned recents block (header + rows +
        // divider); the per-backend tree appended below is the scrolling list.
        let recents_len = items.len();

        for (handle, snapshot) in self.backends.iter().zip(&scoped) {
            let snapshot: &Snapshot = snapshot;
            let agent_states = &handle.view.agent_states.states;
            if !single_backend {
                let version_warning =
                    if handle.id == claude_commander_core::backend::LOCAL_BACKEND_ID {
                        None
                    } else {
                        claude_commander_core::backend::server_version_mismatch(
                            &snapshot.server.version,
                            claude_commander_core::VERSION,
                        )
                    };
                items.push(SessionListItem::ServerHeader {
                    backend: handle.id,
                    name: handle.backend.descriptor().name,
                    connection: handle.view.connection.clone(),
                    version_warning,
                });
            }
            let mut backend_items = match self.ui_state.view_mode {
                claude_commander_core::config::ViewMode::SectionGrouped => {
                    build_section_grouped_items(
                        snapshot,
                        &self.config.sections,
                        self.config.in_progress_limit,
                        agent_states,
                        &self.ui_state.collapsed_sections,
                        self.config.hide_empty_sections,
                    )
                }
                claude_commander_core::config::ViewMode::SectionStacks => {
                    build_stacked_section_items(
                        snapshot,
                        &self.config.sections,
                        self.config.in_progress_limit,
                        agent_states,
                        &self.ui_state.collapsed_sections,
                        self.config.hide_empty_sections,
                    )
                }
                // ProjectGrouped (and Board, unreachable here) use the flat view.
                _ => build_project_grouped_items(snapshot, agent_states),
            };
            items.append(&mut backend_items);
        }
        drop(scoped);

        // Mark rows whose LFS content is still being pulled (UI-only state).
        if !self.ui_state.lfs_pull_in_flight.is_empty() {
            for item in &mut items {
                match item {
                    SessionListItem::Worktree {
                        id, lfs_pulling, ..
                    } => {
                        *lfs_pulling = self.ui_state.lfs_pull_in_flight.contains(id);
                    }
                    // Keep the recents mirror in step with its real row.
                    SessionListItem::RecentSession {
                        session,
                        lfs_pulling,
                        ..
                    } => {
                        *lfs_pulling = self.ui_state.lfs_pull_in_flight.contains(&session.id);
                    }
                    _ => {}
                }
            }
        }

        let selectable: Vec<bool> = items.iter().map(|i| i.is_selectable()).collect();
        let group_starts: Vec<bool> = items.iter().map(|i| i.is_group_header()).collect();
        self.ui_state.list_items = items;
        self.ui_state.recents_len = recents_len;
        // Always install the selectable mask: even ProjectGrouped now carries
        // non-selectable rows (the recents header and its divider), which the
        // old "all rows selectable" `set_item_count` path would let the cursor
        // land on.
        self.ui_state.list_state.set_selectable(selectable);
        self.ui_state.list_state.set_group_starts(group_starts);
    }

    /// Pre-compute the stack-chain breadcrumb for the selected session from its
    /// owning backend's snapshot (stacks never span backends).
    fn recompute_stack_chain(&mut self) {
        let stack_chain = self.ui_state.selected_session_id.map(|sref| {
            let snapshot = &self.view_for(sref.backend).snapshot;
            let session_id = sref.id;
            let by_id = session_index(snapshot);
            let mut entries: Vec<StackChainEntry> = Vec::new();
            if let Some(session) = by_id.get(&session_id).copied() {
                let project_sessions: Vec<&SessionInfo> = snapshot
                    .projects
                    .iter()
                    .find(|p| p.id == session.project_id)
                    .map(|p| {
                        p.session_ids
                            .iter()
                            .filter_map(|sid| by_id.get(sid).copied())
                            .collect()
                    })
                    .unwrap_or_default();
                let base =
                    claude_commander_core::session::stack_root(session_id, &project_sessions);
                let chain =
                    claude_commander_core::session::stack_chain_from_base(base, &project_sessions);
                if chain.len() > 1 {
                    for &sid in &chain {
                        if let Some(s) = by_id.get(&sid).copied() {
                            entries.push(StackChainEntry {
                                title: s.title.clone(),
                                status: s.status,
                                is_current: sid == session_id,
                            });
                        }
                    }
                }
            }
            entries
        });
        self.ui_state.stack_chain = stack_chain.unwrap_or_default();
    }

    /// Save current selection to persisted UI prefs, qualified by the owning
    /// backend's name so it survives a config reorder.
    pub(super) async fn save_selection(&self) {
        let session = self.ui_state.selected_session_id;
        let project = self.ui_state.selected_project_id;
        let backend_id = session
            .map(|r| r.backend)
            .or_else(|| project.map(|(b, _)| b));
        let backend_name =
            backend_id.and_then(|id| self.backend(id).map(|h| h.backend.descriptor().name));
        self.tui_prefs
            .set_selection(session.map(|r| r.id), project.map(|(_, p)| p), backend_name)
            .await;
    }

    /// Restore selection from persisted UI prefs onto the board.
    ///
    /// Prefers the last-selected session (its card row); falls back to the
    /// last-selected project (its sidebar row); otherwise leaves the default
    /// selection installed by `board_state.sync` in `refresh_list_items`.
    /// Session/project ids are unique across backends, so the remembered
    /// backend name needs no disambiguation here.
    pub(super) async fn restore_selection(&mut self) {
        let prefs = self.tui_prefs.prefs();
        let (last_session, last_project) =
            (prefs.last_selected_session, prefs.last_selected_project);

        if self.ui_state.view_mode.is_board() {
            if let Some(sid) = last_session
                && let Some(pos) = self.ui_state.board.position_of(sid)
            {
                self.ui_state.board_state.select(Some(pos));
            } else if let Some(pid) = last_project {
                // Selects the sidebar row (and syncs selection) when the project
                // still exists; a no-op otherwise, leaving the default selection.
                self.select_project_in_sidebar(pid);
            }
        } else if let Some(sid) = last_session
            && let Some(idx) =
                self.ui_state.list_items.iter().position(
                    |item| matches!(item, SessionListItem::Worktree { id, .. } if *id == sid),
                )
        {
            self.ui_state.list_state.select(Some(idx));
        } else if let Some(pid) = last_project
            && let Some(idx) =
                self.ui_state.list_items.iter().position(
                    |item| matches!(item, SessionListItem::Project { id, .. } if *id == pid),
                )
        {
            self.ui_state.list_state.select(Some(idx));
        }
        self.update_selection();
    }
}

/// Order recent-session candidates newest-attached first and cap at `limit`.
/// Each candidate pairs its `last_attached_at` with the row to display. The
/// sort is stable, so equal timestamps keep their input (backend then tree)
/// order. Mirrors the MRU ordering used by the in-tmux session switcher.
pub(super) fn order_recent<T>(
    mut candidates: Vec<(chrono::DateTime<chrono::Utc>, T)>,
    limit: usize,
) -> Vec<T> {
    candidates.sort_by_key(|b| std::cmp::Reverse(b.0));
    candidates
        .into_iter()
        .take(limit)
        .map(|(_, row)| row)
        .collect()
}

/// Index a snapshot's sessions by id for O(1) lookup during stack-chain
/// building.
fn session_index(
    snapshot: &claude_commander_core::api::Snapshot,
) -> std::collections::HashMap<
    claude_commander_core::session::SessionId,
    &claude_commander_core::api::SessionInfo,
> {
    snapshot
        .sessions
        .iter()
        .map(|s| (s.session_id, s))
        .collect()
}

/// Whether the board's worktree rows span more than one distinct program
/// (comparing base program names, so `claude --foo` and `claude --bar` count as
/// one). Cached on `UiState` so the board widget doesn't recompute it per frame.
fn board_has_mixed_programs(board: &claude_commander_core::session::Board) -> bool {
    let mut first: Option<&str> = None;
    for card in board.cards() {
        let SessionListItem::Worktree { program, .. } = &card.row else {
            unreachable!("board rows are always Worktree")
        };
        let p = crate::widgets::status_glyph::program_name(program);
        match first {
            None => first = Some(p),
            Some(f) if f != p => return true,
            _ => {}
        }
    }
    false
}

/// Apply freshly-detected agent states for the sessions just viewed during an
/// attach, leaving every other session's entry untouched.
///
/// Returning from an attach must not blank the whole tree (which happens if the
/// agent-state map is cleared wholesale) nor drop genuine background
/// `Working → Idle` notifications. Only the sessions the user actually saw get
/// their state overwritten here; because the user was watching them, the
/// refreshed state is applied directly without running unread detection, so
/// their own transitions are never re-flagged as unread. Every other session
/// keeps its prior state, preserving the baseline a later poll diffs against.
pub(super) fn apply_viewed_session_refresh(
    agent_states: &mut BTreeMap<SessionId, AgentState>,
    refreshed: BTreeMap<SessionId, AgentState>,
) {
    agent_states.extend(refreshed);
}

/// Preview the stack-retarget that deleting `session_id` would trigger, derived
/// from a workspace snapshot: `(number of direct stacked children, branch they'd
/// be retargeted onto)`. Returns `None` when the session has no direct stacked
/// children, so the delete confirmation only mentions retargeting when it
/// actually applies.
///
/// DTO twin of [`AppState::stack_retarget_preview`](claude_commander_core::config::storage::AppState::stack_retarget_preview):
/// the delete-confirm dialog derives its preview from the cached snapshot rather
/// than reading the store, so a remote backend's snapshot drives it identically.
pub(super) fn stack_retarget_preview_from_snapshot(
    snapshot: &Snapshot,
    session_id: SessionId,
) -> Option<(usize, String)> {
    let deleted = snapshot
        .sessions
        .iter()
        .find(|s| s.session_id == session_id)?;
    let project_id = deleted.project_id;
    let main_branch = snapshot
        .projects
        .iter()
        .find(|p| p.id == project_id)?
        .main_branch
        .clone();
    let project_sessions: Vec<&SessionInfo> = snapshot
        .sessions
        .iter()
        .filter(|s| s.project_id == project_id)
        .collect();

    let child_ids: Vec<SessionId> = project_sessions
        .iter()
        .filter(|s| {
            claude_commander_core::session::resolve_stack_parent(**s, &project_sessions)
                == Some(session_id)
        })
        .map(|s| s.session_id)
        .collect();
    if child_ids.is_empty() {
        return None;
    }

    let new_base_branch =
        claude_commander_core::session::resolve_stack_parent(deleted, &project_sessions)
            .and_then(|pid| project_sessions.iter().find(|s| s.session_id == pid))
            .map(|p| p.branch.clone())
            .unwrap_or(main_branch);
    Some((child_ids.len(), new_base_branch))
}

/// One row per legal stack base for `session_id`: the project's main branch
/// first (the "unstack" target), then every other session in the project that
/// is not one of this session's own descendants.
///
/// Derived from the cached snapshot, like
/// [`stack_retarget_preview_from_snapshot`], so a remote backend's sessions are
/// offered on identical terms. The host re-validates every choice inside its
/// store lock — this filter is for a sane menu, not for safety.
///
/// The "current base" marker is only applied where it can be *proven*. A stack
/// parent resolved from `pr_base_branch`/`stack_parent_session_id` is proof, so
/// that row is marked. Main is not: a session with neither field set may equally
/// be based on a branch no session owns (a `--base-branch` fork, or the
/// checkout-branch flow, both of which leave `base_branch` pointing elsewhere),
/// and those are indistinguishable from here — `SessionInfo` carries no
/// `base_branch`, and adding one means regenerating the frb bindings, which
/// currently forces a `flutter_rust_bridge` version bump. So an unstacked
/// session shows no marker rather than a possibly-wrong one; every row is still
/// a legal target.
pub(super) fn base_picker_rows_from_snapshot(
    snapshot: &Snapshot,
    session_id: SessionId,
) -> Vec<(Option<SessionId>, String, String)> {
    let Some(session) = snapshot
        .sessions
        .iter()
        .find(|s| s.session_id == session_id)
    else {
        return Vec::new();
    };
    let project_id = session.project_id;
    let Some(project) = snapshot.projects.iter().find(|p| p.id == project_id) else {
        return Vec::new();
    };
    let mut project_sessions: Vec<&SessionInfo> = snapshot
        .sessions
        .iter()
        .filter(|s| s.project_id == project_id)
        .collect();
    // Deterministic order: the snapshot's own ordering is not guaranteed, and
    // `resolve_stack_parent` breaks branch-name ties by position.
    project_sessions.sort_by_key(|s| (s.created_at, s.session_id));

    let current_parent =
        claude_commander_core::session::resolve_stack_parent(session, &project_sessions);
    // The main row carries no marker — see the note above on why it cannot be
    // proven to be the current base from the wire DTO alone.
    let mut rows = vec![(
        None,
        project.main_branch.clone(),
        format!("Project main branch ({})", project.main_branch),
    )];

    for candidate in &project_sessions {
        let cid = candidate.session_id;
        if cid == session_id
            || claude_commander_core::session::is_descendant_of(cid, session_id, &project_sessions)
        {
            continue;
        }
        rows.push((
            Some(cid),
            candidate.branch.clone(),
            format!(
                "{} ({}){}",
                candidate.title,
                candidate.branch,
                if current_parent == Some(cid) {
                    CURRENT_SUFFIX
                } else {
                    ""
                }
            ),
        ));
    }

    // A base belonging to no session (a `--base-branch` fork, or a PR targeting
    // a release branch) gets no row and no marker: every row here must be a
    // legal, selectable target, so there is nowhere safe to put a note that is
    // not itself an action. Leaving it unmarked is the honest option.
    rows
}

/// Suffix marking the row a session is already based on.
const CURRENT_SUFFIX: &str = " — current base";

// ---- List-view item builders (revived from main; reuse board.rs helpers) ----

/// Whether `hide_empty_sections` drops the section `name` holding `count`
/// sessions. In Progress is exempt while some project has no sessions at all:
/// it is the only section that lists such a project, so hiding it would make
/// the project unreachable — the usual state of a freshly populated workspace.
fn hides_empty_section(snapshot: &Snapshot, name: &str, count: usize, hide_empty: bool) -> bool {
    hide_empty
        && count == 0
        && !(name == claude_commander_core::session::IN_PROGRESS
            && snapshot.projects.iter().any(|p| p.session_ids.is_empty()))
}

pub(super) fn build_project_grouped_items(
    snapshot: &Snapshot,
    agent_states: &BTreeMap<SessionId, AgentState>,
) -> Vec<SessionListItem> {
    let by_id = session_index(snapshot);
    let mut items = Vec::new();
    let mut projects: Vec<&ProjectInfo> = snapshot.projects.iter().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));

    for project in projects {
        items.push(SessionListItem::Project {
            id: project.id,
            name: project.name.clone(),
            repo_path: project.repo_path.clone(),
            main_branch: project.main_branch.clone(),
            worktree_count: project.session_ids.len(),
            nested: false,
        });

        // Use stack-aware ordering so stacked children render indented
        // directly beneath their stack base.
        let sessions: Vec<&SessionInfo> = project
            .session_ids
            .iter()
            .filter_map(|sid| by_id.get(sid).copied())
            .collect();
        for (sid, stacked_child) in
            claude_commander_core::session::board::build_session_order(&sessions)
        {
            if let Some(session) = by_id.get(&sid).copied() {
                items.push(claude_commander_core::session::board::worktree_item(
                    session,
                    agent_states,
                    None,
                    stacked_child,
                ));
            }
        }
    }
    items
}

pub(super) fn build_section_grouped_items(
    snapshot: &Snapshot,
    sections: &[claude_commander_core::session::SectionConfig],
    in_progress_limit: Option<u32>,
    agent_states: &BTreeMap<SessionId, AgentState>,
    collapsed_sections: &std::collections::HashSet<String>,
    hide_empty_sections: bool,
) -> Vec<SessionListItem> {
    let by_id = session_index(snapshot);
    let groups = claude_commander_core::session::build_sections(&snapshot.sessions, sections);

    let mut projects: Vec<&ProjectInfo> = snapshot.projects.iter().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));

    let mut items = Vec::new();
    let mut first_section = true;
    for group in groups.iter() {
        if hides_empty_section(
            snapshot,
            &group.name,
            group.sessions.len(),
            hide_empty_sections,
        ) {
            continue;
        }
        if !first_section {
            items.push(SessionListItem::Spacer);
        }
        first_section = false;
        let collapsed = collapsed_sections.contains(&group.name);
        items.push(SessionListItem::SectionHeader {
            name: group.name.clone(),
            count: group.sessions.len(),
            collapsed,
            max_sessions: claude_commander_core::session::board::resolve_section_limit(
                &group.name,
                sections,
                in_progress_limit,
            ),
        });

        if collapsed {
            continue;
        }

        let is_in_progress = group.name == claude_commander_core::session::IN_PROGRESS;
        // Preserve group sort order (oldest-first) while partitioning by project.
        let mut by_project: std::collections::HashMap<
            claude_commander_core::session::ProjectId,
            Vec<SessionId>,
        > = Default::default();
        let mut project_order: Vec<claude_commander_core::session::ProjectId> = Vec::new();
        for sid in &group.sessions {
            if let Some(session) = by_id.get(sid) {
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
                    if let Some(session) = by_id.get(sid).copied() {
                        items.push(claude_commander_core::session::board::worktree_item(
                            session,
                            agent_states,
                            None,
                            false,
                        ));
                    }
                }
            }
        }
    }
    items
}

pub(super) fn build_stacked_section_items(
    snapshot: &Snapshot,
    sections: &[claude_commander_core::session::SectionConfig],
    in_progress_limit: Option<u32>,
    agent_states: &BTreeMap<SessionId, AgentState>,
    collapsed_sections: &std::collections::HashSet<String>,
    hide_empty_sections: bool,
) -> Vec<SessionListItem> {
    use chrono::{DateTime, Utc};

    #[derive(Clone)]
    struct GroupRender {
        sort_key: DateTime<Utc>,
        // Tiebreaker for groups whose roots share an entered_section_at —
        // common when one apply_assignment pass stamps multiple sessions
        // with the same `now`. Without a stable tiebreaker, HashMap-
        // randomised insertion order leaks into the sort and the UI
        // appears to churn on every refresh.
        root_id: SessionId,
        order: Vec<(SessionId, bool)>,
    }

    let by_id = session_index(snapshot);

    // Stable project order. Ties on `name` (unusual but possible) fall
    // back to project id so we never depend on HashMap iteration order.
    let mut projects: Vec<&ProjectInfo> = snapshot.projects.iter().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));

    // section_name → project_id → Vec<GroupRender>
    let mut by_section: std::collections::HashMap<
        String,
        std::collections::HashMap<claude_commander_core::session::ProjectId, Vec<GroupRender>>,
    > = std::collections::HashMap::new();
    let valid_section = |name: &str| {
        name == claude_commander_core::session::IN_PROGRESS
            || sections.iter().any(|s| s.name == name)
    };

    for project in &projects {
        // Sort by (created_at, id) so any downstream max_by_key on this
        // slice (e.g. fan-out children with identical created_at in
        // `stack_top`) picks a deterministic winner.
        let mut project_sessions: Vec<&SessionInfo> = snapshot
            .sessions
            .iter()
            .filter(|s| s.project_id == project.id)
            .collect();
        if project_sessions.is_empty() {
            continue;
        }
        project_sessions.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then(a.session_id.cmp(&b.session_id))
        });

        // Bucket every session by its stack root (returns self for
        // unstacked). Track first-encounter root order so we iterate
        // groups deterministically below without leaking
        // HashMap-iteration order into the output.
        let mut groups: std::collections::HashMap<SessionId, Vec<&SessionInfo>> =
            std::collections::HashMap::new();
        let mut group_roots: Vec<SessionId> = Vec::new();
        for s in &project_sessions {
            let root_id =
                claude_commander_core::session::stack_root(s.session_id, &project_sessions);
            if !groups.contains_key(&root_id) {
                group_roots.push(root_id);
            }
            groups.entry(root_id).or_default().push(s);
        }

        for root_id in group_roots {
            let members = groups.remove(&root_id).unwrap_or_default();
            // Placement is anchored on the stack ROOT (the base session), not
            // the leaf: the root's section decides where the whole stack lands,
            // so a child's automatic section (e.g. a draft leaf) can't drag an
            // already-advanced root into an earlier section.
            let Some(root) = members.iter().find(|s| s.session_id == root_id).copied() else {
                continue;
            };

            // Walk leaf → root along the main line collecting the *root-most*
            // valid `section_override` (stale ones — naming a section no longer
            // in config — are skipped, same as `assign_section`, so we fall back
            // to `current_section` rather than dumping into In Progress).
            // Root-most wins so the root's own override takes precedence, but a
            // manual override on a child is still honoured when the root has
            // none — the section picker writes the override to the selected row,
            // which may be a child. Off-path siblings are not considered.
            let mut effective: Option<String> = None;
            let mut cursor = claude_commander_core::session::stack_top(root_id, &project_sessions);
            for _ in 0..project_sessions.len() {
                let Some(cur) = project_sessions
                    .iter()
                    .find(|s| s.session_id == cursor)
                    .copied()
                else {
                    break;
                };
                if let Some(ovr) = &cur.section_override
                    && valid_section(ovr)
                {
                    effective = Some(ovr.clone());
                }
                match claude_commander_core::session::resolve_stack_parent(cur, &project_sessions) {
                    Some(parent) => cursor = parent,
                    None => break,
                }
            }
            let section_name = effective
                .or_else(|| root.current_section.clone())
                .filter(|n| valid_section(n))
                .unwrap_or_else(|| claude_commander_core::session::IN_PROGRESS.to_string());

            // Order within the group: stack-aware (root first, children
            // indented). build_session_order resolves parents only against
            // the slice it's given, so passing just the group's members
            // keeps the root flat and descendants indented even when the
            // subgraph fans out.
            let order = claude_commander_core::session::board::build_session_order(&members);

            by_section
                .entry(section_name)
                .or_default()
                .entry(project.id)
                .or_default()
                .push(GroupRender {
                    sort_key: root.entered_section_at.unwrap_or_default(),
                    root_id,
                    order,
                });
        }
    }

    // Emit IN_PROGRESS first, then user sections in declared order — same
    // overall layout the plain section view uses.
    let section_order: Vec<String> =
        std::iter::once(claude_commander_core::session::IN_PROGRESS.to_string())
            .chain(sections.iter().map(|s| s.name.clone()))
            .collect();

    let mut items = Vec::new();
    let mut first_section = true;
    for section_name in section_order.iter() {
        let project_groups = by_section.get(section_name);
        let total_count: usize = project_groups
            .map(|m| {
                m.values()
                    .flat_map(|g| g.iter())
                    .map(|g| g.order.len())
                    .sum()
            })
            .unwrap_or(0);

        if hides_empty_section(snapshot, section_name, total_count, hide_empty_sections) {
            continue;
        }
        if !first_section {
            items.push(SessionListItem::Spacer);
        }
        first_section = false;

        let collapsed = collapsed_sections.contains(section_name);
        items.push(SessionListItem::SectionHeader {
            name: section_name.clone(),
            count: total_count,
            collapsed,
            max_sessions: claude_commander_core::session::board::resolve_section_limit(
                section_name,
                sections,
                in_progress_limit,
            ),
        });
        if collapsed {
            continue;
        }

        let is_in_progress = section_name == claude_commander_core::session::IN_PROGRESS;
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

            // Stacks within a section sort by their root's
            // entered_section_at, with root_id as a stable tiebreaker so
            // batched apply_assignment calls (which stamp many sessions
            // with the same `now`) don't make the view churn.
            groups_in_proj
                .sort_by(|a, b| a.sort_key.cmp(&b.sort_key).then(a.root_id.cmp(&b.root_id)));
            for group in groups_in_proj {
                for (sid, stacked_child) in group.order {
                    if let Some(session) = by_id.get(&sid).copied() {
                        items.push(claude_commander_core::session::board::worktree_item(
                            session,
                            agent_states,
                            None,
                            stacked_child,
                        ));
                    }
                }
            }
        }
    }
    items
}

#[cfg(test)]
mod unread_transition_tests {
    use super::*;
    use claude_commander_core::api::detect_unread_transitions;
    use claude_commander_core::session::SessionId;
    use std::collections::BTreeMap;

    #[test]
    fn viewed_refresh_preserves_background_unread() {
        // Scenario: attached to session A while a background session B is also
        // running. Both finish (Working → Idle) during the attach. On detach we
        // refresh only the viewed session (A). A subsequent poll must still flag
        // B as unread (we never saw it finish) while leaving A alone.
        let a = SessionId::new();
        let b = SessionId::new();

        // Pre-attach baseline: both working.
        let mut agent_states = BTreeMap::from([(a, AgentState::Working), (b, AgentState::Working)]);

        // Detach refreshes only the viewed session, now observed idle.
        apply_viewed_session_refresh(&mut agent_states, BTreeMap::from([(a, AgentState::Idle)]));

        // A reflects its observed state; B's baseline is untouched (not wiped).
        assert_eq!(agent_states.get(&a), Some(&AgentState::Idle));
        assert_eq!(agent_states.get(&b), Some(&AgentState::Working));

        // Next background poll reports both idle.
        let poll = BTreeMap::from([(a, AgentState::Idle), (b, AgentState::Idle)]);
        let unread = detect_unread_transitions(&agent_states, &poll);

        // Only B is flagged: A's finish was watched, B's was not. A wholesale
        // clear() on detach would have dropped B's notification entirely.
        assert_eq!(unread, vec![b]);
    }
}

#[cfg(test)]
mod stack_order_tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};
    use claude_commander_core::api::snapshot_from_state;
    use claude_commander_core::session::{ProjectId, WorktreeSession};
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

    // `build_session_order`'s ordering rules (stack grouping, newest-first
    // roots, orphan/PR-base handling) are unit-tested where the function lives,
    // in `session::board`.

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

    fn section_named(name: &str) -> claude_commander_core::session::SectionConfig {
        claude_commander_core::session::SectionConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// Build the DTO [`Snapshot`] the tree builders now consume, from a
    /// list of domain sessions — same shaped input as before, projected through
    /// the production `snapshot_from_state` so tests exercise the real
    /// conversion path.
    fn appstate_from(sessions: Vec<WorktreeSession>) -> Snapshot {
        let mut state = claude_commander_core::config::AppState::default();
        // Group sessions by their project_id so projects with multiple
        // worktrees stay linked correctly.
        let mut project_titles: std::collections::HashMap<ProjectId, String> = Default::default();
        for s in &sessions {
            project_titles
                .entry(s.project_id)
                .or_insert_with(|| format!("p-{}", s.project_id));
        }
        for (pid, name) in project_titles {
            let mut project =
                claude_commander_core::session::Project::new(&name, PathBuf::from("/tmp"), "main");
            project.id = pid;
            state.projects.insert(pid, project);
        }
        for s in sessions {
            let pid = s.project_id;
            state.projects.get_mut(&pid).unwrap().add_worktree(s.id);
            state.sessions.insert(s.id, s);
        }
        snapshot_from_state(&state)
    }

    /// The picker offers main plus every non-descendant sibling, and never the
    /// session itself — a session based on its own descendant would close a
    /// cycle, and a cycle removes the whole stack from the rendered order.
    #[test]
    fn base_picker_offers_main_and_excludes_self_and_descendants() {
        let pid = ProjectId::new();
        let mk = |title: &str, branch: &str, age: i64, parent: Option<SessionId>| {
            let mut s = make_session(title, branch, age);
            s.project_id = pid;
            s.stack_parent_session_id = parent;
            s
        };
        let base = mk("base", "base-br", 0, None);
        let mid = mk("mid", "mid-br", 5, Some(base.id));
        let leaf = mk("leaf", "leaf-br", 10, Some(mid.id));
        let solo = mk("solo", "solo-br", 15, None);
        let (base_id, mid_id, leaf_id, solo_id) = (base.id, mid.id, leaf.id, solo.id);
        let snapshot = appstate_from(vec![base, mid, leaf, solo]);

        let rows = base_picker_rows_from_snapshot(&snapshot, mid_id);
        let targets: Vec<Option<SessionId>> = rows.iter().map(|(t, _, _)| *t).collect();

        assert_eq!(targets[0], None, "main branch must be the first row");
        assert_eq!(rows[0].1, "main");
        assert!(
            targets.contains(&Some(base_id)),
            "its own parent is offered"
        );
        assert!(targets.contains(&Some(solo_id)), "an unrelated session too");
        assert!(
            !targets.contains(&Some(mid_id)),
            "a session must not be offered itself"
        );
        assert!(
            !targets.contains(&Some(leaf_id)),
            "a descendant would close a cycle"
        );
    }

    /// The current base is marked so the user can see where they already are.
    #[test]
    fn base_picker_marks_the_current_base() {
        let pid = ProjectId::new();
        let base = {
            let mut s = make_session("base", "base-br", 0);
            s.project_id = pid;
            s
        };
        let child = {
            let mut s = make_session("child", "child-br", 5);
            s.project_id = pid;
            s.stack_parent_session_id = Some(base.id);
            s
        };
        let (base_id, child_id) = (base.id, child.id);
        let snapshot = appstate_from(vec![base, child]);

        let rows = base_picker_rows_from_snapshot(&snapshot, child_id);
        let marked: Vec<&String> = rows
            .iter()
            .filter(|(_, _, l)| l.contains("current base"))
            .map(|(_, _, l)| l)
            .collect();
        assert_eq!(marked.len(), 1, "exactly one row is the current base");
        assert!(marked[0].contains("base-br"));
        // And the parent row really is the one marked.
        let parent_row = rows.iter().find(|(t, _, _)| *t == Some(base_id)).unwrap();
        assert!(parent_row.2.contains("current base"));
    }

    /// A session based on a branch no session owns resolves to no stack parent —
    /// but it is *not* based on main, so main must not be labelled the current
    /// base. Both routes there are covered: a PR targeting a release branch, and
    /// a `--base-branch` / checked-out-branch fork with no PR at all. Only the
    /// session's `base_branch` could tell the second apart from a plain
    /// unstacked session, and that field is *not* on the wire — which is why
    /// the main row carries no marker at all.
    #[test]
    fn base_picker_does_not_claim_main_when_the_base_is_an_unowned_branch() {
        let pid = ProjectId::new();

        let mut via_pr = make_session("via-pr", "a-br", 0);
        via_pr.project_id = pid;
        via_pr.pr_base_branch = Some("release/1.2".to_string());
        let via_pr_id = via_pr.id;

        // No PR and no stack parent — indistinguishable on the wire from a
        // session based on main, which is exactly why main is never marked.
        let mut via_flag = make_session("via-flag", "b-br", 5);
        via_flag.project_id = pid;
        let via_flag_id = via_flag.id;

        let snapshot = appstate_from(vec![via_pr, via_flag]);

        for (id, what) in [(via_pr_id, "a PR base"), (via_flag_id, "--base-branch")] {
            let rows = base_picker_rows_from_snapshot(&snapshot, id);
            assert!(
                !rows.iter().any(|(_, _, l)| l.contains("current base")),
                "nothing may be marked when the base comes from {what}, got {rows:?}"
            );
        }
    }

    /// Main is never marked, even for a session that really is based on it: the
    /// wire DTO cannot distinguish that from a `--base-branch` fork, and a
    /// marker that is sometimes wrong is worse than none on a repair screen.
    #[test]
    fn base_picker_never_marks_the_main_row() {
        let pid = ProjectId::new();
        let mut solo = make_session("solo", "solo-br", 0);
        solo.project_id = pid;
        let solo_id = solo.id;
        let snapshot = appstate_from(vec![solo]);

        let rows = base_picker_rows_from_snapshot(&snapshot, solo_id);
        assert_eq!(rows[0].0, None, "the first row is the main-branch row");
        assert!(
            !rows[0].2.contains("current base"),
            "the main row must never claim to be the current base, got {rows:?}"
        );
    }

    #[test]
    fn retarget_preview_reports_children_and_new_base() {
        let pid = ProjectId::new();
        let base = {
            let mut s = make_session("base", "base-br", 0);
            s.project_id = pid;
            s
        };
        let child = {
            let mut s = make_session("child", "child-br", 5);
            s.project_id = pid;
            s.stack_parent_session_id = Some(base.id);
            s
        };
        let base_id = base.id;
        let child_id = child.id;
        let snapshot = appstate_from(vec![base, child]);

        // Deleting the stack base retargets its one child onto the project's
        // main branch (the base was the stack root).
        assert_eq!(
            stack_retarget_preview_from_snapshot(&snapshot, base_id),
            Some((1, "main".to_string()))
        );
        // The leaf child has no stacked children → no retarget preview.
        assert_eq!(
            stack_retarget_preview_from_snapshot(&snapshot, child_id),
            None
        );
    }

    #[test]
    fn stacked_sections_group_whole_stack_under_root_section() {
        // base.current_section = "Review" (the root), child.current_section =
        // "Open" (the leaf). In the stacked-section view the whole stack is
        // placed by the ROOT, so it appears under "Review" (not the leaf's
        // "Open"), with `child` indented beneath `base`.
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        let mut child = make_session_in_section("child", "child", 10, "Open");
        child.project_id = project_id;
        child.stack_parent_session_id = Some(base.id);

        let state = appstate_from(vec![base.clone(), child.clone()]);
        let sections = vec![section_named("Open"), section_named("Review")];
        let agent_states = BTreeMap::new();
        let collapsed = std::collections::HashSet::new();

        let items =
            build_stacked_section_items(&state, &sections, None, &agent_states, &collapsed, false);

        // Walk items: find the "Review" header, then base+child should follow.
        let found_review = items.iter().any(
            |item| matches!(item, SessionListItem::SectionHeader { name, .. } if name == "Review"),
        );
        assert!(
            found_review,
            "Review section header should be present: {items:?}"
        );

        // After the Review header, expect: Project → base (stacked_child:false) → child (stacked_child:true)
        let after = items
            .iter()
            .skip_while(
                |i| !matches!(i, SessionListItem::SectionHeader { name, .. } if name == "Review"),
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
            "stack should render under root's section with indentation preserved"
        );

        // And there should be no Worktree row under "Open".
        let open_rows: Vec<_> = items
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
            .collect();
        assert!(
            open_rows.is_empty(),
            "stack should not appear under the leaf's Open section: {open_rows:?}"
        );
    }

    #[test]
    fn stacked_sections_place_by_root_not_draft_child_leaf() {
        // Regression for the "why is my non-draft session in Drafts?" report:
        // a non-draft stack *root* in "In Review" with a *draft child* stacked
        // on top (the leaf) whose current_section is "Drafts". The whole stack
        // must be placed by the root's section ("In Review"), NOT the leaf's
        // ("Drafts") — otherwise a draft child drags its already-approved root
        // into the Drafts section.
        let project_id = ProjectId::new();
        let mut root = make_session_in_section("root", "root", 0, "In Review");
        root.project_id = project_id;
        let mut child = make_session_in_section("child", "child", 10, "Drafts");
        child.project_id = project_id;
        child.stack_parent_session_id = Some(root.id);

        let state = appstate_from(vec![root.clone(), child.clone()]);
        let sections = vec![section_named("In Review"), section_named("Drafts")];
        let items = build_stacked_section_items(
            &state,
            &sections,
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
        );

        // Track the enclosing section header for each session row.
        let mut current_header: Option<String> = None;
        let mut landed: std::collections::HashMap<SessionId, String> = Default::default();
        for item in &items {
            match item {
                SessionListItem::SectionHeader { name, .. } => {
                    current_header = Some(name.clone());
                }
                SessionListItem::Worktree { id, .. } => {
                    if let Some(h) = &current_header {
                        landed.insert(*id, h.clone());
                    }
                }
                _ => {}
            }
        }
        assert_eq!(
            landed.get(&root.id).map(String::as_str),
            Some("In Review"),
            "stack should be placed by the root's section, not the draft leaf's"
        );
        assert_eq!(
            landed.get(&child.id).map(String::as_str),
            Some("In Review"),
            "the draft child renders with its stack under the root's section"
        );
    }

    #[test]
    fn stacked_sections_root_override_places_whole_stack() {
        // base (root) has section_override = "Pinned"; child (leaf) has no
        // override. The root's own override anchors the whole stack in "Pinned".
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
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
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
            "whole stack should land in the root's overridden section"
        );
    }

    #[test]
    fn stacked_sections_root_override_beats_child_override() {
        // Root and child BOTH carry a valid (but different) override. Placement
        // is root-anchored, so the root's override wins and the whole stack
        // lands in "Pinned" — not the child's "Open". This pins the precedence
        // rule (root-most valid override wins) that the two single-override
        // tests can't discriminate.
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        base.section_override = Some("Pinned".to_string());
        let mut child = make_session_in_section("child", "child", 10, "Review");
        child.project_id = project_id;
        child.stack_parent_session_id = Some(base.id);
        child.section_override = Some("Open".to_string());

        let state = appstate_from(vec![base.clone(), child.clone()]);
        let sections = vec![
            section_named("Open"),
            section_named("Review"),
            section_named("Pinned"),
        ];
        let items = build_stacked_section_items(
            &state,
            &sections,
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
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
                SessionListItem::Worktree { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(
            pinned_rows,
            vec![base.id, child.id],
            "root's override should win over the child's"
        );

        // Nothing should land under the child's "Open" override.
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
            open_count, 0,
            "child's override must not win over the root's"
        );
    }

    #[test]
    fn stacked_sections_child_override_still_moves_whole_stack() {
        // The section picker writes the override to the *selected* row, which
        // may be a stacked child. Placement is root-anchored, but a manual
        // override on the child (root has none) must still relocate the whole
        // stack — otherwise "move to section" would silently do nothing when a
        // child row is selected.
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        let mut child = make_session_in_section("child", "child", 10, "Open");
        child.project_id = project_id;
        child.stack_parent_session_id = Some(base.id);
        child.section_override = Some("Pinned".to_string());

        let state = appstate_from(vec![base.clone(), child.clone()]);
        let sections = vec![
            section_named("Open"),
            section_named("Review"),
            section_named("Pinned"),
        ];
        let items = build_stacked_section_items(
            &state,
            &sections,
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
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
            "a manual override on the child should relocate the whole stack"
        );
    }

    #[test]
    fn stacked_sections_stale_override_falls_back_to_current_section() {
        // A session pinned (section_override) to a section that no longer
        // exists in config must fall back to its valid current_section, not
        // be dumped into In Progress.
        let project_id = ProjectId::new();
        let mut s = make_session_in_section("s", "s", 0, "Open");
        s.project_id = project_id;
        s.section_override = Some("Deleted Section".to_string());

        let state = appstate_from(vec![s.clone()]);
        let sections = vec![section_named("Open"), section_named("Review")];
        let items = build_stacked_section_items(
            &state,
            &sections,
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
        );

        // Walk items tracking the enclosing section header, then read off
        // which header the session row landed under.
        let mut current_header: Option<String> = None;
        let mut landed: Option<String> = None;
        for item in &items {
            match item {
                SessionListItem::SectionHeader { name, .. } => {
                    current_header = Some(name.clone());
                }
                SessionListItem::Worktree { id, .. } if *id == s.id => {
                    landed = current_header.clone();
                }
                _ => {}
            }
        }
        assert_eq!(
            landed.as_deref(),
            Some("Open"),
            "stale override should defer to current_section, not In Progress"
        );
    }

    #[test]
    fn stacked_sections_fan_out_groups_share_root_and_use_root_section() {
        // Fan-out: base has two children, B (older) and C (newer, the leaf).
        // Placement is anchored on the root, so the whole stack (base+B+C)
        // appears under base's current_section ("Review"), regardless of the
        // newest leaf C sitting in "Open".
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
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
        );

        let review_session_ids: Vec<_> = items
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
            .filter_map(|i| match i {
                SessionListItem::Worktree { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert!(
            review_session_ids.contains(&base.id)
                && review_session_ids.contains(&b.id)
                && review_session_ids.contains(&c.id),
            "all three subgraph members should appear under Review: {review_session_ids:?}"
        );

        let open_session_count = items
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
            open_session_count, 0,
            "Open should be empty because the whole stack follows the root into Review"
        );
    }

    #[test]
    fn stacked_sections_sibling_override_off_leaf_path_is_ignored() {
        // base ← B (override "Pinned"), base ← C (newer leaf, no override).
        // The override walk runs along the main line leaf(C) → base; B is
        // off-path so its "Pinned" override doesn't count. With no on-path
        // override, the stack is placed by the root, so it lands in base's
        // current_section ("Review") — not "Pinned", and not the leaf's "Open".
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
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
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

        let review_count = items
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
            review_count, 3,
            "all three subgraph members should appear under the root's Review: {items:?}"
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

        let first = build_stacked_section_items(
            &state,
            &sections,
            None,
            &BTreeMap::new(),
            &collapsed,
            false,
        );
        // Call many times — every iteration constructs fresh internal
        // HashMaps with a new RandomState, so any non-determinism shows up
        // here. 32 calls is well past the birthday-paradox threshold.
        for _ in 0..32 {
            let again = build_stacked_section_items(
                &state,
                &sections,
                None,
                &BTreeMap::new(),
                &collapsed,
                false,
            );
            assert_eq!(
                again, first,
                "build_stacked_section_items must produce identical output on every call"
            );
        }
    }

    #[test]
    fn stacked_sections_sort_groups_by_root_entered_section_at() {
        // Two unstacked sessions both in "Open" (each is its own root); the
        // newer one's entered_section_at should sort it... wait —
        // build_sections sorts oldest-first within a section. The newer root
        // has a later entered_section_at and therefore appears AFTER the older
        // one.
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
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
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

    // `resolve_section_limit` is unit-tested where it lives, in `session::board`.

    #[test]
    fn section_grouped_header_carries_max_sessions_from_config() {
        let project_id = ProjectId::new();
        let mut s = make_session_in_section("s", "s", 0, "Review");
        s.project_id = project_id;

        let state = appstate_from(vec![s]);
        let sections = vec![claude_commander_core::session::SectionConfig {
            name: "Review".into(),
            max_sessions: Some(5),
            ..Default::default()
        }];

        let items = super::build_section_grouped_items(
            &state,
            &sections,
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
        );

        let review_header = items.iter().find_map(|i| match i {
            SessionListItem::SectionHeader {
                name,
                max_sessions,
                count,
                ..
            } if name == "Review" => Some((*count, *max_sessions)),
            _ => None,
        });
        assert_eq!(review_header, Some((1, Some(5))));
    }

    #[test]
    fn section_grouped_in_progress_header_carries_in_progress_limit() {
        let project_id = ProjectId::new();
        let mut s = make_session("a", "a", 0);
        s.project_id = project_id;

        let state = appstate_from(vec![s]);
        let sections: Vec<claude_commander_core::session::SectionConfig> = vec![];

        let items = super::build_section_grouped_items(
            &state,
            &sections,
            Some(2),
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
        );

        let ip_header = items.iter().find_map(|i| match i {
            SessionListItem::SectionHeader {
                name, max_sessions, ..
            } if name == claude_commander_core::session::IN_PROGRESS => Some(*max_sessions),
            _ => None,
        });
        assert_eq!(ip_header, Some(Some(2)));
    }

    #[test]
    fn stacked_section_header_carries_max_sessions() {
        let project_id = ProjectId::new();
        let mut s = make_session_in_section("s", "s", 0, "Review");
        s.project_id = project_id;

        let state = appstate_from(vec![s]);
        let sections = vec![claude_commander_core::session::SectionConfig {
            name: "Review".into(),
            max_sessions: Some(4),
            ..Default::default()
        }];

        let items = build_stacked_section_items(
            &state,
            &sections,
            Some(1),
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
        );

        let review_limit = items.iter().find_map(|i| match i {
            SessionListItem::SectionHeader {
                name, max_sessions, ..
            } if name == "Review" => Some(*max_sessions),
            _ => None,
        });
        let ip_limit = items.iter().find_map(|i| match i {
            SessionListItem::SectionHeader {
                name, max_sessions, ..
            } if name == claude_commander_core::session::IN_PROGRESS => Some(*max_sessions),
            _ => None,
        });
        assert_eq!(review_limit, Some(Some(4)));
        assert_eq!(ip_limit, Some(Some(1)));
    }

    #[test]
    fn hide_empty_sections_grouped_omits_empty_sections() {
        let project_id = ProjectId::new();
        let mut s = make_session_in_section("s", "s", 0, "Review");
        s.project_id = project_id;

        let state = appstate_from(vec![s]);
        let sections = vec![section_named("Open"), section_named("Review")];

        // With hide_empty_sections=false, all sections appear.
        let items = super::build_section_grouped_items(
            &state,
            &sections,
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            false,
        );
        let headers: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                SessionListItem::SectionHeader { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            headers.contains(&claude_commander_core::session::IN_PROGRESS),
            "In Progress should appear when hide_empty_sections is false",
        );
        assert!(
            headers.contains(&"Open"),
            "Open should appear when hide_empty_sections is false"
        );
        assert!(headers.contains(&"Review"), "Review should appear");

        // With hide_empty_sections=true, empty sections are omitted.
        let items = super::build_section_grouped_items(
            &state,
            &sections,
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            true,
        );
        let headers: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                SessionListItem::SectionHeader { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec!["Review"]);
    }

    #[test]
    fn hide_empty_sections_stacked_omits_empty_sections() {
        let project_id = ProjectId::new();
        let mut s = make_session_in_section("s", "s", 0, "Review");
        s.project_id = project_id;

        let state = appstate_from(vec![s]);
        let sections = vec![section_named("Open"), section_named("Review")];

        // With hide_empty_sections=true, empty sections are omitted.
        let items = build_stacked_section_items(
            &state,
            &sections,
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            true,
        );
        let headers: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                SessionListItem::SectionHeader { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec!["Review"]);
    }

    /// A project with no sessions is listed only under In Progress, so hiding
    /// an In Progress that holds no sessions made it unreachable. That is the
    /// normal case for a workspace whose projects are all new: every session
    /// is elsewhere, the In Progress count is zero, and the project vanished.
    #[test]
    fn hide_empty_sections_keeps_in_progress_for_a_project_without_sessions() {
        let mut state = claude_commander_core::config::AppState::default();
        let project =
            claude_commander_core::session::Project::new("empty", PathBuf::from("/tmp"), "main");
        let project_id = project.id;
        state.projects.insert(project_id, project);
        let snapshot = snapshot_from_state(&state);
        let sections = vec![section_named("Open"), section_named("Review")];

        for (view, items) in [
            (
                "grouped",
                super::build_section_grouped_items(
                    &snapshot,
                    &sections,
                    None,
                    &BTreeMap::new(),
                    &std::collections::HashSet::new(),
                    true,
                ),
            ),
            (
                "stacked",
                build_stacked_section_items(
                    &snapshot,
                    &sections,
                    None,
                    &BTreeMap::new(),
                    &std::collections::HashSet::new(),
                    true,
                ),
            ),
        ] {
            let headers: Vec<_> = items
                .iter()
                .filter_map(|i| match i {
                    SessionListItem::SectionHeader { name, count, .. } => {
                        Some((name.as_str(), *count))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                headers,
                vec![(claude_commander_core::session::IN_PROGRESS, 0)],
                "{view}: only In Progress, with no sessions counted",
            );
            assert!(
                items
                    .iter()
                    .any(|i| matches!(i, SessionListItem::Project { id, .. } if *id == project_id)),
                "{view}: the session-less project must still be listed",
            );
        }
    }

    #[test]
    fn hide_empty_sections_no_spacers_when_all_hidden() {
        // When all sections are empty and hide_empty_sections is true,
        // the result should be empty (no spacers or headers).
        let state = appstate_from(Vec::<WorktreeSession>::new());
        let sections = vec![section_named("Open"), section_named("Review")];

        let items = super::build_section_grouped_items(
            &state,
            &sections,
            None,
            &BTreeMap::new(),
            &std::collections::HashSet::new(),
            true,
        );
        assert!(
            items.is_empty(),
            "empty state with hide_empty_sections should yield no items"
        );
    }

    // -----------------------------------------------------------------------
    // Perf baseline: tree building over a large seeded state.
    //
    // The tree builders (`build_project_grouped_items` /
    // `build_section_grouped_items` / `build_stacked_section_items`) currently
    // read the local `AppState` directly. The Phase C refactor moves them onto
    // DTO snapshots behind a backend trait; this seeder + `#[ignore]`d timing
    // test pin the current cost so the refactor can prove it did not regress
    // tree building for local users.
    //
    // `seed_large_state` is intentionally reusable (`pub(super)`) so Phase C can
    // feed the *same* shaped state into the DTO-based builders and compare
    // apples-to-apples. Run with:
    //   cargo test -p claude-commander-core tree_build_perf_baseline -- --ignored --nocapture
    // -----------------------------------------------------------------------

    /// Build a deterministic `AppState` with `n_projects` projects, each holding
    /// `sessions_per_project` sessions. Roughly one in five sessions is a
    /// stacked child of the session before it (exercising `resolve_stack_parent`
    /// and the stack-ordering path), and sessions are round-robin assigned a
    /// `current_section` so the section builders have populated buckets. Also
    /// returns matching `agent_states` (one entry per session, cycling through
    /// the agent states) and a `sections` config for the section views.
    ///
    /// All timestamps are derived from a fixed base + per-session offset so the
    /// output ordering is fully determined (no wall-clock reads leak in).
    #[allow(clippy::type_complexity)]
    pub(super) fn seed_large_state(
        n_projects: usize,
        sessions_per_project: usize,
    ) -> (
        claude_commander_core::config::AppState,
        BTreeMap<SessionId, AgentState>,
        Vec<claude_commander_core::session::SectionConfig>,
    ) {
        use chrono::TimeZone;
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let section_names = ["Review", "Ready", "Blocked"];
        let sections: Vec<claude_commander_core::session::SectionConfig> =
            section_names.iter().map(|n| section_named(n)).collect();
        let agent_cycle = [
            AgentState::Working,
            AgentState::Idle,
            AgentState::WaitingForInput,
            AgentState::Unknown,
        ];

        let mut state = claude_commander_core::config::AppState::default();
        let mut agent_states = BTreeMap::new();
        let mut global_idx: i64 = 0;

        for p in 0..n_projects {
            let project = claude_commander_core::session::Project::new(
                format!("project-{p:02}"),
                PathBuf::from("/tmp/repo"),
                "main",
            );
            let project_id = project.id;
            state.projects.insert(project_id, project);

            let mut prev_in_project: Option<SessionId> = None;
            for s in 0..sessions_per_project {
                let mut session = WorktreeSession::new(
                    project_id,
                    format!("session-{p:02}-{s:03}"),
                    format!("branch-{p:02}-{s:03}"),
                    PathBuf::from("/tmp/wt"),
                    if s % 3 == 0 { "codex" } else { "claude" },
                );
                let ts = base + ChronoDuration::seconds(global_idx);
                session.created_at = ts;
                session.entered_section_at = ts;
                // Every 5th session (that has a predecessor) stacks on the one
                // before it, forming short chains within the project.
                if s % 5 == 0 && s != 0 {
                    session.stack_parent_session_id = prev_in_project;
                }
                // Spread sessions across the catch-all + configured sections.
                session.current_section = match s % 4 {
                    0 => None, // In Progress catch-all
                    other => Some(section_names[other - 1].to_string()),
                };
                session.unread = s % 7 == 0;
                if s % 6 == 0 {
                    session.pr_number = Some(1000 + global_idx as u32);
                    session.pr_state = Some(claude_commander_core::git::PrState::Open);
                }

                agent_states.insert(session.id, agent_cycle[(global_idx as usize) % 4]);
                prev_in_project = Some(session.id);

                let sid = session.id;
                state.sessions.insert(sid, session);
                state
                    .projects
                    .get_mut(&project_id)
                    .unwrap()
                    .add_worktree(sid);
                global_idx += 1;
            }
        }
        (state, agent_states, sections)
    }

    #[test]
    #[ignore = "perf baseline; run explicitly with --ignored --nocapture"]
    fn tree_build_perf_baseline() {
        // ~100 sessions across ~10 projects, matching the Phase C brief.
        let (state, agent_states, sections) = seed_large_state(10, 10);
        let session_count = state.sessions.len();
        assert!(
            session_count >= 100,
            "expected at least 100 sessions, got {session_count}"
        );
        let collapsed = std::collections::HashSet::new();
        // Project into the DTO snapshot the builders now consume, once, outside
        // the timed loop — we measure the builders, not snapshot construction
        // (the cached snapshot is built on change, not per refresh).
        let snapshot = snapshot_from_state(&state);

        // Warm up so the first-touch allocation cost doesn't dominate the timing.
        for _ in 0..50 {
            std::hint::black_box(build_project_grouped_items(&snapshot, &agent_states));
        }

        let iterations = 2_000u32;
        let time = |label: &str, f: &mut dyn FnMut()| {
            let start = std::time::Instant::now();
            for _ in 0..iterations {
                f();
            }
            let per = start.elapsed() / iterations;
            println!("{label:<28} {per:?}/build ({session_count} sessions)");
            // Generous ceiling: a build of ~100 sessions is microsecond-scale;
            // 5ms leaves multiple orders of magnitude of slack while still
            // catching a catastrophic regression (e.g. an accidental O(n^2)).
            assert!(
                per < std::time::Duration::from_millis(5),
                "{label} regressed: {per:?}/build exceeds the 5ms ceiling"
            );
        };

        time("project_grouped", &mut || {
            std::hint::black_box(build_project_grouped_items(&snapshot, &agent_states));
        });
        time("section_grouped", &mut || {
            std::hint::black_box(build_section_grouped_items(
                &snapshot,
                &sections,
                Some(5),
                &agent_states,
                &collapsed,
                false,
            ));
        });
        time("section_stacks", &mut || {
            std::hint::black_box(build_stacked_section_items(
                &snapshot,
                &sections,
                Some(5),
                &agent_states,
                &collapsed,
                false,
            ));
        });
    }
}

#[cfg(test)]
mod order_recent_tests {
    use super::order_recent;
    use chrono::{Duration as ChronoDuration, Utc};

    #[test]
    fn orders_newest_attached_first_and_caps_at_limit() {
        let now = Utc::now();
        // Deliberately shuffled input; oldest-to-newest offsets -30, 0, -10.
        let candidates = vec![
            (now - ChronoDuration::seconds(30), "old"),
            (now, "newest"),
            (now - ChronoDuration::seconds(10), "mid"),
        ];
        assert_eq!(
            order_recent(candidates.clone(), 5),
            vec!["newest", "mid", "old"]
        );
        // The limit truncates the tail after ordering.
        assert_eq!(order_recent(candidates, 2), vec!["newest", "mid"]);
    }

    #[test]
    fn zero_limit_and_empty_input_yield_nothing() {
        let now = Utc::now();
        assert!(order_recent(vec![(now, "x")], 0).is_empty());
        assert!(order_recent(Vec::<(_, &str)>::new(), 5).is_empty());
    }

    #[test]
    fn equal_timestamps_keep_input_order() {
        // A stable sort preserves the input (backend-then-tree) order for ties.
        let t = Utc::now();
        let candidates = vec![(t, "first"), (t, "second"), (t, "third")];
        assert_eq!(
            order_recent(candidates, 3),
            vec!["first", "second", "third"]
        );
    }
}
