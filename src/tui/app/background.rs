//! Background tasks: preview updates, PR status checks, info fetching, AI summaries.

use futures::StreamExt;

use super::*;

/// Cap concurrent subprocess fan-outs (e.g. `gh pr list` across all
/// sessions). Each call holds 3+ pipe FDs, so unbounded fan-out can
/// EMFILE under the macOS launchd 256-FD default.
const PR_FANOUT_CONCURRENCY: usize = 8;

/// Cap concurrent project-branch pulls so a user with many projects
/// doesn't spawn one `git fetch` per project at the same instant.
const PROJECT_PULL_FANOUT_CONCURRENCY: usize = 4;

impl App {
    /// Spawn a background task to fetch preview/diff/shell data.
    ///
    /// The task runs in parallel with the main event loop so that
    /// keyboard input is never blocked by I/O. Results arrive as
    /// `StateUpdate::PreviewReady` events.
    pub(super) fn spawn_preview_update(&mut self) {
        // Skip if a fetch is already in flight (with 5s safety timeout)
        if let Some(spawned_at) = self.ui_state.preview_update_spawned_at {
            if spawned_at.elapsed() < Duration::from_secs(5) {
                return;
            }
            debug!("Preview update stale (>5s), spawning new one");
        }

        let session_id = self.ui_state.selected_session_id;
        let project_id = self.ui_state.selected_project_id;
        let mgr = self.service.session_manager().clone();
        let tx = self.event_loop.sender();

        self.ui_state.preview_update_spawned_at = Some(Instant::now());

        debug!(
            "Spawning preview update for session={:?} project={:?}",
            session_id, project_id
        );

        tokio::spawn(async move {
            let (preview_content, diff_info, shell_content) =
                fetch_preview_data(&mgr, session_id, project_id).await;

            debug!(
                "Preview fetch complete, sending PreviewReady (preview_len={} diff_lines={})",
                preview_content.len(),
                diff_info.line_count
            );

            let _ = tx
                .send(AppEvent::StateUpdate(StateUpdate::PreviewReady {
                    session_id,
                    project_id,
                    preview_content,
                    diff_info,
                    shell_content,
                }))
                .await;
        });
    }

    /// Spawn a background task to check PR status for all sessions
    pub(super) fn spawn_pr_status_check(&mut self) {
        self.ui_state.last_pr_check = Some(Instant::now());

        let store = self.service.store().clone();
        let tx = self.event_loop.sender();

        tokio::spawn(async move {
            // Collect session info under a brief read lock
            let sessions_to_check: Vec<(SessionId, String, std::path::PathBuf)> = {
                let state = store.read().await;
                state
                    .sessions
                    .values()
                    .filter(|s| s.status != SessionStatus::Creating)
                    .filter_map(|s| {
                        let project = state.projects.get(&s.project_id)?;
                        Some((s.id, s.branch.clone(), project.repo_path.clone()))
                    })
                    .collect()
            };

            let results: Vec<_> = futures::stream::iter(sessions_to_check.into_iter().map(
                |(session_id, branch, repo_path)| async move {
                    let pr_result = check_pr_for_branch(&repo_path, &branch).await;
                    (session_id, pr_result)
                },
            ))
            .buffer_unordered(PR_FANOUT_CONCURRENCY)
            .collect()
            .await;

            let _ = tx
                .send(AppEvent::StateUpdate(StateUpdate::PrStatusReady {
                    results,
                }))
                .await;
        });
    }

    /// Spawn background fetches for info pane data (enriched PR + AI summary).
    ///
    /// Only called from user-initiated actions (pane switch, selection change).
    /// Not called from background ticks to avoid unnecessary regeneration.
    pub(super) fn spawn_info_fetch(&mut self) {
        // Only relevant when the Info pane is active
        if self.ui_state.right_pane_view != RightPaneView::Info {
            return;
        }

        let Some(session_id) = self.ui_state.selected_session_id else {
            return;
        };

        // Find the session's PR number and project repo path
        let session_info = self.ui_state.list_items.iter().find_map(|item| {
            if let SessionListItem::Worktree { id, pr_number, .. } = item {
                if *id == session_id {
                    Some(*pr_number)
                } else {
                    None
                }
            } else {
                None
            }
        });

        let Some(pr_number) = session_info.flatten() else {
            // No PR for this session — skip enriched PR fetch
            return;
        };

        // Spawn enriched PR fetch if not already cached for this session
        let needs_enriched = self
            .ui_state
            .enriched_pr
            .as_ref()
            .is_none_or(|(sid, _)| *sid != session_id);

        if needs_enriched && self.ui_state.gh_available {
            let store = self.service.store().clone();
            let tx = self.event_loop.sender();

            tokio::spawn(async move {
                // Look up the project repo path
                let repo_path = {
                    let state = store.read().await;
                    state
                        .sessions
                        .get(&session_id)
                        .and_then(|s| state.projects.get(&s.project_id))
                        .map(|p| p.repo_path.clone())
                };

                let info = if let Some(repo_path) = repo_path {
                    fetch_enriched_pr(&repo_path, pr_number).await
                } else {
                    None
                };

                let _ = tx
                    .send(AppEvent::StateUpdate(StateUpdate::EnrichedPrReady {
                        session_id,
                        info,
                    }))
                    .await;
            });
        }
    }

