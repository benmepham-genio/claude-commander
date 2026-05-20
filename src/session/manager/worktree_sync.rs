//! Worktree synchronization: import unmanaged git worktrees as sessions.

use super::*;

impl SessionManager {
    /// Sync unmanaged git worktrees as stopped sessions
    ///
    /// Lists actual git worktrees for the project and imports any that aren't
    /// already tracked as sessions. Imported worktrees get `Stopped` status —
    /// they have no running tmux session but can be attached to (which will
    /// recreate the tmux session on demand).
    #[instrument(skip(self))]
    pub async fn sync_worktrees(&self, project_id: &ProjectId) -> Result<usize> {
        let (repo_path, existing_paths, multi_repo_paths) = {
            let state = self.store.read().await;
            let project = match state.get_project(project_id) {
                Some(p) => p,
                None => return Ok(0),
            };

            let repo_path = project.repo_path.clone();

            // Collect canonicalized worktree paths from all existing sessions
            let paths: Vec<PathBuf> = project
                .worktrees
                .iter()
                .filter_map(|sid| state.get_session(sid))
                .filter_map(|s| std::fs::canonicalize(&s.worktree_path).ok())
                .collect();

            // Collect canonicalized worktree paths owned by multi-repo sessions
            // so we don't re-import their per-repo worktrees as standalone
            // single-repo sessions on every sync tick.
            let mr_paths: std::collections::HashSet<PathBuf> = state
                .multi_repo_sessions
                .values()
                .flat_map(|mr| mr.repos.iter())
                .filter_map(|entry| std::fs::canonicalize(&entry.worktree_path).ok())
                .collect();

            (repo_path, paths, mr_paths)
        };

        // Open git backend and list worktrees
        let backend = match GitBackend::open(&repo_path) {
            Ok(b) => b,
            Err(e) => {
                warn!("Failed to open git backend for sync: {}", e);
                return Ok(0);
            }
        };

        let worktrees_dir = self.config_store.read().worktrees_dir()?;
        let canonical_worktrees_dir =
            std::fs::canonicalize(&worktrees_dir).unwrap_or_else(|_| worktrees_dir.clone());
        let worktree_manager = WorktreeManager::new(backend, worktrees_dir);

        let worktrees = match worktree_manager.list_worktrees().await {
            Ok(w) => w,
            Err(e) => {
                warn!("Failed to list worktrees for sync: {}", e);
                return Ok(0);
            }
        };

        // Also canonicalize the repo path for main worktree comparison
        let canonical_repo = std::fs::canonicalize(&repo_path).unwrap_or(repo_path);

        let mut imported = 0;
        let mut new_sessions = Vec::new();

        for wt in &worktrees {
            if wt.is_main {
                continue;
            }

            let canonical_wt = match std::fs::canonicalize(&wt.path) {
                Ok(p) => p,
                Err(_) => continue, // Worktree path doesn't exist, skip
            };

            // Skip if this path matches the main repo
            if canonical_wt == canonical_repo {
                continue;
            }

            // Only import worktrees inside the managed worktrees directory
            if !canonical_wt.starts_with(&canonical_worktrees_dir) {
                continue;
            }

            // Skip if already tracked by an existing session
            if existing_paths.contains(&canonical_wt) {
                continue;
            }

            // Skip worktrees that are owned by a multi-repo session — they
            // live inside the shared parent dir and are managed there, not
            // as standalone per-project sessions.
            if multi_repo_paths.contains(&canonical_wt) {
                continue;
            }

            let mut session = WorktreeSession::new(
                *project_id,
                wt.branch.clone(),
                wt.branch.clone(),
                wt.path.clone(),
                self.config_store.read().default_program.clone(),
            );
            session.set_status(SessionStatus::Stopped);
            session.base_commit = Some(wt.head.clone());

            info!(
                "Importing unmanaged worktree as session: branch={}, path={:?}",
                wt.branch, wt.path
            );

            new_sessions.push(session);
            imported += 1;
        }

        if !new_sessions.is_empty() {
            self.store
                .mutate(move |state| {
                    for session in new_sessions {
                        state.add_session(session);
                    }
                })
                .await?;
        }

        if imported > 0 {
            info!(
                "Synced {} unmanaged worktree(s) for project {}",
                imported, project_id
            );
        }

        Ok(imported)
    }
}
