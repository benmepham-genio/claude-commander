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

    /// If `base_branch` matches an existing session's branch in the same
    /// project, link the new session as stacked by setting
    /// `stack_parent_session_id`. When multiple sessions share a branch, the
    /// most recently created one is chosen. No-op when `base_branch` is `None`
    /// or doesn't match any session.
    ///
    /// Call this between `prepare_session` and `finalize_session` so that
    /// `finalize_session` can inject the PR-base context into the Claude
    /// prompt and cascade/push-stack operations recognise the relationship.
    pub async fn link_stack_parent_by_branch(
        &self,
        session_id: &SessionId,
        base_branch: Option<&str>,
    ) -> Result<()> {
        let Some(base) = base_branch else {
            return Ok(());
        };
        let sid = *session_id;
        let base = base.to_string();
        self.store
            .mutate(move |state| {
                let session_project = state.get_session(&sid).map(|s| s.project_id);
                if let Some(pid) = session_project {
                    let parent_id = state
                        .sessions
                        .values()
                        .filter(|s| s.project_id == pid && s.branch == base && s.id != sid)
                        .max_by_key(|s| s.created_at)
                        .map(|s| s.id);
                    if let Some(parent_id) = parent_id
                        && let Some(session) = state.get_session_mut(&sid)
                    {
                        session.stack_parent_session_id = Some(parent_id);
                    }
                }
            })
            .await
    }

    /// Finalize a session that was created with `prepare_session`.
    ///
    /// Performs the heavy work: git fetch, worktree creation, tmux session
    /// setup. On success, transitions the session from `Creating` to `Running`.
    #[instrument(skip(self))]
    pub async fn finalize_session(
        &self,
        session_id: &SessionId,
        initial_prompt: Option<String>,
    ) -> Result<SessionId> {
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
        let finalize_start = std::time::Instant::now();

        // Fetch latest changes from origin
        if self.config_store.read().fetch_before_create {
            info!(
                "Fetching latest changes from origin in {}",
                repo_path.display()
            );
            let fetch_start = std::time::Instant::now();
            let output = tokio::process::Command::new("git")
                .current_dir(&repo_path)
                .args(["fetch", "origin"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await?;
            info!(
                "[timing] git fetch origin took {}ms",
                fetch_start.elapsed().as_millis()
            );
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
        let worktree_create_start = std::time::Instant::now();
        let worktree_info = WorktreeManager::run_create_worktree(
            worktrees_dir,
            repo_path.clone(),
            worktree_path,
            branch_name.clone(),
            branch_exists,
            start_point,
        )
        .await?;
        info!(
            "[timing] run_create_worktree (git worktree add + worktree includes) took {}ms",
            worktree_create_start.elapsed().as_millis()
        );

        // Read tmux_session_name from the placeholder session
        let tmux_session_name = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            session.tmux_session_name.clone()
        };

        // Build a single positional prompt arg combining stack context (for
        // stacked sessions) and any user-provided initial prompt. The Claude
        // CLI accepts exactly one positional prompt, so both must be merged.
        let launch_cmd = {
            let mut prompt_parts: Vec<String> = Vec::new();
            if let Some(pb) = stack_parent_branch.as_deref()
                && program_is_claude(&program)
            {
                prompt_parts.push(format!(
                    "This branch is stacked on `{pb}` (not main). \
                     When creating a PR for this session, use: \
                     gh pr create --base {pb}"
                ));
            }
            if let Some(ref user_prompt) = initial_prompt
                && program_is_claude(&program)
            {
                prompt_parts.push(user_prompt.clone());
            }
            if prompt_parts.is_empty() {
                program.clone()
            } else {
                let combined = prompt_parts.join("\n\n");
                let escaped = shell_escape_single_quote(&combined);
                format!("{program} '{escaped}'")
            }
        };
        let launch_cmd = program_with_session_name(&launch_cmd, &title);

        // Create tmux session in the worktree directory
        let tmux_start = std::time::Instant::now();
        self.tmux
            .create_session(&tmux_session_name, &worktree_info.path, Some(&launch_cmd))
            .await?;
        info!(
            "[timing] tmux create_session took {}ms",
            tmux_start.elapsed().as_millis()
        );

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
        info!(
            "[timing] finalize_session total took {}ms",
            finalize_start.elapsed().as_millis()
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
        let (tmux_session_name, shell_tmux_name, worktree_path, title, program, status_bar) = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            (
                session.tmux_session_name.clone(),
                session.shell_tmux_session_name.clone(),
                session.worktree_path.clone(),
                session.title.clone(),
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
        let resume_program = program_with_session_name(&resume_program, &title);
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

    /// Restart a session's tmux pane without `--resume`, identified by tmux
    /// session name. Used when the process inside the pane exits (e.g. Claude
    /// had nothing to resume) so the user seamlessly gets a fresh conversation
    /// instead of being dropped back to the TUI.
    pub async fn restart_session_fresh_by_tmux_name(&self, tmux_name: &str) -> Result<()> {
        let (session_id, worktree_path, title, program, status_bar) = {
            let state = self.store.read().await;
            let session = state
                .sessions
                .values()
                .find(|s| s.tmux_session_name == tmux_name)
                .ok_or_else(|| SessionError::TmuxSessionNotFound(tmux_name.to_string()))?;
            (
                session.id,
                session.worktree_path.clone(),
                session.title.clone(),
                session.program.clone(),
                self.status_bar_info(session, &state),
            )
        };

        let _ = self.tmux.kill_session(tmux_name).await;

        let launch_cmd = program_with_session_name(&program, &title);
        let create_result = self
            .tmux
            .create_session(tmux_name, &worktree_path, Some(&launch_cmd))
            .await;

        if let Err(e) = create_result {
            let sid = session_id;
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

        self.tmux.configure_status_bar(tmux_name, &status_bar).await;

        let sid = session_id;
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&sid) {
                    session.set_status(SessionStatus::Running);
                }
            })
            .await?;

        info!(
            "Restarted session {} fresh (no --resume) via tmux name: {}",
            session_id, tmux_name
        );
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
pub fn program_is_claude(program: &str) -> bool {
    program.split_whitespace().next() == Some("claude")
}

/// Insert `--permission-mode <mode>` and/or `--effort <level>` into a Claude
/// command string. Always uses long-form flags (never short flags like `-p`)
/// because short flags on the Claude CLI can have different meanings.
///
/// `"default"` mode is treated as a no-op — the Claude CLI uses its own
/// default when the flag is absent. Effort has no equivalent no-op value
/// (its levels are `high`/`medium`/`low`), so all values are passed through.
///
/// No-op when the program isn't Claude.
pub fn program_with_claude_flags(
    program: &str,
    mode: Option<&str>,
    effort: Option<&str>,
) -> String {
    if !program_is_claude(program) || (mode.is_none() && effort.is_none()) {
        return program.to_string();
    }

    let mut parts = program.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap();
    let rest = parts.next();

    let mut flags = Vec::new();
    if let Some(m) = mode
        && m != "default"
    {
        flags.push(format!("--permission-mode {m}"));
    }
    if let Some(e) = effort {
        flags.push(format!("--effort {e}"));
    }

    match (flags.is_empty(), rest) {
        (true, Some(r)) => format!("{cmd} {r}"),
        (true, None) => cmd.to_string(),
        (false, Some(r)) => format!("{cmd} {} {r}", flags.join(" ")),
        (false, None) => format!("{cmd} {}", flags.join(" ")),
    }
}

/// Inject `-n <session_title>` into a Claude command so the Claude Code
/// session is named to match the Claude Commander session.
///
/// For non-claude programs the command is returned unchanged.
pub(super) fn program_with_session_name(program: &str, session_title: &str) -> String {
    if !program_is_claude(program) || session_title.is_empty() {
        return program.to_string();
    }
    let escaped = shell_escape_single_quote(session_title);
    let mut parts = program.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap();
    match parts.next() {
        Some(rest) => format!("{cmd} -n '{escaped}' {rest}"),
        None => format!("{cmd} -n '{escaped}'"),
    }
}

fn shell_escape_single_quote(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    // --- program_is_claude ---

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

    // --- program_with_claude_flags ---

    #[test]
    fn claude_flags_effort_only() {
        assert_eq!(
            program_with_claude_flags("claude", None, Some("high")),
            "claude --effort high"
        );
    }

    #[test]
    fn claude_flags_mode_only() {
        assert_eq!(
            program_with_claude_flags("claude", Some("auto"), None),
            "claude --permission-mode auto"
        );
    }

    #[test]
    fn claude_flags_both() {
        assert_eq!(
            program_with_claude_flags("claude", Some("plan"), Some("low")),
            "claude --permission-mode plan --effort low"
        );
    }

    #[test]
    fn claude_flags_default_mode_is_noop() {
        assert_eq!(
            program_with_claude_flags("claude", Some("default"), None),
            "claude"
        );
    }

    #[test]
    fn claude_flags_preserves_existing_args() {
        assert_eq!(
            program_with_claude_flags("claude --resume", Some("auto"), Some("high")),
            "claude --permission-mode auto --effort high --resume"
        );
    }

    #[test]
    fn claude_flags_noop_for_non_claude() {
        assert_eq!(
            program_with_claude_flags("bash", Some("auto"), Some("high")),
            "bash"
        );
    }

    #[test]
    fn claude_flags_noop_when_no_flags() {
        assert_eq!(
            program_with_claude_flags("claude --resume", None, None),
            "claude --resume"
        );
    }

    // --- program_with_session_name ---

    #[test]
    fn session_name_injected_for_bare_claude() {
        let cmd = program_with_session_name("claude", "my session");
        assert_eq!(cmd, "claude -n 'my session'");
    }

    #[test]
    fn session_name_injected_with_existing_args() {
        let cmd = program_with_session_name("claude --resume", "fix auth");
        assert_eq!(cmd, "claude -n 'fix auth' --resume");
    }

    #[test]
    fn session_name_skipped_for_non_claude() {
        let cmd = program_with_session_name("bash", "my session");
        assert_eq!(cmd, "bash");
    }

    #[test]
    fn session_name_skipped_for_empty_title() {
        let cmd = program_with_session_name("claude", "");
        assert_eq!(cmd, "claude");
    }

    #[test]
    fn session_name_escapes_single_quotes() {
        let cmd = program_with_session_name("claude", "it's a test");
        assert_eq!(cmd, "claude -n 'it'\\''s a test'");
    }

    #[test]
    fn session_name_with_prompt_arg() {
        // Simulates the shape produced when an initial prompt is appended
        let with_prompt = "claude 'Fix the auth bug'";
        let cmd = program_with_session_name(with_prompt, "my session");
        assert!(cmd.starts_with("claude -n 'my session' '"));
        assert!(cmd.contains("Fix the auth bug"));
    }
}
