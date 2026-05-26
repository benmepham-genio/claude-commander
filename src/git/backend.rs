//! Git backend using pure gitoxide
//!
//! Provides git operations without any CLI dependencies.

use std::path::{Path, PathBuf};

use gix::Repository;
use tracing::{debug, instrument};

use crate::error::{GitError, Result};

/// Git backend using gitoxide
///
/// Provides all git operations through pure Rust implementation.
pub struct GitBackend {
    /// The gitoxide repository handle
    repo: Repository,
    /// Path to the repository
    path: PathBuf,
}

impl GitBackend {
    /// Open an existing repository
    #[instrument(skip_all, fields(path = %path.as_ref().display()))]
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        let repo = gix::open(&path).map_err(|e| {
            if e.to_string().contains("not a git repository") {
                GitError::NotARepository(path.clone())
            } else {
                GitError::Gix(e.to_string())
            }
        })?;

        debug!("Opened repository at {:?}", path);

        Ok(Self { repo, path })
    }

    /// Discover repository from a path (searches parent directories)
    #[instrument(skip_all, fields(path = %path.as_ref().display()))]
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        let repo =
            gix::discover(path).map_err(|_e| GitError::NotARepository(path.to_path_buf()))?;

        // Use `common_dir()` instead of `path()` because `path()` returns
        // `.git/worktrees/<name>` for linked worktrees, while `common_dir()`
        // always returns the main `.git` directory. `.parent()` then gives
        // the actual repository root in both cases.
        //
        // `common_dir()` may contain unresolved `../..` segments (e.g.
        // `.git/worktrees/foo/../..`), so we canonicalize before taking
        // the parent to get a clean path.
        let common = std::fs::canonicalize(repo.common_dir())
            .unwrap_or_else(|_| repo.common_dir().to_path_buf());
        let repo_path = common
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| path.to_path_buf());

        debug!("Discovered repository at {:?}", repo_path);

        Ok(Self {
            repo,
            path: repo_path,
        })
    }

    /// Get the repository path
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the current branch name
    pub fn current_branch(&self) -> Result<String> {
        let head = self.repo.head().map_err(|e| GitError::Gix(e.to_string()))?;

        match head.kind {
            gix::head::Kind::Symbolic(reference) => {
                // Get shortened name from the reference
                let name = reference.name.shorten().to_string();
                Ok(name)
            }
            gix::head::Kind::Detached { .. } => {
                // Return short commit ID for detached HEAD
                match head.id() {
                    Some(id) => {
                        let id_str = id.to_string();
                        let short = if id_str.len() > 8 {
                            &id_str[..8]
                        } else {
                            &id_str
                        };
                        Ok(format!("HEAD detached at {}", short))
                    }
                    None => Ok("HEAD (no commits)".to_string()),
                }
            }
            gix::head::Kind::Unborn(full_name) => {
                // Unborn branch - the full_name IS the reference name (a FullName)
                let name = full_name.shorten().to_string();
                Ok(name)
            }
        }
    }

    /// Check if a branch exists
    pub fn branch_exists(&self, branch_name: &str) -> Result<bool> {
        let refs = self
            .repo
            .references()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let branch_ref = format!("refs/heads/{}", branch_name);

        for reference in refs.all().map_err(|e| GitError::Gix(e.to_string()))? {
            match reference {
                Ok(r) => {
                    if r.name().as_bstr() == branch_ref.as_bytes() {
                        return Ok(true);
                    }
                }
                Err(_) => continue,
            }
        }

        Ok(false)
    }

    /// Get the HEAD commit ID
    pub fn head_commit_id(&self) -> Result<String> {
        let head = self.repo.head().map_err(|e| GitError::Gix(e.to_string()))?;
        match head.id() {
            Some(id) => Ok(id.to_string()),
            None => Err(GitError::InvalidRef("HEAD has no commits".to_string()).into()),
        }
    }

    /// Check if the working directory is dirty
    pub fn is_dirty(&self) -> Result<bool> {
        // Get the index
        let _index = self
            .repo
            .index()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        // For now, we'll use a simple heuristic: check if there are any changes
        // A full implementation would compare index to HEAD and worktree to index

        // This is a simplified check - in practice you'd want to use gix-status
        // which provides full status information
        Ok(false) // Placeholder - full implementation needed
    }

    /// Get the default branch from the remote via `refs/remotes/origin/HEAD`.
    ///
    /// Returns `None` if the ref doesn't exist or isn't a symbolic reference.
    pub fn remote_default_branch(&self) -> Option<String> {
        let reference = self
            .repo
            .try_find_reference("refs/remotes/origin/HEAD")
            .ok()??;

        let target_name = reference.inner.target.try_name()?;
        let short = target_name.shorten().to_string();
        short.strip_prefix("origin/").map(|s| s.to_string())
    }

    /// List all local and remote branches in the repository.
    ///
    /// Returns entries as `(short_name, is_remote)` where:
    /// - Local branches use their short name (e.g. `"main"`).
    /// - Remote branches use their short ref name including the remote
    ///   prefix (e.g. `"origin/feature-foo"`).
    /// - The remote pseudo-branch `origin/HEAD` is excluded.
    ///
    /// Entries are deduplicated and sorted: local branches first (alphabetical),
    /// then remote branches (alphabetical).
    pub fn list_branches(&self) -> Result<Vec<(String, bool)>> {
        let refs = self
            .repo
            .references()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let mut local: Vec<String> = Vec::new();
        let mut remote: Vec<String> = Vec::new();

        // Local branches
        for r in refs
            .local_branches()
            .map_err(|e| GitError::Gix(e.to_string()))?
            .flatten()
        {
            let name = r.name().shorten().to_string();
            local.push(name);
        }

        // Remote tracking branches (skip symbolic HEAD refs like `origin/HEAD`)
        for r in refs
            .remote_branches()
            .map_err(|e| GitError::Gix(e.to_string()))?
            .flatten()
        {
            let name = r.name().shorten().to_string();
            if name.ends_with("/HEAD") {
                continue;
            }
            remote.push(name);
        }

        local.sort();
        local.dedup();
        remote.sort();
        remote.dedup();

        let mut out: Vec<(String, bool)> = Vec::with_capacity(local.len() + remote.len());
        out.extend(local.into_iter().map(|n| (n, false)));
        out.extend(remote.into_iter().map(|n| (n, true)));
        Ok(out)
    }

    /// Check if a reference exists locally (e.g. `"refs/remotes/origin/main"`).
    pub fn ref_exists(&self, ref_name: &str) -> Result<bool> {
        let reference = self
            .repo
            .try_find_reference(ref_name)
            .map_err(|e| GitError::Gix(e.to_string()))?;
        Ok(reference.is_some())
    }

    /// Get the main branch name (main or master)
    pub fn detect_main_branch(&self) -> Result<String> {
        // Prefer the remote's declared default branch
        if let Some(branch) = self.remote_default_branch() {
            return Ok(branch);
        }

        // Fall back to local heuristic: main -> master -> current branch
        if self.branch_exists("main")? {
            Ok("main".to_string())
        } else if self.branch_exists("master")? {
            Ok("master".to_string())
        } else {
            self.current_branch()
        }
    }

    /// Get the repository name (directory name)
    pub fn repo_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    }

    /// Get the gitoxide repository handle
    pub fn repo(&self) -> &Repository {
        &self.repo
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_test_repo() -> (TempDir, GitBackend) {
        let temp_dir = TempDir::new().unwrap();

        // Initialize a git repository using gix
        let repo = gix::init(temp_dir.path()).unwrap();

        let backend = GitBackend {
            repo,
            path: temp_dir.path().to_path_buf(),
        };

        (temp_dir, backend)
    }

    #[test]
    fn test_repo_name() {
        let (_temp, backend) = init_test_repo();
        // TempDir creates random names, so just check it's not empty
        assert!(!backend.repo_name().is_empty());
    }

    #[test]
    fn test_detect_main_branch_unborn() {
        let (_temp, backend) = init_test_repo();
        // Newly initialized repo has unborn 'main' or 'master' branch
        let branch = backend.detect_main_branch();
        // This should not error, even for unborn branches
        assert!(branch.is_ok());
    }

    #[test]
    fn test_remote_default_branch_none_without_remote() {
        let (_temp, backend) = init_test_repo();
        assert!(backend.remote_default_branch().is_none());
    }

    #[test]
    fn test_ref_exists_false_without_remote() {
        let (_temp, backend) = init_test_repo();
        assert!(!backend.ref_exists("refs/remotes/origin/main").unwrap());
    }

    #[test]
    fn test_discover_from_worktree_resolves_to_main_repo() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repo with an initial commit (required for worktree add)
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo_path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(repo_path)
            .output()
            .unwrap();

        // Create a linked worktree
        let wt_path = temp_dir.path().join("my-worktree");
        std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                wt_path.to_str().unwrap(),
                "-b",
                "wt-branch",
            ])
            .current_dir(repo_path)
            .output()
            .unwrap();
        assert!(wt_path.exists(), "worktree should have been created");

        // Discover from the worktree path — should resolve to the main repo root
        let backend = GitBackend::discover(&wt_path).unwrap();
        let canonical_repo = std::fs::canonicalize(repo_path).unwrap();
        let canonical_discovered = std::fs::canonicalize(backend.path()).unwrap();
        assert_eq!(
            canonical_discovered, canonical_repo,
            "discover() from a worktree should resolve to the main repo root"
        );
    }
}
