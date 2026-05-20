//! Session content retrieval: attach commands, captured content, and diffs.

use super::*;

impl SessionManager {
    /// Attach to a session (returns tmux session name for external attach)
    pub async fn get_attach_command(&self, session_id: &SessionId) -> Result<String> {
        info!("get_attach_command called for session: {}", session_id);

        let (tmux_name, worktree_path, title, program, status_bar) = {
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
            // Recreate the tmux session, adding --resume if configured so the
            // agent picks up where it left off
            let resume_program = if self.config_store.read().resume_session {
                format!("{} --resume", program)
            } else {
                program.clone()
            };
            let resume_program =
                super::lifecycle::program_with_session_name(&resume_program, &title);
            info!("Recreating tmux session with: {}", resume_program);
            self.tmux
                .create_session(&tmux_name, &worktree_path, Some(&resume_program))
                .await?;

            // Configure CC status bar on the recreated session
            self.tmux
                .configure_status_bar(&tmux_name, &status_bar)
                .await;

            let sid = *session_id;
            let _ = self
                .store
                .mutate(move |state| {
                    if let Some(session) = state.get_session_mut(&sid) {
                        session.set_status(SessionStatus::Running);
                    }
                })
                .await;
        }

        let cmd = format!("tmux attach-session -t {}", tmux_name);
        info!("Returning attach command: {}", cmd);
        Ok(cmd)
    }

    /// Attach to a multi-repo session (returns tmux attach command)
    pub async fn get_multi_repo_attach_command(
        &self,
        session_id: &crate::session::MultiRepoSessionId,
    ) -> Result<String> {
        let (tmux_name, parent_dir, program, status) = {
            let state = self.store.read().await;
            let session = state.get_multi_repo_session(session_id).ok_or_else(|| {
                SessionError::CreationFailed("Multi-repo session not found".into())
            })?;
            if !session.status.can_attach() {
                return Err(SessionError::CreationFailed(
                    "Session is not in an attachable state".into(),
                )
                .into());
            }
            (
                session.tmux_session_name.clone(),
                session.parent_dir.clone(),
                session.program.clone(),
                session.status,
            )
        };

        let exists = self.tmux.session_exists(&tmux_name).await?;
        let needs_recreate = if !exists {
            true
        } else {
            let pane_dead = self.tmux.is_pane_dead(&tmux_name).await.unwrap_or(false);
            if pane_dead {
                let _ = self.tmux.kill_session(&tmux_name).await;
                true
            } else {
                false
            }
        };

        if needs_recreate {
            let resume_program = if self.config_store.read().resume_session
                && status == SessionStatus::Stopped
            {
                format!("{} --resume", program)
            } else {
                program.clone()
            };
            self.tmux
                .create_session(&tmux_name, &parent_dir, Some(&resume_program))
                .await?;

            let sid = *session_id;
            let _ = self
                .store
                .mutate(move |state| {
                    if let Some(session) = state.get_multi_repo_session_mut(&sid) {
                        session.set_status(SessionStatus::Running);
                    }
                })
                .await;
        }

        Ok(format!("tmux attach-session -t {}", tmux_name))
    }

    /// Get captured content for a multi-repo session
    pub async fn get_multi_repo_content(
        &self,
        session_id: &crate::session::MultiRepoSessionId,
    ) -> Result<CapturedContent> {
        let tmux_session_name = {
            let state = self.store.read().await;
            state
                .get_multi_repo_session(session_id)
                .ok_or_else(|| {
                    SessionError::CreationFailed("Multi-repo session not found".into())
                })?
                .tmux_session_name
                .clone()
        };

        self.content_capture.get_content(&tmux_session_name).await
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
        let worktree_path = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            session.worktree_path.clone()
        };

        self.diff_cache.get_diff(session_id, &worktree_path).await
    }
}