    /// Walk the project list, decide which projects are due for a pull
    /// this tick (respecting the `project_pull_interval_secs` cadence,
    /// a 5s startup grace, and the in-flight set), and dispatch them via
    /// `spawn_project_pulls`.
    pub(super) async fn maybe_spawn_project_pulls(&mut self) {
        // Honour a small grace period after startup so the first tick after
        // launch doesn't immediately hammer every project.
        const STARTUP_GRACE: Duration = Duration::from_secs(5);
        if self.ui_state.started_at.elapsed() < STARTUP_GRACE {
            return;
        }

        let interval = Duration::from_secs(self.config.project_pull_interval_secs);

        // Cheap global throttle: a project can become due at most once per
        // `interval`, so sweeping the project list (state lock + clone) more
        // often than that on every render tick is wasted work. The per-project
        // `last_project_pull` cadence still governs which projects actually run.
        if let Some(last) = self.ui_state.last_project_pull_sweep
            && last.elapsed() < interval
        {
            return;
        }
        self.ui_state.last_project_pull_sweep = Some(Instant::now());

        let projects: Vec<(ProjectId, std::path::PathBuf, String)> = {
            let state = self.service.store().read().await;
            state
                .projects
                .values()
                .map(|p| (p.id, p.repo_path.clone(), p.main_branch.clone()))
                .collect()
        };

        let mut due: Vec<(ProjectId, std::path::PathBuf, String)> = Vec::new();
        for (id, path, main) in projects {
            if self.ui_state.project_pull_in_flight.contains(&id) {
                continue;
            }
            let is_due = match self.ui_state.last_project_pull.get(&id) {
                Some(t) => t.elapsed() >= interval,
                None => true,
            };
            if is_due {
                self.ui_state.project_pull_in_flight.insert(id);
                due.push((id, path, main));
            }
        }

        self.spawn_project_pulls(due);
    }

    /// Spawn background fast-forward pulls for each project listed in
    /// `due`. Sends one `ProjectPullFinished` event per project as work
    /// completes. The caller is responsible for marking the projects as
    /// in-flight in `UiState` so we don't double-spawn.
    pub(super) fn spawn_project_pulls(&self, due: Vec<(ProjectId, std::path::PathBuf, String)>) {
        if due.is_empty() {
            return;
        }
        let tx = self.event_loop.sender();
        tokio::spawn(async move {
            futures::stream::iter(due.into_iter().map(|(project_id, repo_path, main_branch)| {
                let tx = tx.clone();
                async move {
                    let outcome = run_project_pull(&repo_path, &main_branch).await;
                    let _ = tx
                        .send(AppEvent::StateUpdate(StateUpdate::ProjectPullFinished {
                            project_id,
                            outcome,
                        }))
                        .await;
                }
            }))
            .buffer_unordered(PROJECT_PULL_FANOUT_CONCURRENCY)
            .for_each(|_| async {})
            .await;
        });
    }

    /// Spawn AI summary generation for the given session.
    ///
    /// Called from the `GenerateSummary` hotkey handler. Always generates
    /// (unless already in flight or AI is disabled). Computes a full branch
    /// diff (committed vs main + uncommitted) and pipes it into Claude.
    pub(super) fn spawn_ai_summary_if_needed(&mut self, session_id: SessionId) {
        if !self.config.ai_summary_enabled {
            return;
        }

        // Don't spawn if already in flight
        if matches!(
            self.ui_state.ai_summaries.get(&session_id),
            Some(AiSummary::Loading)
        ) {
            return;
        }

        self.ui_state
            .ai_summaries
            .insert(session_id, AiSummary::Loading);

        let store = self.service.store().clone();
        let model = self.config.ai_summary_model.clone();
        let tx = self.event_loop.sender();

        tokio::spawn(async move {
            let session_info = {
                let state = store.read().await;
                state.sessions.get(&session_id).and_then(|s| {
                    let project = state.projects.get(&s.project_id)?;
                    Some((s.worktree_path.clone(), project.main_branch.clone()))
                })
            };

            let result = if let Some((worktree_path, main_branch)) = session_info {
                let diff_text = crate::git::compute_branch_diff(&worktree_path, &main_branch).await;
                let new_hash = diff_hash(&diff_text);
                let summary_result = fetch_branch_summary(&diff_text, &model).await;
                (summary_result, new_hash)
            } else {
                (Err("Session not found".to_string()), 0)
            };

            let _ = tx
                .send(AppEvent::StateUpdate(StateUpdate::AiSummaryReady {
                    session_id,
                    result: result.0,
                    diff_hash: result.1,
                }))
                .await;
        });
    }
}

