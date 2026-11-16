//! Session lifecycle: create, restart, kill, and delete sessions.

use super::*;
use crate::agent::AgentKind;
use crate::git::git_command;

pub(crate) struct DeletePlan {
    provider: claude_commander_protocol::hosting::CodeHostProvider,
    repo_path: Option<PathBuf>,
    session: WorktreeSession,
    retargets: Vec<crate::config::PrBaseRetarget>,
}

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
        let program = program.unwrap_or_else(|| self.config_store.read().default_session_program());

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
    ///
    /// When `base_branch` is `Some`, the session's (freshly generated) branch
    /// is forked off that base branch rather than `origin/<main>`. This backs
    /// the CLI `--base-branch` flag. For stacked sessions the fork point is
    /// derived from the stack parent instead and takes precedence.
    pub async fn finalize_session(
        &self,
        session_id: &SessionId,
        initial_prompt: Option<String>,
        base_branch: Option<String>,
    ) -> Result<SessionId> {
        let provider = self.config_store.read().code_host_provider;
        self.finalize_session_for_provider(session_id, initial_prompt, base_branch, provider)
            .await
    }

    /// Finalize using the provider snapshotted by the service at the beginning
    /// of the wider create operation. Direct manager callers use
    /// [`Self::finalize_session`], which snapshots at their own operation edge.
    #[instrument(skip(self))]
    pub(crate) async fn finalize_session_for_provider(
        &self,
        session_id: &SessionId,
        initial_prompt: Option<String>,
        base_branch: Option<String>,
        provider: claude_commander_protocol::hosting::CodeHostProvider,
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

        // Kick off a background `nix develop` pre-warm for the project's dev
        // shell now, so it overlaps the slow steps below (git fetch + worktree
        // add). The new worktree checks out the same commit, so by the time its
        // pane runs `nix develop` the shared store is warm and the pane reuses
        // it instead of building from cold. No-op for non-flake projects.
        self.prewarm_nix_shell(&repo_path);

        // Fetch latest changes from origin
        if self.config_store.read().fetch_before_create {
            info!(
                "Fetching latest changes from origin in {}",
                repo_path.display()
            );
            let fetch_start = std::time::Instant::now();
            let output = git_command()
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
        let (branch_exists, branch_preexisted, start_point) = {
            let backend = GitBackend::open(&repo_path)?;
            let exists = backend.branch_exists(&branch_name)?;
            // Whether this session adopts a branch that may already carry
            // commits (Checkout Branch, locally or from `origin/<branch>`),
            // rather than a fresh branch we create empty. Drives whether
            // `base_commit` records the fork point or HEAD (see below).
            let preexisted =
                exists || backend.ref_exists(&format!("refs/remotes/origin/{}", branch_name))?;
            // Fork point for the new branch. A stacked parent's branch takes
            // precedence (it exists locally thanks to its own worktree);
            // otherwise honour the CLI `--base-branch` value. Both mean "create
            // the new branch off this base", overriding the origin/<main>
            // fallback below.
            let fork_base = stack_parent_branch.as_deref().or(base_branch.as_deref());
            let sp = if exists {
                // The branch already exists locally — `git worktree add` will
                // just check it out, so no explicit start point is needed.
                None
            } else if let Some(base) = fork_base {
                // Fork off the base branch, preferring the local branch and
                // falling back to its remote tracking ref, then origin/<main>.
                let base_remote_ref = format!("refs/remotes/origin/{}", base);
                let main_remote_ref = format!("refs/remotes/origin/{}", main_branch);
                if backend.branch_exists(base)? {
                    Some(base.to_string())
                } else if backend.ref_exists(&base_remote_ref)? {
                    Some(format!("origin/{}", base))
                } else if backend.ref_exists(&main_remote_ref)? {
                    Some(format!("origin/{}", main_branch))
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
                if backend.ref_exists(&branch_remote_ref)? {
                    Some(format!("origin/{}", branch_name))
                } else if backend.ref_exists(&main_remote_ref)? {
                    Some(format!("origin/{}", main_branch))
                } else {
                    None
                }
            };
            (exists, preexisted, sp)
        };
        let worktree_path = worktrees_dir.join(&worktree_name);
        let skip_lfs_smudge = self.config_store.read().skip_lfs_smudge;
        let worktree_create_start = std::time::Instant::now();
        let worktree_info = WorktreeManager::run_create_worktree(
            worktrees_dir,
            repo_path.clone(),
            worktree_path,
            branch_name.clone(),
            branch_exists,
            start_point,
            skip_lfs_smudge,
        )
        .await?;
        info!(
            "[timing] run_create_worktree (git worktree add + worktree includes) took {}ms",
            worktree_create_start.elapsed().as_millis()
        );

        // Everything from here on can still fail (tmux create, store writes).
        // Scope those steps together so a failure unregisters the worktree we
        // just created — otherwise the leaked registration makes every retry
        // with the same title fail with "'<branch>' is already used by
        // worktree at …".
        let finalized: Result<SessionId> = async {
            // Read tmux_session_name from the placeholder session
            let tmux_session_name = {
                let state = self.store.read().await;
                let session = state
                    .get_session(session_id)
                    .ok_or(SessionError::NotFound(*session_id))?;
                session.tmux_session_name.clone()
            };

            // Build a single positional prompt arg combining stack context (for
            // stacked sessions) and any user-provided initial prompt. Harnesses that
            // accept a positional prompt (Claude, Codex) take exactly one, so both
            // parts are merged; harnesses that don't (a bare shell) get neither.
            let accepts_prompt = AgentKind::from_program(&program).accepts_positional_prompt();
            let launch_cmd = {
                let mut prompt_parts: Vec<String> = Vec::new();
                if let Some(pb) = stack_parent_branch.as_deref()
                    && accepts_prompt
                {
                    prompt_parts.push(crate::git::hosting::stack_instruction(provider, pb));
                }
                if let Some(ref user_prompt) = initial_prompt
                    && accepts_prompt
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
            let launch_cmd = self.maybe_wrap_nix_develop(&launch_cmd, &worktree_info.path);

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
            // A fresh branch is created empty off its base, so HEAD *is* the fork
            // point. A checked-out branch sits on its tip, so record its genuine
            // fork point instead — else `merge-base(base, HEAD)` is HEAD and the
            // review diff comes up empty for a branch that is ahead of its target.
            let base_commit = crate::git::managed_base_commit(
                &wt_path,
                &head,
                Some(&main_branch),
                branch_preexisted,
            )
            .await;
            // The branch this session forked from, recorded so the review diff can
            // resolve its base against that branch's *live* tip rather than the
            // frozen `base_commit`: a stack parent's branch, an explicit
            // `--base-branch`, or the project's main branch.
            let base_branch = stack_parent_branch
                .as_deref()
                .or(base_branch.as_deref())
                .unwrap_or(&main_branch)
                .to_string();
            self.store
                .mutate(move |state| {
                    if let Some(session) = state.get_session_mut(&sid) {
                        session.worktree_path = wt_path;
                        session.base_commit = Some(base_commit);
                        session.base_branch = Some(base_branch);
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
        .await;

        if finalized.is_err()
            && let Ok(backend) = GitBackend::open(&repo_path)
        {
            let worktree_manager =
                WorktreeManager::new(backend, self.config_store.read().worktrees_dir()?);
            if let Err(e) = worktree_manager
                .remove_worktree(&worktree_info.path, true)
                .await
            {
                warn!("Failed to remove worktree after failed finalize: {}", e);
            }
        }
        finalized
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
        self.diff_cache.invalidate(session_id).await;
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
        if self
            .store
            .read()
            .await
            .get_session(session_id)
            .is_some_and(|s| s.pending_program.is_some())
        {
            return self.restart_session_fresh(session_id).await;
        }

        let (
            tmux_session_name,
            shell_tmux_name,
            worktree_path,
            title,
            program,
            hibernated,
            status_bar,
        ) = {
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
                session.hibernated,
                self.status_bar_info(session, &state),
            )
        };

        // Bump last_active_at *before* the destructive kill. A hibernation pass
        // that snapshotted this session earlier compares the stamp at its
        // pre-kill recheck ([`still_hibernatable`]); bumping it now means an
        // in-flight restart — pane killed, not yet recreated — presents a
        // changed stamp, so the racing hibernate bails instead of killing the
        // pane we are about to rebuild. (set_status(Running) below re-stamps it,
        // but that lands only after recreation.)
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&sid) {
                    session.touch();
                }
            })
            .await?;

        self.kill_tmux_sessions(&tmux_session_name, shell_tmux_name.as_deref())
            .await;

        // Create a fresh tmux session, resuming the prior agent session if
        // configured, or unconditionally when this session was auto-hibernated
        // (resume is what makes hibernation non-destructive).
        let force_resume = self.config_store.read().resume_session || hibernated;
        let resume_program = resume_program_for(&program, force_resume);
        let resume_program = program_with_session_name(&resume_program, &title);
        let resume_program = self.maybe_wrap_nix_develop(&resume_program, &worktree_path);
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

        // Set status to Running and clear the hibernation marker — the pane has
        // been recreated (resumed above when it was hibernated).
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&sid) {
                    session.set_status(SessionStatus::Running);
                    session.hibernated = false;
                }
            })
            .await?;

        info!("Restarted session {}", session_id);
        Ok(())
    }

    /// Explicitly restart without resume, applying any pending program change.
    pub async fn restart_session_fresh(&self, session_id: &SessionId) -> Result<()> {
        let tmux_name = {
            let state = self.store.read().await;
            state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?
                .tmux_session_name
                .clone()
        };
        self.recreate_session_fresh(&tmux_name).await
    }

    async fn recreate_session_fresh(&self, tmux_name: &str) -> Result<()> {
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
                session
                    .pending_program
                    .as_ref()
                    .unwrap_or(&session.program)
                    .clone(),
                self.status_bar_info(session, &state),
            )
        };

        // Bump last_active_at *before* the destructive kill, mirroring
        // [`Self::restart_session`]. A hibernation pass that snapshotted this
        // session earlier compares the stamp at its pre-kill recheck
        // ([`still_hibernatable`]); bumping it now means an in-flight
        // fresh-restart — pane killed, not yet recreated — presents a changed
        // stamp, so the racing hibernate bails instead of killing the pane we
        // are about to rebuild. Matters for user-initiated relaunches of
        // long-idle sessions (e.g. `change_program`), not just the attach-loop
        // caller whose session was active moments ago.
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&session_id) {
                    session.touch();
                }
            })
            .await?;

        let _ = self.tmux.kill_session(tmux_name).await;

        let launch_cmd = program_with_session_name(&program, &title);
        let launch_cmd = self.maybe_wrap_nix_develop(&launch_cmd, &worktree_path);
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
                    session.program = program.clone();
                    if session.pending_program.as_deref() == Some(program.as_str()) {
                        session.pending_program = None;
                    }
                    session.set_status(SessionStatus::Running);
                    // The pane is live again, so clear the hibernation marker to
                    // uphold the "live pane ⇒ not hibernated" invariant (matches
                    // restart_session and the attach/recreate wake path).
                    session.hibernated = false;
                }
            })
            .await?;

        info!(
            "Restarted session {} fresh (no --resume) via tmux name: {}",
            session_id, tmux_name
        );
        Ok(())
    }

    /// Kill a session (stop tmux, optionally remove worktree).
    ///
    /// When the worktree is kept the kill is non-destructive, so the session
    /// is marked `hibernated` — the next wake then resumes the prior agent
    /// conversation even when the global `resume_session` config is off,
    /// exactly like an auto-hibernated session.
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
            self.remove_session_worktree(repo_path.as_deref(), &session.worktree_path)
                .await?;
        }

        // Update state. A kill that keeps the worktree is non-destructive, so
        // flag it for resume-on-wake; a destructive kill leaves nothing to
        // resume into.
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&sid) {
                    session.set_status(SessionStatus::Stopped);
                    session.hibernated = !remove_worktree;
                }
            })
            .await?;

        info!("Killed session {}", session_id);
        Ok(())
    }

    /// Hibernate a session: stop its tmux process to free memory while keeping
    /// the worktree, branch, and all metadata intact. Unlike
    /// [`kill_session`](Self::kill_session) — which shares the non-destructive
    /// stop + `hibernated` marker when the worktree is kept — this is a
    /// *policy* action driven by the idle-hibernation loop, so it guards
    /// against racing attaches and restarts. The wake path then resumes the
    /// agent conversation even when the global `resume_session` config is off.
    ///
    /// Guards against racing a concurrent manual restart or a late attach:
    ///  - `last_active_at` is snapshotted at the top of this function; the
    ///    re-check immediately before the destructive kill compares it via
    ///    [`still_hibernatable`]. `restart_session` bumps `last_active_at`
    ///    *before* its own kill (and again when it flips the session back to
    ///    `Running`), so an in-flight or completed restart presents a changed
    ///    stamp: "a live pane was just recreated — don't kill it";
    ///  - a client attached just before the kill is detected via
    ///    `is_attached_including_shell` (spanning the paired shell, and erring
    ///    toward "attached" on any glitch);
    ///  - as a backstop for the reverse interleaving, if the tmux session
    ///    reappears after the kill the status update is skipped, and the final
    ///    mutate only transitions a still-`Running` session.
    /// Returns `true` if the session was actually hibernated, `false` if a guard
    /// skipped it (attached, no longer a candidate, or tmux reappeared) — so the
    /// caller can record telemetry only for real hibernations.
    #[instrument(skip(self))]
    pub async fn hibernate_session(&self, session_id: &SessionId) -> Result<bool> {
        let (tmux_session_name, shell_tmux_name, last_active_at) = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            (
                session.tmux_session_name.clone(),
                session.shell_tmux_session_name.clone(),
                session.last_active_at,
            )
        };

        // A client attached since the decision was made? Leave it running.
        // Spans the paired shell (Ctrl-\ toggles to it) since the kill below
        // destroys both. Conservative: a failed probe counts as attached.
        if self
            .is_attached_including_shell(&tmux_session_name, shell_tmux_name.as_deref())
            .await
        {
            info!(
                "Session {} attached before hibernate kill; skipping",
                session_id
            );
            return Ok(false);
        }

        // Final re-check under the lock, as close to the kill as possible: a
        // manual restart that completed in the meantime bumped last_active_at
        // (and would have recreated a live pane), so bail rather than clobber it.
        {
            let state = self.store.read().await;
            let still = state.get_session(session_id).is_some_and(|s| {
                still_hibernatable(s.status, s.keep_alive, s.last_active_at, last_active_at)
            });
            if !still {
                info!(
                    "Session {} no longer a hibernate candidate at kill time; skipping",
                    session_id
                );
                return Ok(false);
            }
        }

        self.kill_tmux_sessions(&tmux_session_name, shell_tmux_name.as_deref())
            .await;

        // If a concurrent restart recreated the tmux session between our re-check
        // and the kill, don't mark it Stopped — that would leave a live pane
        // flagged hibernated.
        if self
            .tmux
            .session_exists(&tmux_session_name)
            .await
            .unwrap_or(false)
        {
            warn!(
                "Session {} tmux reappeared after hibernate kill; skipping status update",
                session_id
            );
            return Ok(false);
        }

        let sid = *session_id;
        let hibernated = self
            .store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&sid)
                    && session.status == SessionStatus::Running
                {
                    session.set_status(SessionStatus::Stopped);
                    session.hibernated = true;
                    true
                } else {
                    false
                }
            })
            .await?;

        if hibernated {
            info!("Hibernated session {}", session_id);
        }
        Ok(hibernated)
    }

    /// Set a session's keep-alive flag (opt-out of auto-hibernation). Returns
    /// the value that was set, or [`SessionError::NotFound`] if the session no
    /// longer exists — so callers don't report success for a no-op (matches
    /// [`toggle_keep_alive`](Self::toggle_keep_alive)).
    pub async fn set_keep_alive(&self, session_id: &SessionId, keep_alive: bool) -> Result<bool> {
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                state.get_session_mut(&sid).map(|session| {
                    session.keep_alive = keep_alive;
                    session.keep_alive
                })
            })
            .await?
            .ok_or_else(|| SessionError::NotFound(sid).into())
    }

    /// Toggle a session's keep-alive flag, returning the new value. The flip is
    /// done inside a single mutate so concurrent toggles can't race.
    pub async fn toggle_keep_alive(&self, session_id: &SessionId) -> Result<bool> {
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                state.get_session_mut(&sid).map(|session| {
                    session.keep_alive = !session.keep_alive;
                    session.keep_alive
                })
            })
            .await?
            .ok_or_else(|| SessionError::NotFound(sid).into())
    }

    /// Remove a session's git worktree, best-effort. `repo_path` is the owning
    /// project's repository path (where the worktree list lives); `None` — or a
    /// path that isn't a git repo — skips removal. A removal failure is logged
    /// and swallowed: the caller has already dropped (or is about to drop) the
    /// session, and an orphaned worktree is reconciled at next startup. Returns
    /// `Err` only if the configured worktrees dir can't be resolved.
    pub(super) async fn remove_session_worktree(
        &self,
        repo_path: Option<&Path>,
        worktree_path: &Path,
    ) -> Result<()> {
        if let Some(repo_path) = repo_path
            && let Ok(backend) = GitBackend::open(repo_path)
        {
            let worktree_manager =
                WorktreeManager::new(backend, self.config_store.read().worktrees_dir()?);
            if let Err(e) = worktree_manager.remove_worktree(worktree_path, true).await {
                warn!("Failed to remove worktree: {}", e);
            }
        }
        Ok(())
    }

    /// Delete a session (remove from state)
    #[instrument(skip(self))]
    pub async fn delete_session(&self, session_id: &SessionId) -> Result<()> {
        let plan = self.prepare_delete_session(session_id).await?;
        self.finish_delete_session(plan).await;
        Ok(())
    }

    pub(crate) async fn prepare_delete_session(
        &self,
        session_id: &SessionId,
    ) -> Result<DeletePlan> {
        let provider = self.config_store.read().code_host_provider;
        // Resolve the owning project's repo path up front (needed to open the
        // git backend for worktree removal) — a cheap store read while the
        // session still exists.
        let repo_path = {
            let state = self.store.read().await;
            let session = state
                .get_session(session_id)
                .ok_or(SessionError::NotFound(*session_id))?;
            state
                .get_project(&session.project_id)
                .map(|p| p.repo_path.clone())
        };

        // Remove from state FIRST so the tree updates immediately: this single
        // mutate bumps the change feed, and the row disappears without waiting on
        // the slow tmux-kill + `git worktree remove` below (which the kill-first
        // ordering used to block the row's removal on). Stacked children are
        // re-pointed onto the parent and the durable PR-base edits are planned
        // atomically inside the same mutate — so no concurrent task can
        // invalidate the plan between read and remove — and it hands back the
        // removed session so the teardown still has its tmux names and worktree
        // path.
        let sid = *session_id;
        let (removed, pr_retargets) = self
            .store
            .mutate(move |state| state.remove_session_retargeting_children(&sid))
            .await?;
        let Some(session) = removed else {
            // A concurrent delete won the race and already removed it.
            return Err(SessionError::NotFound(*session_id).into());
        };

        Ok(DeletePlan {
            provider,
            repo_path,
            session,
            retargets: pr_retargets,
        })
    }

    pub(crate) async fn finish_delete_session(&self, plan: DeletePlan) {
        let DeletePlan {
            provider,
            repo_path,
            session,
            retargets: pr_retargets,
        } = plan;
        // Tear down the (already-removed) session's resources. Unconditional: a
        // Stopped session has no live tmux session but its worktree is still on
        // disk and must be cleaned up. A failure here can't restore the row and
        // only orphans a resource that startup reconciliation cleans up, so it's
        // logged rather than propagated.
        self.kill_tmux_sessions(
            &session.tmux_session_name,
            session.shell_tmux_session_name.as_deref(),
        )
        .await;
        // Swallow (don't `?`) any teardown error: the session is already gone, so
        // propagating would report a failed delete and skip the child-PR retarget
        // below, leaving children retargeted locally but not on GitHub.
        if let Err(e) = self
            .remove_session_worktree(repo_path.as_deref(), &session.worktree_path)
            .await
        {
            warn!("Failed to remove worktree while deleting session: {}", e);
        }
        // Release the worktree's filesystem watch along with its cached diff
        // only now: a get_diff racing the teardown could otherwise re-register
        // a watch on the still-present worktree. Unconditional even when the
        // removal failed — the session is gone, so nothing would release it.
        self.diff_cache.invalidate(&session.id).await;

        // Durably retarget child reviews on the selected host (best-effort,
        // non-fatal).
        Self::retarget_child_prs(provider, pr_retargets).await;

        info!("Deleted session {}", session.id);
    }

    /// Run planned review-base edits for a stack deletion. Best-effort: each
    /// CLI failure is logged and skipped — the local metadata
    /// retarget already keeps the UI correct. Shared by the CLI and TUI delete
    /// paths.
    pub async fn retarget_child_prs(
        provider: claude_commander_protocol::hosting::CodeHostProvider,
        retargets: Vec<crate::config::PrBaseRetarget>,
    ) {
        for r in retargets {
            if let Err(message) = crate::git::hosting::try_retarget_review_base(
                provider,
                &r.repo_path,
                r.pr_number,
                &r.new_base_branch,
            )
            .await
            {
                warn!(
                    "Failed to retarget review #{} to '{}': {}",
                    r.pr_number, r.new_base_branch, message
                );
            }
        }
    }
}

