//! Copies `.worktreeinclude`-matched files into new worktrees.
//!
//! When the newly-created worktree has a `.worktreeinclude` file at its root,
//! files that are both gitignored in the source working tree and match its
//! patterns are copied from the source (e.g. `node_modules/`, build caches,
//! local env files).
//!
//! The include file is read from the new worktree rather than the source
//! working tree, so a stale source (on an older commit than the ref being
//! forked from) still triggers the copy as long as the ref itself contains
//! `.worktreeinclude`.

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::error::{GitError, Result};

/// Copy files matching `.worktreeinclude` patterns into a new worktree.
///
/// Runs two `git ls-files` commands against the repo root:
/// 1. All gitignored untracked files
/// 2. All files matching `.worktreeinclude` patterns
///
/// The intersection (files that are both gitignored AND match `.worktreeinclude`)
/// is copied into the worktree. Symlinks are skipped for security.
pub(super) async fn copy_worktree_includes(repo_path: &Path, worktree_path: &Path) -> Result<()> {
    let include_file = worktree_path.join(".worktreeinclude");
    if !tokio::fs::try_exists(&include_file).await.unwrap_or(false) {
        return Ok(());
    }

    // Build --exclude-from=<abs path> without losing non-UTF-8 bytes.
    let mut exclude_from_arg = std::ffi::OsString::from("--exclude-from=");
    exclude_from_arg.push(include_file.as_os_str());

    // Run both git ls-files commands concurrently
    let (gitignored_result, included_result) = tokio::join!(
        Command::new("git")
            .current_dir(repo_path)
            .args([
                "ls-files",
                "--ignored",
                "--exclude-standard",
                "-o",
                "-z",
                "--directory",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
        Command::new("git")
            .current_dir(repo_path)
            .args(["ls-files", "--ignored"])
            .arg(&exclude_from_arg)
            .args(["-o", "-z", "--directory"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    );

    let gitignored_output = gitignored_result
        .map_err(|e| GitError::WorktreeError(format!("Failed to list gitignored files: {}", e)))?;
    let included_output = included_result.map_err(|e| {
        GitError::WorktreeError(format!("Failed to list worktreeinclude files: {}", e))
    })?;

    if !gitignored_output.status.success() {
        let stderr = String::from_utf8_lossy(&gitignored_output.stderr);
        warn!("git ls-files --exclude-standard failed: {}", stderr);
        return Ok(());
    }
    if !included_output.status.success() {
        let stderr = String::from_utf8_lossy(&included_output.stderr);
        warn!(
            "git ls-files --exclude-from={} failed: {}",
            include_file.display(),
            stderr
        );
        return Ok(());
    }

    let gitignored: HashSet<&str> = parse_nul_separated(&gitignored_output.stdout);
    let included: HashSet<&str> = parse_nul_separated(&included_output.stdout);

    let intersection: Vec<&str> = gitignored.intersection(&included).copied().collect();

    if intersection.is_empty() {
        return Ok(());
    }

    let mut copied = 0usize;
    for rel_path in &intersection {
        // Reject paths with .. components to prevent directory traversal
        if Path::new(rel_path)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            warn!("Skipping path with '..' component: {}", rel_path);
            continue;
        }

        let source = repo_path.join(rel_path);
        let dest = worktree_path.join(rel_path);

        // Skip symlinks
        let meta = match tokio::fs::symlink_metadata(&source).await {
            Ok(meta) if meta.is_symlink() => {
                debug!("Skipping symlink: {}", rel_path);
                continue;
            }
            Err(e) => {
                warn!("Failed to stat {}: {}", rel_path, e);
                continue;
            }
            Ok(meta) => meta,
        };

        if meta.is_dir() {
            match copy_dir_recursive(&source, &dest).await {
                Ok(n) => copied += n,
                Err(e) => warn!("Failed to copy directory {}: {}", rel_path, e),
            }
        } else {
            if let Some(parent) = dest.parent()
                && let Err(e) = tokio::fs::create_dir_all(parent).await
            {
                warn!("Failed to create parent dir for {}: {}", rel_path, e);
                continue;
            }
            match tokio::fs::copy(&source, &dest).await {
                Ok(_) => copied += 1,
                Err(e) => warn!("Failed to copy {}: {}", rel_path, e),
            }
        }
    }

    info!("Copied {} worktree-included files", copied);
    Ok(())
}

/// Parse NUL-separated output from `git ls-files -z`, stripping trailing slashes
/// from directory entries produced by `--directory`.
fn parse_nul_separated(bytes: &[u8]) -> HashSet<&str> {
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => {
            warn!("git ls-files output contains non-UTF-8 filenames: {}", e);
            return HashSet::new();
        }
    };
    text.split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.strip_suffix('/').unwrap_or(s))
        .collect()
}

/// Recursively copy a directory, skipping symlinks at every level.
/// Returns the number of files copied.
async fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<usize> {
    tokio::fs::create_dir_all(dest)
        .await
        .map_err(|e| GitError::WorktreeError(format!("Failed to create dir {:?}: {}", dest, e)))?;

    let mut entries = tokio::fs::read_dir(src)
        .await
        .map_err(|e| GitError::WorktreeError(format!("Failed to read dir {:?}: {}", src, e)))?;

    let mut count = 0;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| GitError::WorktreeError(format!("Failed to read dir entry: {}", e)))?
    {
        let meta = match tokio::fs::symlink_metadata(entry.path()).await {
            Ok(m) => m,
            Err(e) => {
                warn!("Failed to stat {:?}: {}", entry.path(), e);
                continue;
            }
        };

        if meta.is_symlink() {
            continue;
        }

        let dest_entry = dest.join(entry.file_name());
        if meta.is_dir() {
            count += Box::pin(copy_dir_recursive(&entry.path(), &dest_entry)).await?;
        } else if let Err(e) = tokio::fs::copy(entry.path(), &dest_entry).await {
            warn!("Failed to copy {:?}: {}", entry.path(), e);
            continue;
        } else {
            count += 1;
        }
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;
    use std::process::Stdio;

    use tokio::process::Command;

    use super::*;

    #[test]
    fn test_parse_nul_separated() {
        let input = b"foo\0bar/baz\0node_modules/\0";
        let result = parse_nul_separated(input);
        assert_eq!(result, HashSet::from(["foo", "bar/baz", "node_modules"]));
    }

    #[test]
    fn test_parse_nul_separated_empty() {
        let result = parse_nul_separated(b"");
        assert!(result.is_empty());
    }

    /// Helper to run a git command in a directory
    async fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Initialize a git repo with an initial commit
    async fn init_repo(dir: &Path) {
        git(dir, &["init"]).await;
        git(dir, &["config", "user.email", "test@test.com"]).await;
        git(dir, &["config", "user.name", "Test"]).await;
        // Create an initial commit so HEAD exists
        let placeholder = dir.join(".gitkeep");
        tokio::fs::write(&placeholder, "").await.unwrap();
        git(dir, &["add", ".gitkeep"]).await;
        git(dir, &["-c", "commit.gpgsign=false", "commit", "-m", "init"]).await;
    }

    #[tokio::test]
    async fn test_copy_worktree_includes_no_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        tokio::fs::create_dir(&repo).await.unwrap();
        init_repo(&repo).await;

        let worktree = tmp.path().join("wt");
        tokio::fs::create_dir(&worktree).await.unwrap();

        // No .worktreeinclude — should be a no-op
        let result = copy_worktree_includes(&repo, &worktree).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_copy_worktree_includes_empty_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        tokio::fs::create_dir(&repo).await.unwrap();
        init_repo(&repo).await;

        // Create a gitignored file
        tokio::fs::write(repo.join(".gitignore"), "*.log\n")
            .await
            .unwrap();
        tokio::fs::write(repo.join("app.log"), "log content")
            .await
            .unwrap();

        let worktree = tmp.path().join("wt");
        tokio::fs::create_dir(&worktree).await.unwrap();

        // Create empty .worktreeinclude in the new worktree (where the
        // function reads it from)
        tokio::fs::write(worktree.join(".worktreeinclude"), "")
            .await
            .unwrap();

        let result = copy_worktree_includes(&repo, &worktree).await;
        assert!(result.is_ok());
        // Empty .worktreeinclude matches nothing — file should not be copied
        assert!(!worktree.join("app.log").exists());
    }

    #[tokio::test]
    async fn test_copy_worktree_includes_intersection() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        tokio::fs::create_dir(&repo).await.unwrap();
        init_repo(&repo).await;

        // .gitignore ignores *.log and .env
        tokio::fs::write(repo.join(".gitignore"), "*.log\n.env\n")
            .await
            .unwrap();
        // .worktreeinclude only wants *.log (committed to the repo so it's in
        // the ref; copied into wt below to simulate checkout)
        tokio::fs::write(repo.join(".worktreeinclude"), "*.log\n")
            .await
            .unwrap();

        // Create files
        tokio::fs::write(repo.join("app.log"), "log data")
            .await
            .unwrap();
        tokio::fs::write(repo.join(".env"), "SECRET=x")
            .await
            .unwrap();
        tokio::fs::create_dir(repo.join("src")).await.unwrap();
        tokio::fs::write(repo.join("src/main.rs"), "fn main(){}")
            .await
            .unwrap();

        // Commit tracked files so git knows about them
        git(
            &repo,
            &["add", ".gitignore", ".worktreeinclude", "src/main.rs"],
        )
        .await;
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-m", "add files"],
        )
        .await;

        let worktree = tmp.path().join("wt");
        tokio::fs::create_dir(&worktree).await.unwrap();
        tokio::fs::write(worktree.join(".worktreeinclude"), "*.log\n")
            .await
            .unwrap();

        let result = copy_worktree_includes(&repo, &worktree).await;
        assert!(result.is_ok());

        // app.log is gitignored AND in .worktreeinclude → copied
        assert!(worktree.join("app.log").exists());
        let content = tokio::fs::read_to_string(worktree.join("app.log"))
            .await
            .unwrap();
        assert_eq!(content, "log data");

        // .env is gitignored but NOT in .worktreeinclude → not copied
        assert!(!worktree.join(".env").exists());

        // src/main.rs is tracked, not gitignored → not copied
        assert!(!worktree.join("src/main.rs").exists());
    }

    #[tokio::test]
    async fn test_copy_worktree_includes_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        tokio::fs::create_dir(&repo).await.unwrap();
        init_repo(&repo).await;

        // .gitignore ignores node_modules/
        tokio::fs::write(repo.join(".gitignore"), "node_modules/\n")
            .await
            .unwrap();
        // .worktreeinclude includes node_modules/
        tokio::fs::write(repo.join(".worktreeinclude"), "node_modules/\n")
            .await
            .unwrap();

        // Create node_modules with nested structure
        let nm = repo.join("node_modules");
        tokio::fs::create_dir_all(nm.join("pkg/lib")).await.unwrap();
        tokio::fs::write(nm.join("pkg/package.json"), r#"{"name":"pkg"}"#)
            .await
            .unwrap();
        tokio::fs::write(nm.join("pkg/lib/index.js"), "module.exports = {}")
            .await
            .unwrap();

        git(&repo, &["add", ".gitignore", ".worktreeinclude"]).await;
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-m", "add files"],
        )
        .await;

        let worktree = tmp.path().join("wt");
        tokio::fs::create_dir(&worktree).await.unwrap();
        tokio::fs::write(worktree.join(".worktreeinclude"), "node_modules/\n")
            .await
            .unwrap();

        let result = copy_worktree_includes(&repo, &worktree).await;
        assert!(result.is_ok());

        // Entire directory tree should be copied
        assert!(worktree.join("node_modules/pkg/package.json").exists());
        assert!(worktree.join("node_modules/pkg/lib/index.js").exists());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_copy_worktree_includes_symlink_skipped() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        tokio::fs::create_dir(&repo).await.unwrap();
        init_repo(&repo).await;

        tokio::fs::write(repo.join(".gitignore"), "build/\n")
            .await
            .unwrap();
        tokio::fs::write(repo.join(".worktreeinclude"), "build/\n")
            .await
            .unwrap();

        // Create build dir with a real file and a symlink
        let build = repo.join("build");
        tokio::fs::create_dir(&build).await.unwrap();
        tokio::fs::write(build.join("output.bin"), "binary")
            .await
            .unwrap();
        symlink("/etc/passwd", build.join("sneaky_link")).unwrap();

        git(&repo, &["add", ".gitignore", ".worktreeinclude"]).await;
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-m", "add files"],
        )
        .await;

        let worktree = tmp.path().join("wt");
        tokio::fs::create_dir(&worktree).await.unwrap();
        tokio::fs::write(worktree.join(".worktreeinclude"), "build/\n")
            .await
            .unwrap();

        let result = copy_worktree_includes(&repo, &worktree).await;
        assert!(result.is_ok());

        // Real file is copied
        assert!(worktree.join("build/output.bin").exists());
        // Symlink is NOT copied
        assert!(!worktree.join("build/sneaky_link").exists());
    }

    /// Regression: the include file should be read from the new worktree,
    /// not the source working tree. If the source is on a stale commit that
    /// predates `.worktreeinclude` being added to the repo, the file won't be
    /// on disk in the source — but the newly-created worktree (checked out at
    /// a newer ref) will have it, and that's what should drive the copy.
    #[tokio::test]
    async fn test_copy_worktree_includes_reads_from_worktree_not_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        tokio::fs::create_dir(&repo).await.unwrap();
        init_repo(&repo).await;

        tokio::fs::write(repo.join(".gitignore"), "*.log\n")
            .await
            .unwrap();
        tokio::fs::write(repo.join("app.log"), "log data")
            .await
            .unwrap();
        git(&repo, &["add", ".gitignore"]).await;
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-m", "gitignore"],
        )
        .await;

        // Deliberately DO NOT create .worktreeinclude in the source repo —
        // simulating a stale main worktree that doesn't yet have it.
        assert!(!repo.join(".worktreeinclude").exists());

        let worktree = tmp.path().join("wt");
        tokio::fs::create_dir(&worktree).await.unwrap();
        // The new worktree was forked from a newer ref that contains the
        // include file.
        tokio::fs::write(worktree.join(".worktreeinclude"), "*.log\n")
            .await
            .unwrap();

        let result = copy_worktree_includes(&repo, &worktree).await;
        assert!(result.is_ok());

        // Copy should happen despite the source repo having no include file.
        assert!(worktree.join("app.log").exists());
    }
}
