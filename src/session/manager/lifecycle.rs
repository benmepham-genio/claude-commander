//! Session lifecycle: create, restart, kill, and delete sessions.

use super::*;

impl SessionManager {
    /// Prepare a placeholder session in `Creating` state.
    ///
    /// This inserts the session into state immediately so the UI can show a
    /// spinner. Call `finalize_session` in a background task to do the heavy
    /// git/tmux work.
    ///
    /// When `base_branch` is `Some`, the worktree will be created against
    /// that branch (existing local branch, or created from `origin/<branch>`
    /// if only the remote tracking branch exists). When `None`, a new branch
    /// is generated from `title` using the configured branch prefix.
    #[instrument(skip(self))]
    pub async fn prepare_session(
        &self,
        project_id: &ProjectId,
        title: String,
        program: Option<String>,
        base_branch: Option<String>,
    ) -> Result<SessionId> {
        let program = program.unwrap_or_else(|| self.config_store.read().default_program.clone());

        // Validate project exists
        {
            let state = self.store.read().await;
            state
                .get_project(project_id)
                .ok_or_else(|| SessionError::ProjectNotFound(project_id.to_string()))?;
        }

        let branch_name = match base_branch {
            Some(b) => b,
            None => self.generate_branch_name(&title),
        };

        let session = WorktreeSession::new_creating(*project_id, title, branch_name, program);
        let session_id = session.id;

        self.store
            .mutate(move |state| {
                state.add_session(session);
            })
            .await?;

        info!("Prepared creating session {}", session_id);
        Ok(session_id)
    }

    /// Finalize a session that was created with `prepare_session`.
    ///
    /// Performs the heavy work: git fetch, worktree creation, tmux session
    /// setup. On success, transitions the session from `Creating` to `Running`.
    #[instrument(skip(self))]
    pub async fn finalize_session(&self, session_id: &SessionId) -> Result<SessionId> {
        // Read session and project info, plus stack parent's branch if any so
        // we know to fork from it below.
        let (project_id, title, branch_name, program, stack_parent_branch) = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            let parent_branch = session
                .stack_parent_session_id
                .and_then(|pid| state.get_session(&pid))
                .map(|p| p.branch.clone());
            (
                session.project_id,
                session.title.clone(),
                session.branch.clone(),
                session.program.clone(),
                parent_branch,
            )
        };

        let (repo_path, main_branch) = {
            let state = self.store.read().await;
            let project = state
                .get_project(&project_id)
                .ok_or_else(|| SessionError::ProjectNotFound(project_id.to_string()))?;
            (project.repo_path.clone(), project.main_branch.clone())
        };

        info!(
            "Finalizing session '{}' with branch '{}' in project {}",
            title, branch_name, project_id
        );

        // Fetch latest changes from origin
        if self.config_store.read().fetch_before_create {
            info!(
                "Fetching latest changes from origin in {}",
                repo_path.display()
            );
            let output = tokio::process::Command::new("git")
                .current_dir(&repo_path)
                .args(["fetch", "origin"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("git fetch failed (continuing anyway): {}", stderr);
            }
        }

        // Generate unique worktree name
        let worktree_name = format!(
            "{}-{}",
            self.sanitize_name(&title),
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("")
        );

        // Create worktree — sync gix work (branch check + start point) is done
        // in a block so non-Sync types are dropped before the first .await,
        // keeping the overall future Send.
        let repo_name = sanitize_name(
            repo_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown"),
        );
        let worktrees_dir = self.config_store.read().resolve_worktrees_dir(&repo_name)?;
        let (branch_exists, start_point) = {
            let backend = GitBackend::open(&repo_path)?;
            let exists = backend.branch_exists(&branch_name)?;
            // For a stacked session we fork the new branch off the parent
            // session's local branch (which exists on disk thanks to its own
            // worktree). This overrides the usual origin/<branch>/origin/<main>
            // fallback so the stack topology is preserved.
            let sp = if let Some(parent_branch) = stack_parent_branch.as_deref() {
                if !exists && backend.branch_exists(parent_branch)? {
                    Some(parent_branch.to_string())
                } else {
                    None
                }
            } else {
                // Prefer origin/<branch_name> as the start point when the local
                // branch doesn't exist — this supports checking out an existing
                // remote branch (e.g. via the Checkout modal) as well as falling
                // back to origin/<main_branch> when creating a fresh branch.
                let branch_remote_ref = format!("refs/remotes/origin/{}", branch_name);
                let main_remote_ref = format!("refs/remotes/origin/{}", main_branch);
                if !exists && backend.ref_exists(&branch_remote_ref)? {
                    Some(format!("origin/{}", branch_name))
                } else if backend.ref_exists(&main_remote_ref)? {
                    Some(format!("origin/{}", main_branch))
                } else {
                    None
                }
            };
            (exists, sp)
        };
        let worktree_path = worktrees_dir.join(&worktree_name);
        let worktree_info = WorktreeManager::run_create_worktree(
            worktrees_dir,
            repo_path.clone(),
            worktree_path,
            branch_name.clone(),
            branch_exists,
            start_point,
        )
        .await?;

        // Read tmux_session_name from the placeholder session
        let tmux_session_name = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            session.tmux_session_name.clone()
        };

