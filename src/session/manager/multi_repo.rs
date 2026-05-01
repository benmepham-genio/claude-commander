//! Multi-repo session lifecycle: create, finalize, delete.

use std::path::PathBuf;

use tracing::{info, instrument, warn};

use super::*;
use crate::session::{MultiRepoEntry, MultiRepoSession, MultiRepoSessionId};

impl SessionManager {
    /// Prepare a placeholder multi-repo session in `Creating` state.
    ///
    /// Inserts into state immediately so the UI can show a spinner.
    /// Call `finalize_multi_repo_session` in a background task to do the
    /// heavy git/tmux work.
    #[instrument(skip(self))]
    pub async fn prepare_multi_repo_session(
        &self,
        project_ids: Vec<ProjectId>,
        title: String,
        program: Option<String>,
    ) -> Result<MultiRepoSessionId> {
        let program = program.unwrap_or_else(|| self.config_store.read().default_program.clone());
        let branch_name = self.generate_branch_name(&title);

        // Validate all projects exist
        {
            let state = self.store.read().await;
            for pid in &project_ids {
                state
                    .get_project(pid)
                    .ok_or_else(|| SessionError::ProjectNotFound(pid.to_string()))?;
            }
        }

        let mut session = MultiRepoSession::new_creating(title, branch_name, program);
        // Pre-populate repos with project IDs (paths filled in during finalize)
        for pid in &project_ids {
            session.repos.push(MultiRepoEntry {
                project_id: *pid,
                worktree_path: PathBuf::new(),
                base_commit: None,
            });
        }
        let session_id = session.id;

        self.store
            .mutate(move |state| {
                state.add_multi_repo_session(session);
            })
            .await?;

        info!("Prepared multi-repo session {}", session_id);
        Ok(session_id)
    }

