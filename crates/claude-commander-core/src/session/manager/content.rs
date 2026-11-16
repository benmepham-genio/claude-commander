//! Session content retrieval: attach commands, captured content, and diffs.

use super::*;

impl SessionManager {
    /// Attach to a session (returns tmux session name for external attach)
    pub async fn get_attach_command(&self, session_id: &SessionId) -> Result<String> {
        let tmux_name = self.ensure_attachable(session_id).await?;
        let cmd = format!("tmux attach-session -t {}", tmux_name);
        info!("Returning attach command: {}", cmd);
        Ok(cmd)
    }

    /// Ensure a session's tmux session is live and attachable, returning its
    /// tmux session name. Validates the session can be attached, and recreates
    /// the tmux session (resuming the agent and reconfiguring the status bar)
    /// when it is missing or its pane has died — so every frontend's attach
    /// path (TUI, CLI, and the backend trait) gets the same revive-on-attach
    /// behaviour rather than failing on a stale session.
    pub async fn ensure_attachable(&self, session_id: &SessionId) -> Result<String> {
        info!("ensure_attachable called for session: {}", session_id);

        let (tmux_name, worktree_path, title, program, status, hibernated, status_bar) = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;

            info!(
                "Session found, status: {:?}, can_attach: {}",
                session.status,
                session.status.can_attach()
            );

            if !session.status.can_attach() {
                return Err(SessionError::InvalidState(*session_id).into());
            }

            (
                session.tmux_session_name.clone(),
                session.worktree_path.clone(),
                session.title.clone(),
                session.program.clone(),
                session.status,
                session.hibernated,
                self.status_bar_info(session, &state),
            )
        };

        info!("Checking if tmux session '{}' exists", tmux_name);

        // Verify tmux session actually exists before returning attach command
        let exists = self.tmux.session_exists(&tmux_name).await?;
        info!("Tmux session exists: {}", exists);

        let needs_recreate = if !exists {
            info!("Tmux session not found, will recreate");
            true
        } else {
            // Check if the pane is dead (program exited)
            let pane_dead = self.tmux.is_pane_dead(&tmux_name).await.unwrap_or(false);
            info!("Pane dead: {}", pane_dead);
            if pane_dead {
                info!("Pane is dead, killing tmux session for recreation");
                let _ = self.tmux.kill_session(&tmux_name).await;
                true
            } else {
                false
            }
        };

        if needs_recreate {
            // Recreate the tmux session, resuming the prior agent session when
            // configured, or unconditionally when it was auto-hibernated (resume
            // is what makes hibernation non-destructive). Resume syntax is
            // harness-specific; an unrecognised program launches fresh.
            let force_resume = self.config_store.read().resume_session || hibernated;
            let resume_program = super::lifecycle::resume_program_for(&program, force_resume);
            let resume_program =
                super::lifecycle::program_with_session_name(&resume_program, &title);
            let resume_program = self.maybe_wrap_nix_develop(&resume_program, &worktree_path);
            info!("Recreating tmux session with: {}", resume_program);
            self.tmux
                .create_session(&tmux_name, &worktree_path, Some(&resume_program))
                .await?;

            // Configure CC status bar on the recreated session
            self.tmux
                .configure_status_bar(&tmux_name, &status_bar)
                .await;
        }

        // The pane is live now — recreated above, or alive all along. Reflect
        // that in the stored status and clear the hibernation marker: the pane
        // holds the conversation, so a future wake shouldn't force resume again.
        //
        // The already-alive case matters as much as the recreate one: a session
        // can legitimately reach here `Stopped` with a live pane (a reconcile
        // pass that read tmux as absent — see `api::reconciled_status`), and
        // leaving it Stopped keeps agent-state detection suppressed, so the row
        // renders `○` while the user sits in a working agent pane.
        if needs_recreate || status != SessionStatus::Running {
            let sid = *session_id;
            let _ = self
                .store
                .mutate(move |state| {
                    if let Some(session) = state.get_session_mut(&sid) {
                        session.set_status(SessionStatus::Running);
                        session.hibernated = false;
                    }
                })
                .await;
        }

        Ok(tmux_name)
    }

    /// Get captured content for a session
    pub async fn get_content(&self, session_id: &SessionId) -> Result<CapturedContent> {
        let tmux_session_name = {
            let state = self.store.read().await;
            state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?
                .tmux_session_name
                .clone()
        };

        self.content_capture.get_content(&tmux_session_name).await
    }

    /// Get diff for a session
    pub async fn get_diff(&self, session_id: &SessionId) -> Result<Arc<DiffInfo>> {
        self.get_diff_part(session_id, false).await
    }
    pub async fn get_diff_stats(&self, session_id: &SessionId) -> Result<Arc<DiffInfo>> {
        self.get_diff_part(session_id, true).await
    }
    async fn get_diff_part(
        &self,
        session_id: &SessionId,
        stats_only: bool,
    ) -> Result<Arc<DiffInfo>> {
        let worktree_path = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            session.worktree_path.clone()
        };

        if stats_only {
            self.diff_cache.get_stats(session_id, &worktree_path).await
        } else {
            self.diff_cache.get_diff(session_id, &worktree_path).await
        }
    }
}