        // For stacked sessions running `claude`, prepend an initial prompt
        // arg so Claude knows the correct `--base` when the user asks it to
        // open a PR. Non-claude programs get the unchanged program string.
        let launch_cmd = match stack_parent_branch.as_deref() {
            Some(pb) if program_is_claude(&program) => program_with_stack_context(&program, pb),
            _ => program.clone(),
        };

        // Create tmux session in the worktree directory
        self.tmux
            .create_session(&tmux_session_name, &worktree_info.path, Some(&launch_cmd))
            .await?;

        // Update session to Running with the real worktree info
        let sid = *session_id;
        let wt_path = worktree_info.path.clone();
        let head = worktree_info.head.clone();
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&sid) {
                    session.worktree_path = wt_path;
                    session.base_commit = Some(head);
                    session.set_status(SessionStatus::Running);
                }
            })
            .await?;

        // Configure CC status bar (branch only, no PR yet)
        let status_bar = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            self.status_bar_info(session, &state)
        };
        self.tmux
            .configure_status_bar(&tmux_session_name, &status_bar)
            .await;

        info!(
            "Finalized session {} with tmux session {}",
            session_id, tmux_session_name
        );
        Ok(*session_id)
    }

    /// Remove a session that is still in `Creating` state (e.g., on failure or startup cleanup).
    #[instrument(skip(self))]
    pub async fn remove_creating_session(&self, session_id: &SessionId) -> Result<()> {
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                state.remove_session(&sid);
            })
            .await?;
        info!("Removed creating session {}", session_id);
        Ok(())
    }

    /// Kill tmux sessions (main + shell) for a worktree session.
    pub(super) async fn kill_tmux_sessions(&self, tmux_name: &str, shell_tmux_name: Option<&str>) {
        if let Err(e) = self.tmux.kill_session(tmux_name).await {
            warn!("Failed to kill tmux session: {}", e);
        }
        if let Some(shell_name) = shell_tmux_name {
            let _ = self.tmux.kill_session(shell_name).await;
        }
    }

    /// Restart a session (kill tmux and recreate, optionally with --resume)
    #[instrument(skip(self))]
    pub async fn restart_session(&self, session_id: &SessionId) -> Result<()> {
        let (tmux_session_name, shell_tmux_name, worktree_path, program, status_bar) = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            (
                session.tmux_session_name.clone(),
                session.shell_tmux_session_name.clone(),
                session.worktree_path.clone(),
                session.program.clone(),
                self.status_bar_info(session, &state),
            )
        };

        self.kill_tmux_sessions(&tmux_session_name, shell_tmux_name.as_deref())
            .await;

        // Create a fresh tmux session, adding --resume if configured
        let resume_program = if self.config_store.read().resume_session {
            format!("{} --resume", program)
        } else {
            program.clone()
        };
        let create_result = self
            .tmux
            .create_session(&tmux_session_name, &worktree_path, Some(&resume_program))
            .await;

        if let Err(e) = create_result {
            // Tmux is dead but recreation failed — mark as Stopped so state is consistent
            let sid = *session_id;
            let _ = self
                .store
                .mutate(move |state| {
                    if let Some(session) = state.get_session_mut(&sid) {
                        session.set_status(SessionStatus::Stopped);
                    }
                })
                .await;
            return Err(e);
        }

        // Configure status bar on the new session
        self.tmux
            .configure_status_bar(&tmux_session_name, &status_bar)
            .await;

        // Set status to Running
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&sid) {
                    session.set_status(SessionStatus::Running);
                }
            })
            .await?;

        info!("Restarted session {}", session_id);
        Ok(())
    }

    /// Kill a session (stop tmux, optionally remove worktree)
    #[instrument(skip(self))]
    pub async fn kill_session(&self, session_id: &SessionId, remove_worktree: bool) -> Result<()> {
        let session = {
            let state = self.store.read().await;
            state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?
                .clone()
        };

        self.kill_tmux_sessions(
            &session.tmux_session_name,
            session.shell_tmux_session_name.as_deref(),
        )
        .await;

        // Optionally remove worktree
        if remove_worktree {
            let repo_path = {
                let state = self.store.read().await;
                state
                    .get_project(&session.project_id)
                    .map(|p| p.repo_path.clone())
            };

            if let Some(repo_path) = repo_path
                && let Ok(backend) = GitBackend::open(&repo_path)
            {
                let worktree_manager =
                    WorktreeManager::new(backend, self.config_store.read().worktrees_dir()?);
                if let Err(e) = worktree_manager
                    .remove_worktree(&session.worktree_path, true)
                    .await
                {
                    warn!("Failed to remove worktree: {}", e);
                }
            }
        }

        // Update state
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&sid) {
                    session.set_status(SessionStatus::Stopped);
                }
            })
            .await?;

        info!("Killed session {}", session_id);
        Ok(())
    }

    /// Delete a session (remove from state)
    #[instrument(skip(self))]
    pub async fn delete_session(&self, session_id: &SessionId) -> Result<()> {
        // First kill if active
        {
            let state = self.store.read().await;
            if let Some(session) = state.get_session(session_id)
                && session.status.is_active()
            {
                drop(state);
                self.kill_session(session_id, true).await?;
            }
        }

        // Remove from state
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                state.remove_session(&sid);
            })
            .await?;

        info!("Deleted session {}", session_id);
        Ok(())
    }
}