/// Insert agent launch flags into a command string: `--permission-mode` and
/// `--effort` for Claude only, `--model <name>` for any harness that
/// understands it (Claude, Codex). Always uses long-form flags (never short
/// flags like `-p`) because short flags can have different meanings across
/// harnesses.
///
/// `"default"` mode is treated as a no-op — the Claude CLI uses its own
/// default when the flag is absent. Effort has no equivalent no-op value
/// (its levels are `high`/`medium`/`low`), so all values are passed through.
pub fn program_with_agent_flags(
    program: &str,
    mode: Option<&str>,
    effort: Option<&str>,
    model: Option<&str>,
) -> String {
    let kind = AgentKind::from_program(program);

    let mut flags = Vec::new();
    if kind.is_claude() {
        if let Some(m) = mode
            && m != "default"
        {
            flags.push(format!("--permission-mode {m}"));
        }
        if let Some(e) = effort {
            flags.push(format!("--effort {e}"));
        }
    }
    if kind.supports_model_flag()
        && let Some(m) = model
    {
        flags.push(format!("--model {m}"));
    }

    if flags.is_empty() {
        return program.to_string();
    }

    let mut parts = program.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap();
    match parts.next() {
        Some(r) => format!("{cmd} {} {r}", flags.join(" ")),
        None => format!("{cmd} {}", flags.join(" ")),
    }
}

