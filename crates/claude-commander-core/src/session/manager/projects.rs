//! Project lifecycle: add and remove git repositories.

use super::*;

/// Canonicalize `path`, keeping it unchanged when it can't be resolved (it may
/// not exist yet, or sit behind a permission the caller will fail on anyway).
async fn canonical_or_keep(path: &Path) -> PathBuf {
    let path = path.to_path_buf();
    tokio::fs::canonicalize(&path).await.unwrap_or(path)
}

/// The identity a project is *stored* under: the canonicalized root of the git
/// repository containing `path`.
///
/// [`SessionManager::add_project`] writes this, and
/// [`CommanderService::ensure_project`](crate::api::CommanderService::ensure_project)
/// looks projects up by it. One function so the read and the write cannot
/// disagree: a lookup that resolved differently from the write would miss every
/// other spelling of the same checkout — a symlinked projects dir, macOS's
/// `/var` → `/private/var`, or a path pointing *inside* the work tree — and each
/// miss registers a duplicate project for one repository, which is exactly what
/// `ensure_project` exists to prevent.
///
/// Falls back to `path` when the repository can't be discovered; resolving is not
/// this function's place to report an error, and `add_project` produces the real
/// one a moment later.
///
/// The discovered `GitBackend` is dropped before the `await` — deliberately, so
/// this stays `Send` even though `gix::Repository` is not.
pub(crate) async fn repo_identity(path: &Path) -> PathBuf {
    let root = GitBackend::discover(path).map(|backend| backend.path().to_path_buf());
    canonical_or_keep(root.as_deref().unwrap_or(path)).await
}

impl SessionManager {
    /// Add a new project (git repository), tagged with `workspace` (`None` =
    /// Main). The tag is written with the project in one state mutation, so no
    /// snapshot ever shows it in the wrong workspace. Validating the name and
    /// ensuring its definition exist is the caller's job
    /// ([`CommanderService::add_project`](crate::api::CommanderService::add_project)).
    #[instrument(skip(self))]
    pub async fn add_project(
        &self,
        repo_path: PathBuf,
        workspace: Option<String>,
    ) -> Result<ProjectId> {
        // Discover git repository
        let backend = GitBackend::discover(&repo_path)?;
        let main_branch = backend.detect_main_branch()?;
        let name = backend.repo_name();
        // Read from the backend we already have open — the repo picker needs
        // this to tell which of the user's GitHub repos are already projects.
        let origin_url = backend.origin_url();

        info!("Adding project '{}' from {:?}", name, repo_path);

        // Shared with `repo_identity`, which is how `ensure_project` recognises a
        // path that is already registered.
        let repo_path = canonical_or_keep(backend.path()).await;
        let mut project = Project::new(name, repo_path, main_branch);
        project.origin_url = origin_url;
        project.workspace = workspace;
        let project_id = project.id;

        self.store
            .mutate(move |state| {
                state.add_project(project);
            })
            .await?;

        // Import any existing worktrees as sessions
        if let Err(e) = self.sync_worktrees(&project_id).await {
            warn!("Failed to sync worktrees on project add: {}", e);
        }

        Ok(project_id)
    }

    /// Scan a directory for git repositories and add them as projects.
    ///
    /// Walks the directory tree recursively. When a `.git` directory is found
    /// the repo is registered and that subtree is not descended further.
    /// Repos that already exist (matched by canonicalized git root) are skipped
    /// and keep their workspace; new ones are tagged with `workspace`.
    #[instrument(skip(self))]
    pub async fn scan_directory(
        &self,
        dir: &Path,
        workspace: Option<String>,
    ) -> Result<ScanResult> {
        // Collect all existing repo paths for duplicate detection
        let existing_paths: std::collections::HashSet<PathBuf> = {
            let state = self.store.read().await;
            state
                .projects
                .values()
                .map(|p| p.repo_path.clone())
                .collect()
        };

        // Walk the directory tree and collect git repo roots. The walk is an
        // unbounded recursive `read_dir`, so run it on a blocking thread rather
        // than stalling the async runtime.
        let scan_root = dir.to_path_buf();
        let repo_paths = tokio::task::spawn_blocking(move || Self::find_git_repos(&scan_root))
            .await
            .map_err(|e| {
                crate::error::GitError::OperationFailed(format!("directory scan task failed: {e}"))
            })?;
        info!("Found {} git repositories in {:?}", repo_paths.len(), dir);

        let mut added = 0;
        let mut skipped = 0;

        for repo_path in repo_paths {
            // Resolve to canonical git root for duplicate detection
            let canonical = match GitBackend::discover(&repo_path) {
                Ok(backend) => tokio::fs::canonicalize(backend.path())
                    .await
                    .unwrap_or_else(|_| backend.path().to_path_buf()),
                Err(e) => {
                    debug!("Skipping {:?}: {}", repo_path, e);
                    continue;
                }
            };

            if existing_paths.contains(&canonical) {
                skipped += 1;
                continue;
            }

            match self.add_project(repo_path, workspace.clone()).await {
                Ok(_) => added += 1,
                Err(e) => {
                    debug!("Failed to add {:?}: {}", canonical, e);
                }
            }
        }

        Ok(ScanResult { added, skipped })
    }

    /// Walk a directory tree and collect paths that contain a `.git` directory.
    /// Prunes subtrees once a `.git` is found (does not descend into repos).
    fn find_git_repos(dir: &Path) -> Vec<PathBuf> {
        let mut repos = Vec::new();
        let mut stack = vec![dir.to_path_buf()];

        while let Some(current) = stack.pop() {
            let entries = match std::fs::read_dir(&current) {
                Ok(entries) => entries,
                Err(_) => continue, // permission denied or other error — skip silently
            };

            let mut is_git_repo = false;
            let mut subdirs = Vec::new();

            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if entry.file_name() == ".git" {
                        is_git_repo = true;
                        // Don't break — we still want to avoid descending
                    } else {
                        subdirs.push(path);
                    }
                }
            }

            if is_git_repo {
                repos.push(current);
                // Prune: don't descend into this repo's subdirectories
            } else {
                // Not a repo — continue descending
                stack.extend(subdirs);
            }
        }

        repos
    }

    /// Remove a project and all its sessions
    #[instrument(skip(self))]
    pub async fn remove_project(&self, project_id: &ProjectId) -> Result<()> {
        // Read project data for tmux cleanup
        let project = {
            let state = self.store.read().await;
            state
                .get_project(project_id)
                .ok_or_else(|| SessionError::ProjectNotFound(project_id.to_string()))?
                .clone()
        };

        // Kill project shell tmux session if it exists
        if let Some(ref shell_name) = project.shell_tmux_session_name {
            let _ = self.tmux.kill_session(shell_name).await;
        }

        // Tear down each session: kill its tmux (a no-op when stopped) AND
        // remove its git worktree. `kill_session(_, true)` does both uniformly
        // regardless of status — removing only tmux here leaked the worktrees.
        for session_id in &project.worktrees {
            if let Err(e) = self.kill_session(session_id, true).await {
                warn!(
                    "Failed to tear down session {} while removing project: {}",
                    session_id, e
                );
            }
        }

        // Remove project from state (also removes sessions)
        let pid = *project_id;
        self.store
            .mutate(move |state| {
                state.remove_project(&pid);
            })
            .await?;

        // Release the filesystem watches held for the project and its sessions.
        self.project_diff_cache.invalidate(project_id).await;
        for session_id in &project.worktrees {
            self.diff_cache.invalidate(session_id).await;
        }

        info!("Removed project {}", project_id);
        Ok(())
    }
}