/// Whether the program string starts with the `claude` CLI. Used to decide
/// whether an appended initial-prompt arg will be understood or will break
/// the invocation (e.g. for `bash` or `zsh` as the program).
fn program_is_claude(program: &str) -> bool {
    program.split_whitespace().next() == Some("claude")
}

/// Build the launch command for a stacked session running Claude Code.
///
/// Appends a single-quoted initial prompt telling Claude the stack parent's
/// branch name, so that when the user later asks Claude to open a PR it uses
/// `--base <parent_branch>` rather than defaulting to `main`. Branch names
/// cannot contain single quotes per git's refname rules, so no extra escaping
/// is needed.
fn program_with_stack_context(program: &str, parent_branch: &str) -> String {
    let prompt = format!(
        "This branch is stacked on `{parent_branch}` (not main). \
         When creating a PR for this session, use: gh pr create --base {parent_branch}"
    );
    format!("{program} '{prompt}'")
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn program_is_claude_matches_bare_command() {
        assert!(program_is_claude("claude"));
    }

    #[test]
    fn program_is_claude_matches_with_args() {
        assert!(program_is_claude("claude --resume"));
    }

    #[test]
    fn program_is_claude_rejects_other_shells() {
        assert!(!program_is_claude("bash"));
        assert!(!program_is_claude("zsh -l"));
        assert!(!program_is_claude(""));
    }

    #[test]
    fn stack_context_embeds_parent_branch() {
        let cmd = program_with_stack_context("claude", "feature-auth");
        assert!(cmd.starts_with("claude '"));
        assert!(cmd.contains("feature-auth"));
        assert!(cmd.contains("gh pr create --base feature-auth"));
        assert!(cmd.ends_with('\''));
    }

    #[test]
    fn stack_context_preserves_program_flags() {
        let cmd = program_with_stack_context("claude --resume", "base-branch");
        assert!(cmd.starts_with("claude --resume '"));
    }
}