/// Inject `-n <session_title>` into a Claude command so the Claude Code
/// session is named to match the Claude Commander session.
///
/// For non-claude programs the command is returned unchanged.
pub(super) fn program_with_session_name(program: &str, session_title: &str) -> String {
    if !AgentKind::from_program(program).is_claude() || session_title.is_empty() {
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

pub(super) fn shell_escape_single_quote(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Choose the launch command when recreating a session's tmux pane: the
/// harness's resume command when `force_resume` is set, otherwise the program
/// launched fresh. Resume syntax is harness-specific; an unrecognised program
/// has no resume mechanism, so it launches fresh regardless.
///
/// Callers combine two inputs into `force_resume`: the global `resume_session`
/// config and the per-session `hibernated` marker (an auto-hibernated session
/// must resume to be non-destructive, even when the global flag is off).
pub(super) fn resume_program_for(program: &str, force_resume: bool) -> String {
    if force_resume {
        AgentKind::from_program(program)
            .resume_command(program)
            .unwrap_or_else(|| program.to_string())
    } else {
        program.to_string()
    }
}

/// Whether a session is still a valid hibernate target at the pre-kill re-check.
///
/// `snapshot_last_active` is `last_active_at` captured at the start of
/// [`SessionManager::hibernate_session`]. A session is still hibernatable only if it is `Running`, not
/// keep-alive, and its `last_active_at` is unchanged — any advance means a
/// concurrent restart/wake flipped it back to `Running` (`set_status` bumps the
/// stamp) and recreated a live pane that must not be killed. Pure, so the
/// clobber-guard logic is unit-tested without tmux.
pub(super) fn still_hibernatable(
    status: SessionStatus,
    keep_alive: bool,
    last_active_at: chrono::DateTime<chrono::Utc>,
    snapshot_last_active: chrono::DateTime<chrono::Utc>,
) -> bool {
    status == SessionStatus::Running && !keep_alive && last_active_at == snapshot_last_active
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    // --- still_hibernatable (pre-kill clobber/attach guard) ---

    #[test]
    fn still_hibernatable_when_running_and_unchanged() {
        let t = chrono::Utc::now();
        assert!(still_hibernatable(SessionStatus::Running, false, t, t));
    }

    #[test]
    fn not_hibernatable_when_last_active_advanced() {
        // A concurrent restart/wake bumped last_active_at — its live pane must
        // not be killed even though the status is (again) Running.
        let snapshot = chrono::Utc::now();
        let after_restart = snapshot + chrono::Duration::seconds(1);
        assert!(!still_hibernatable(
            SessionStatus::Running,
            false,
            after_restart,
            snapshot
        ));
    }

    #[test]
    fn not_hibernatable_when_keep_alive_or_not_running() {
        let t = chrono::Utc::now();
        assert!(!still_hibernatable(SessionStatus::Running, true, t, t));
        assert!(!still_hibernatable(SessionStatus::Stopped, false, t, t));
    }

    // --- program_with_agent_flags ---

    #[test]
    fn claude_flags_effort_only() {
        assert_eq!(
            program_with_agent_flags("claude", None, Some("high"), None),
            "claude --effort high"
        );
    }

    #[test]
    fn claude_flags_mode_only() {
        assert_eq!(
            program_with_agent_flags("claude", Some("auto"), None, None),
            "claude --permission-mode auto"
        );
    }

    #[test]
    fn claude_flags_both() {
        assert_eq!(
            program_with_agent_flags("claude", Some("plan"), Some("low"), None),
            "claude --permission-mode plan --effort low"
        );
    }

    #[test]
    fn claude_flags_default_mode_is_noop() {
        assert_eq!(
            program_with_agent_flags("claude", Some("default"), None, None),
            "claude"
        );
    }

    #[test]
    fn claude_flags_preserves_existing_args() {
        assert_eq!(
            program_with_agent_flags("claude --resume", Some("auto"), Some("high"), None),
            "claude --permission-mode auto --effort high --resume"
        );
    }

    #[test]
    fn claude_flags_noop_for_non_claude() {
        assert_eq!(
            program_with_agent_flags("bash", Some("auto"), Some("high"), None),
            "bash"
        );
        // Codex has its own flag conventions — never inject Claude's flags.
        assert_eq!(
            program_with_agent_flags("codex", Some("auto"), Some("high"), None),
            "codex"
        );
    }

    #[test]
    fn claude_flags_noop_when_no_flags() {
        assert_eq!(
            program_with_agent_flags("claude --resume", None, None, None),
            "claude --resume"
        );
    }

    #[test]
    fn model_flag_injected_for_claude() {
        assert_eq!(
            program_with_agent_flags("claude", None, None, Some("opus")),
            "claude --model opus"
        );
    }

    #[test]
    fn model_flag_injected_for_codex() {
        assert_eq!(
            program_with_agent_flags("codex", None, None, Some("gpt-5")),
            "codex --model gpt-5"
        );
    }

    #[test]
    fn model_flag_combines_with_claude_only_flags() {
        assert_eq!(
            program_with_agent_flags("claude", Some("plan"), Some("high"), Some("opus")),
            "claude --permission-mode plan --effort high --model opus"
        );
    }

    #[test]
    fn model_flag_noop_for_unknown_program() {
        assert_eq!(
            program_with_agent_flags("bash", None, None, Some("opus")),
            "bash"
        );
    }

    #[test]
    fn model_flag_injected_for_opencode() {
        assert_eq!(
            program_with_agent_flags("opencode", None, None, Some("anthropic/claude-sonnet-4-5")),
            "opencode --model anthropic/claude-sonnet-4-5"
        );
        // Claude-only flags are ignored for OpenCode.
        assert_eq!(
            program_with_agent_flags(
                "opencode",
                Some("auto"),
                Some("high"),
                Some("anthropic/claude-sonnet-4-5")
            ),
            "opencode --model anthropic/claude-sonnet-4-5"
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
        // Codex has no `-n` session-name flag — leave its command untouched.
        let codex = program_with_session_name("codex", "my session");
        assert_eq!(codex, "codex");
        // OpenCode has no `-n` session-name flag either.
        let opencode = program_with_session_name("opencode", "my session");
        assert_eq!(opencode, "opencode");
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

    // --- resume_program_for ---

    #[test]
    fn resume_program_for_forces_resume_per_harness() {
        assert_eq!(resume_program_for("claude", true), "claude --resume");
        assert_eq!(resume_program_for("codex", true), "codex resume --last");
        assert_eq!(resume_program_for("opencode", true), "opencode --continue");
        assert_eq!(
            resume_program_for("opencode --auto", true),
            "opencode --auto --continue"
        );

        assert_eq!(resume_program_for("claude -c", true), "claude -c --resume");
    }

    #[test]
    fn resume_program_for_unknown_harness_launches_fresh_even_when_forced() {
        // A bare shell has no resume mechanism, so forcing resume can't change it.
        assert_eq!(resume_program_for("bash", true), "bash");
    }

    #[test]
    fn resume_program_for_without_force_launches_fresh() {
        assert_eq!(resume_program_for("claude", false), "claude");
        assert_eq!(resume_program_for("codex", false), "codex");
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