/// Fetch preview/diff/shell data for the currently selected session or project.
///
/// Runs outside the main event loop so it never blocks keyboard input.
pub(super) async fn fetch_preview_data(
    mgr: &SessionManager,
    session_id: Option<SessionId>,
    project_id: Option<ProjectId>,
) -> (String, Arc<DiffInfo>, String) {
    if let Some(sid) = session_id {
        // Check if session is still Creating (no tmux session to capture yet)
        let is_creating = {
            let state = mgr.store.read().await;
            state
                .get_session(&sid)
                .is_some_and(|s| s.status == SessionStatus::Creating)
        };
        if is_creating {
            return (
                "Creating session...".to_string(),
                Arc::new(DiffInfo::empty()),
                String::new(),
            );
        }

        debug!(
            "fetch_preview_data: fetching content/diff/shell for session {}",
            sid
        );
        let (preview_result, diff_result, shell_result) = tokio::join!(
            mgr.get_content(&sid),
            mgr.get_diff(&sid),
            mgr.get_shell_content(&sid),
        );

        let preview = preview_result.map(|c| c.content).unwrap_or_else(|e| {
            debug!("fetch_preview_data: get_content error: {}", e);
            "Unable to capture content".to_string()
        });
        let diff = diff_result.unwrap_or_else(|e| {
            debug!("fetch_preview_data: get_diff error: {}", e);
            Arc::new(DiffInfo::empty())
        });
        let shell = match shell_result {
            Ok(Some(c)) => c.content,
            Ok(None) => "No shell session. Press 's' to open one.".to_string(),
            Err(e) => {
                debug!("fetch_preview_data: get_shell_content error: {}", e);
                "No shell session. Press 's' to open one.".to_string()
            }
        };

        (preview, diff, shell)
    } else if let Some(pid) = project_id {
        debug!(
            "fetch_preview_data: fetching diff/shell for project {}",
            pid
        );
        let (diff_result, shell_result) = tokio::join!(
            mgr.get_project_diff(&pid),
            mgr.get_project_shell_content(&pid),
        );

        let diff = diff_result.unwrap_or_else(|e| {
            debug!("fetch_preview_data: get_project_diff error: {}", e);
            Arc::new(DiffInfo::empty())
        });
        let shell = match shell_result {
            Ok(Some(c)) => c.content,
            _ => "No shell session. Press 's' to open one.".to_string(),
        };

        (String::new(), diff, shell)
    } else {
        debug!("fetch_preview_data: no selection");
        (
            "Select a session to see preview".to_string(),
            Arc::new(DiffInfo::empty()),
            String::new(),
        )
    }
}

/// Kill tmux sessions and remove a git worktree in the background.
///
/// Sends an error event if worktree removal fails.
pub(super) async fn cleanup_session_tmux(
    tmux: &crate::tmux::TmuxExecutor,
    tmux_name: &str,
    shell_tmux_name: Option<&str>,
    worktree_path: Option<(&std::path::Path, &std::path::Path)>,
    tx: &tokio::sync::mpsc::Sender<AppEvent>,
) {
    if let Err(e) = tmux.kill_session(tmux_name).await {
        debug!("Failed to kill tmux session: {}", e);
    }
    if let Some(shell_name) = shell_tmux_name {
        let _ = tmux.kill_session(shell_name).await;
    }
    if let Some((worktree_path, repo_path)) = worktree_path {
        let output = tokio::process::Command::new("git")
            .current_dir(repo_path)
            .args(["worktree", "remove", "--force"])
            .arg(worktree_path)
            .output()
            .await;
        if let Err(e) = output.as_ref().map_err(|e| e.to_string()).and_then(|o| {
            if o.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&o.stderr).into_owned())
            }
        }) {
            let _ = tx
                .send(AppEvent::StateUpdate(StateUpdate::Error {
                    message: format!("Background cleanup failed: {}", e),
                }))
                .await;
        }
    }
}