    /// Finalize a multi-repo session: create worktrees, generate CLAUDE.md,
    /// and launch the tmux session.
    #[instrument(skip(self))]
    pub async fn finalize_multi_repo_session(
        &self,
        session_id: &MultiRepoSessionId,
    ) -> Result<MultiRepoSessionId> {
        // Read session metadata
        let (title, branch_name, program, repo_entries) = {
            let state = self.store.read().await;
            let session = state
                .get_multi_repo_session(session_id)
                .ok_or_else(|| SessionError::CreationFailed("Multi-repo session not found".into()))?;
            (
                session.title.clone(),
                session.branch.clone(),
                session.program.clone(),
                session.repos.clone(),
            )
        };

        // Collect project info for each repo
        let project_infos: Vec<(ProjectId, PathBuf, String, String)> = {
            let state = self.store.read().await;
            repo_entries
                .iter()
                .filter_map(|entry| {
                    state.get_project(&entry.project_id).map(|p| {
                        (
                            p.id,
                            p.repo_path.clone(),
                            p.main_branch.clone(),
                            p.name.clone(),
                        )
                    })
                })
                .collect()
        };

        info!(
            "Finalizing multi-repo session '{}' with branch '{}' across {} repos",
            title,
            branch_name,
            project_infos.len()
        );

        // Create shared parent directory
        let worktrees_dir = self.config_store.read().worktrees_dir()?;
        let parent_name = format!(
            "{}-{}",
            self.sanitize_name(&title),
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("")
        );
        let parent_dir = worktrees_dir.join(&parent_name);
        tokio::fs::create_dir_all(&parent_dir).await.map_err(|e| {
            SessionError::CreationFailed(format!("Failed to create parent dir: {}", e))
        })?;

        // Fetch and create worktree for each repo
        let mut updated_entries: Vec<MultiRepoEntry> = Vec::new();

        for (project_id, repo_path, main_branch, project_name) in &project_infos {
            // Optionally fetch
            if self.config_store.read().fetch_before_create {
                info!("Fetching origin in {}", repo_path.display());
                let output = tokio::process::Command::new("git")
                    .current_dir(repo_path)
                    .args(["fetch", "origin"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .await?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!("git fetch failed for {} (continuing): {}", project_name, stderr);
                }
            }

            // Check branch existence and determine start point
            let (branch_exists, start_point) = {
                let backend = GitBackend::open(repo_path)?;
                let exists = backend.branch_exists(&branch_name)?;
                let branch_remote_ref = format!("refs/remotes/origin/{}", branch_name);
                let main_remote_ref = format!("refs/remotes/origin/{}", main_branch);
                let sp = if !exists && backend.ref_exists(&branch_remote_ref)? {
                    Some(format!("origin/{}", branch_name))
                } else if backend.ref_exists(&main_remote_ref)? {
                    Some(format!("origin/{}", main_branch))
                } else {
                    None
                };
                (exists, sp)
            };

            // Create worktree inside the parent directory, named after the project
            let worktree_path = parent_dir.join(project_name);
            let worktree_info = WorktreeManager::run_create_worktree(
                parent_dir.clone(),
                repo_path.clone(),
                worktree_path.clone(),
                branch_name.clone(),
                branch_exists,
                start_point,
            )
            .await?;

            updated_entries.push(MultiRepoEntry {
                project_id: *project_id,
                worktree_path,
                base_commit: Some(worktree_info.head),
            });
        }

        // Generate CLAUDE.md in parent directory
        let claude_md = self.generate_multi_repo_claude_md(&title, &branch_name, &project_infos);
        let claude_md_path = parent_dir.join("CLAUDE.md");
        tokio::fs::write(&claude_md_path, claude_md)
            .await
            .map_err(|e| {
                SessionError::CreationFailed(format!("Failed to write CLAUDE.md: {}", e))
            })?;

        // Read tmux session name from placeholder
        let tmux_session_name = {
            let state = self.store.read().await;
            let session = state.get_multi_repo_session(session_id).ok_or_else(|| {
                SessionError::CreationFailed("Multi-repo session disappeared".into())
            })?;
            session.tmux_session_name.clone()
        };

        // Create tmux session in the parent directory
        self.tmux
            .create_session(&tmux_session_name, &parent_dir, Some(&program))
            .await?;

        // Update session to Running with the real info
        let sid = *session_id;
        let pd = parent_dir.clone();
        self.store
            .mutate(move |state| {
                if let Some(session) = state.get_multi_repo_session_mut(&sid) {
                    session.parent_dir = pd;
                    session.repos = updated_entries;
                    session.set_status(SessionStatus::Running);
                }
            })
            .await?;

        info!(
            "Finalized multi-repo session {} with tmux session {}",
            session_id, tmux_session_name
        );
        Ok(*session_id)
    }

    /// Remove a multi-repo session that is still in `Creating` state.
    #[instrument(skip(self))]
    pub async fn remove_creating_multi_repo_session(
        &self,
        session_id: &MultiRepoSessionId,
    ) -> Result<()> {
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                state.remove_multi_repo_session(&sid);
            })
            .await?;
        info!("Removed creating multi-repo session {}", session_id);
        Ok(())
    }

    /// Delete a multi-repo session: kill tmux, remove worktrees, clean up parent dir.
    #[instrument(skip(self))]
    pub async fn delete_multi_repo_session(
        &self,
        session_id: &MultiRepoSessionId,
    ) -> Result<()> {
        let session = {
            let state = self.store.read().await;
            state
                .get_multi_repo_session(session_id)
                .ok_or_else(|| {
                    SessionError::CreationFailed("Multi-repo session not found".into())
                })?
                .clone()
        };

        // Kill tmux session
        if let Err(e) = self.tmux.kill_session(&session.tmux_session_name).await {
            warn!("Failed to kill tmux session: {}", e);
        }
        // Kill the lazily-created shell tmux session if one exists
        if let Some(ref shell_name) = session.shell_tmux_session_name {
            let _ = self.tmux.kill_session(shell_name).await;
        }

        // Remove each worktree via git CLI (avoids holding non-Send gix types across await)
        for entry in &session.repos {
            if entry.worktree_path.as_os_str().is_empty() {
                continue;
            }
            let repo_path = {
                let state = self.store.read().await;
                state
                    .get_project(&entry.project_id)
                    .map(|p| p.repo_path.clone())
            };
            if let Some(repo_path) = repo_path {
                let output = tokio::process::Command::new("git")
                    .current_dir(&repo_path)
                    .args(["worktree", "remove", "--force"])
                    .arg(&entry.worktree_path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .await;
                match output {
                    Ok(o) if !o.status.success() => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        warn!(
                            "git worktree remove failed for {}: {}",
                            entry.worktree_path.display(),
                            stderr
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Failed to run git worktree remove for {}: {}",
                            entry.worktree_path.display(),
                            e
                        );
                    }
                    _ => {}
                }
            }
        }

        // Remove parent directory (CLAUDE.md + any remaining files)
        if session.parent_dir.exists()
            && let Err(e) = tokio::fs::remove_dir_all(&session.parent_dir).await
        {
            warn!(
                "Failed to remove parent dir {}: {}",
                session.parent_dir.display(),
                e
            );
        }

        // Remove from state
        let sid = *session_id;
        self.store
            .mutate(move |state| {
                state.remove_multi_repo_session(&sid);
            })
            .await?;

        info!("Deleted multi-repo session {}", session_id);
        Ok(())
    }

    /// Generate CLAUDE.md content for a multi-repo session.
    fn generate_multi_repo_claude_md(
        &self,
        title: &str,
        branch: &str,
        projects: &[(ProjectId, PathBuf, String, String)],
    ) -> String {
        let mut md = format!(
            "# Multi-Repo Session: {}\n\n\
             This directory contains worktrees from multiple repositories.\n\
             Each subdirectory is a separate git repo with its own CLAUDE.md.\n\n\
             ## Repositories\n\n",
            title
        );
        for (_id, repo_path, _main_branch, name) in projects {
            md.push_str(&format!(
                "- `./{}/` — {}\n",
                name,
                repo_path.display()
            ));
        }
        md.push_str(&format!(
            "\n## Notes\n\n\
             - Each repo has its own git history, branches, and CLAUDE.md\n\
             - Create commits and PRs in each repo separately\n\
             - Branch name: `{}`\n",
            branch
        ));
        md
    }

    /// Ensure a shell tmux session exists for this multi-repo session, creating
    /// one rooted at `parent_dir` on first use. Returns the tmux session name.
    pub async fn ensure_multi_repo_shell_session(
        &self,
        session_id: &MultiRepoSessionId,
    ) -> Result<String> {
        let (existing_shell_name, parent_dir, id_prefix) = {
            let state = self.store.read().await;
            let session = state.get_multi_repo_session(session_id).ok_or_else(|| {
                SessionError::CreationFailed("Multi-repo session not found".into())
            })?;
            (
                session.shell_tmux_session_name.clone(),
                session.parent_dir.clone(),
                session.id.to_string(),
            )
        };

        if let Some(ref shell_name) = existing_shell_name
            && self.tmux.session_exists(shell_name).await.unwrap_or(false)
        {
            return Ok(shell_name.clone());
        }

        let shell_name = format!("cc-mr-{}-sh", id_prefix);

        if self.tmux.session_exists(&shell_name).await.unwrap_or(false) {
            let pane_dead = self.tmux.is_pane_dead(&shell_name).await.unwrap_or(false);
            if pane_dead {
                let _ = self.tmux.kill_session(&shell_name).await;
            } else {
                let sid = *session_id;
                let name = shell_name.clone();
                self.store
                    .mutate(move |state| {
                        if let Some(s) = state.get_multi_repo_session_mut(&sid) {
                            s.shell_tmux_session_name = Some(name);
                        }
                    })
                    .await?;
                return Ok(shell_name);
            }
        }

        let shell_program = self.config_store.read().shell_program.clone();
        self.tmux
            .create_session(&shell_name, &parent_dir, Some(&shell_program))
            .await?;

        let sid = *session_id;
        let name = shell_name.clone();
        self.store
            .mutate(move |state| {
                if let Some(s) = state.get_multi_repo_session_mut(&sid) {
                    s.shell_tmux_session_name = Some(name);
                }
            })
            .await?;

        info!(
            "Created shell session {} for multi-repo session {}",
            shell_name, session_id
        );
        Ok(shell_name)
    }

    /// Get attach command for the multi-repo shell session (creates it if needed).
    pub async fn get_multi_repo_shell_attach_command(
        &self,
        session_id: &MultiRepoSessionId,
    ) -> Result<String> {
        let shell_name = self.ensure_multi_repo_shell_session(session_id).await?;

        let exists = self.tmux.session_exists(&shell_name).await?;
        if !exists {
            let sid = *session_id;
            let _ = self
                .store
                .mutate(move |state| {
                    if let Some(s) = state.get_multi_repo_session_mut(&sid) {
                        s.shell_tmux_session_name = None;
                    }
                })
                .await;
            return Err(SessionError::TmuxSessionNotFound(shell_name).into());
        }

        let pane_dead = self.tmux.is_pane_dead(&shell_name).await.unwrap_or(false);
        if pane_dead {
            let _ = self.tmux.kill_session(&shell_name).await;
            let sid = *session_id;
            let _ = self
                .store
                .mutate(move |state| {
                    if let Some(s) = state.get_multi_repo_session_mut(&sid) {
                        s.shell_tmux_session_name = None;
                    }
                })
                .await;
            return Err(SessionError::TmuxSessionNotFound(format!(
                "{} (shell exited)",
                shell_name
            ))
            .into());
        }

        Ok(format!("tmux attach-session -t {}", shell_name))
    }
}
