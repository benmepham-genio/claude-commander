//! Git worktree management
//!
//! Provides worktree lifecycle operations:
//! - Create worktree with new or existing branch
//! - Remove worktree
//! - List worktrees

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tracing::{debug, info, instrument, warn};

use super::GitBackend;
use super::worktree_include::copy_worktree_includes;
use crate::error::{GitError, Result};

/// Worktree information
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Path to the worktree
    pub path: PathBuf,
    /// Branch name
    pub branch: String,
    /// HEAD commit ID
    pub head: String,
    /// Whether this is the main worktree
    pub is_main: bool,
}

/// Worktree manager
///
/// Handles git worktree operations for session isolation.
///
/// Note: gitoxide's worktree support is still evolving, so we use
/// a hybrid approach: gitoxide for read operations, git CLI for mutations.
pub struct WorktreeManager {
    /// Git backend
    backend: GitBackend,
    /// Base directory for worktrees
    worktrees_dir: PathBuf,
}

impl WorktreeManager {
    /// Create a new worktree manager
    pub fn new(backend: GitBackend, worktrees_dir: PathBuf) -> Self {
        Self {
            backend,
            worktrees_dir,
        }
    }

    /// Get the repository path
    pub fn repo_path(&self) -> &Path {
        self.backend.path()
    }

    /// Get the worktrees directory
    pub fn worktrees_dir(&self) -> &Path {
        &self.worktrees_dir
    }

    /// Create a new worktree
    ///
    /// If the branch exists, checks it out into the worktree.
    /// If the branch doesn't exist, creates it from HEAD.
    #[instrument(skip(self))]
    pub async fn create_worktree(
        &self,
        worktree_name: &str,
        branch_name: &str,
    ) -> Result<WorktreeInfo> {
        let worktree_path = self.worktrees_dir.join(worktree_name);
        let worktrees_dir = self.worktrees_dir.clone();
        let repo_path = self.backend.path().to_owned();

        // Check if branch exists (sync gix operation — done before any .await
        // so that non-Sync gix types don't cross await boundaries)
        let branch_exists = self.backend.branch_exists(branch_name)?;

        // All remaining work is async CLI commands that don't need &self
        Self::run_create_worktree(
            worktrees_dir,
            repo_path,
            worktree_path,
            branch_name.to_string(),
            branch_exists,
            None,
        )
        .await
    }

    /// Run the async portion of worktree creation (CLI commands only, no gix types).
    ///
    /// This is a standalone async function so that non-Sync gix types from
    /// the sync preparation phase are not held across await points, keeping
    /// the resulting future `Send`.
    pub async fn run_create_worktree(
        worktrees_dir: PathBuf,
        repo_path: PathBuf,
        worktree_path: PathBuf,
        branch_name: String,
        branch_exists: bool,
        start_point: Option<String>,
    ) -> Result<WorktreeInfo> {
        // Ensure worktrees directory exists
        tokio::fs::create_dir_all(&worktrees_dir)
            .await
            .map_err(|e| {
                GitError::WorktreeError(format!("Failed to create worktrees dir: {}", e))
            })?;

        let mut cmd = Command::new("git");
        cmd.current_dir(&repo_path).arg("worktree").arg("add");

        if branch_exists {
            // Checkout existing branch (start_point is not applicable here)
            debug!("Branch {} exists, checking out", branch_name);
            if start_point.is_some() {
                debug!("Ignoring start_point for existing branch {}", branch_name);
            }
            cmd.arg(&worktree_path).arg(&branch_name);
        } else {
            // Create new branch, optionally from a specific start point
            debug!("Creating new branch {}", branch_name);
            cmd.arg("-b").arg(&branch_name).arg(&worktree_path);
            if let Some(ref sp) = start_point {
                debug!("Using start point {}", sp);
                cmd.arg(sp);
            }
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let git_add_start = std::time::Instant::now();
        let output = cmd
            .output()
            .await
            .map_err(|e| GitError::WorktreeError(format!("Failed to run git worktree: {}", e)))?;
        info!(
            "[timing] git worktree add took {}ms",
            git_add_start.elapsed().as_millis()
        );

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(
                GitError::WorktreeError(format!("git worktree add failed: {}", stderr)).into(),
            );
        }

        info!(
            "Created worktree at {:?} with branch {}",
            worktree_path, branch_name
        );

        // Copy .worktreeinclude files (best-effort)
        let includes_start = std::time::Instant::now();
        if let Err(e) = copy_worktree_includes(&repo_path, &worktree_path).await {
            warn!("Failed to copy worktree includes: {}", e);
        }
        info!(
            "[timing] copy_worktree_includes took {}ms",
            includes_start.elapsed().as_millis()
        );

        // Get the HEAD of the new worktree
        let head = Self::get_worktree_head_static(&worktree_path).await?;

        Ok(WorktreeInfo {
            path: worktree_path,
            branch: branch_name,
            head,
            is_main: false,
        })
    }

    /// Remove a worktree
    #[instrument(skip(self))]
    pub async fn remove_worktree(&self, worktree_path: &Path, force: bool) -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.current_dir(self.backend.path())
            .arg("worktree")
            .arg("remove");

        if force {
            cmd.arg("--force");
        }

        cmd.arg(worktree_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd
            .output()
            .await
            .map_err(|e| GitError::WorktreeError(format!("Failed to run git worktree: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(
                GitError::WorktreeError(format!("git worktree remove failed: {}", stderr)).into(),
            );
        }

        info!("Removed worktree at {:?}", worktree_path);
        Ok(())
    }

    /// List all worktrees
    #[instrument(skip(self))]
    pub async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let output = Command::new("git")
            .current_dir(self.backend.path())
            .args(["worktree", "list", "--porcelain"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| GitError::WorktreeError(format!("Failed to list worktrees: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(
                GitError::WorktreeError(format!("git worktree list failed: {}", stderr)).into(),
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let worktrees = parse_worktree_list(&stdout)?;

        Ok(worktrees)
    }

    /// Get HEAD commit of a worktree
    async fn get_worktree_head_static(worktree_path: &Path) -> Result<String> {
        let output = Command::new("git")
            .current_dir(worktree_path)
            .args(["rev-parse", "HEAD"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| GitError::WorktreeError(format!("Failed to get HEAD: {}", e)))?;

        if !output.status.success() {
            return Ok("unknown".to_string());
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Prune stale worktree references
    pub async fn prune(&self) -> Result<()> {
        let output = Command::new("git")
            .current_dir(self.backend.path())
            .args(["worktree", "prune"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| GitError::WorktreeError(format!("Failed to prune worktrees: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(
                GitError::WorktreeError(format!("git worktree prune failed: {}", stderr)).into(),
            );
        }

        Ok(())
    }
}

/// Parse git worktree list --porcelain output
fn parse_worktree_list(output: &str) -> Result<Vec<WorktreeInfo>> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_head: Option<String> = None;
    let mut current_branch: Option<String> = None;
    let mut is_main = true; // First worktree is main

    for line in output.lines() {
        if line.starts_with("worktree ") {
            // Save previous worktree if complete
            if let (Some(path), Some(head)) = (current_path.take(), current_head.take()) {
                worktrees.push(WorktreeInfo {
                    path,
                    branch: current_branch.take().unwrap_or_else(|| "HEAD".to_string()),
                    head,
                    is_main,
                });
                is_main = false;
            }

            current_path = Some(PathBuf::from(line.trim_start_matches("worktree ")));
        } else if line.starts_with("HEAD ") {
            current_head = Some(line.trim_start_matches("HEAD ").to_string());
        } else if line.starts_with("branch ") {
            let branch = line.trim_start_matches("branch refs/heads/");
            current_branch = Some(branch.to_string());
        }
    }

    // Don't forget the last worktree
    if let (Some(path), Some(head)) = (current_path, current_head) {
        worktrees.push(WorktreeInfo {
            path,
            branch: current_branch.unwrap_or_else(|| "HEAD".to_string()),
            head,
            is_main,
        });
    }

    Ok(worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_worktree_list() {
        let output = r#"worktree /path/to/main
HEAD abc123def456
branch refs/heads/main

worktree /path/to/feature
HEAD def456abc123
branch refs/heads/feature-branch
"#;

        let worktrees = parse_worktree_list(output).unwrap();
        assert_eq!(worktrees.len(), 2);

        assert_eq!(worktrees[0].path, PathBuf::from("/path/to/main"));
        assert_eq!(worktrees[0].branch, "main");
        assert!(worktrees[0].is_main);

        assert_eq!(worktrees[1].path, PathBuf::from("/path/to/feature"));
        assert_eq!(worktrees[1].branch, "feature-branch");
        assert!(!worktrees[1].is_main);
    }

    #[test]
    fn test_parse_worktree_list_single_main() {
        let output = "worktree /path/to/main\nHEAD abc123\nbranch refs/heads/main\n";
        let worktrees = parse_worktree_list(output).unwrap();
        assert_eq!(worktrees.len(), 1);
        assert!(worktrees[0].is_main);
        assert_eq!(worktrees[0].branch, "main");
    }

    #[test]
    fn test_parse_worktree_list_detached_head() {
        let output = "worktree /path/to/main\nHEAD abc123\nbranch refs/heads/main\n\nworktree /path/to/detached\nHEAD def456\n";
        let worktrees = parse_worktree_list(output).unwrap();
        assert_eq!(worktrees.len(), 2);
        assert_eq!(worktrees[1].branch, "HEAD");
    }

    #[test]
    fn test_parse_worktree_list_empty() {
        let worktrees = parse_worktree_list("").unwrap();
        assert!(worktrees.is_empty());
    }
}
