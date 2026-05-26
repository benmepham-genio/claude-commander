//! Cached diff computation
//!
//! Provides efficient diff computation with caching:
//! - TTL-based cache to avoid redundant computation
//! - Background refresh for active sessions
//! - Incremental updates when possible

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, instrument};

use crate::error::{GitError, Result};

/// Cap concurrent `git diff --no-index` subprocesses for untracked files.
/// A noisy worktree with many untracked files can otherwise EMFILE.
const UNTRACKED_DIFF_CONCURRENCY: usize = 8;

/// Default diff cache TTL (500ms)
pub const DEFAULT_DIFF_CACHE_TTL: Duration = Duration::from_millis(500);

/// Computed diff information
#[derive(Debug, Clone)]
pub struct DiffInfo {
    /// The raw diff output
    pub diff: String,
    /// Number of files changed
    pub files_changed: usize,
    /// Lines added
    pub lines_added: usize,
    /// Lines removed
    pub lines_removed: usize,
    /// Total number of lines in the diff (precomputed)
    pub line_count: usize,
    /// When the diff was computed
    pub computed_at: Instant,
    /// Base commit for the diff
    pub base_commit: String,
}

impl DiffInfo {
    /// Create an empty diff info
    pub fn empty() -> Self {
        Self {
            diff: String::new(),
            files_changed: 0,
            lines_added: 0,
            lines_removed: 0,
            line_count: 0,
            computed_at: Instant::now(),
            base_commit: String::new(),
        }
    }

    /// Check if this diff is stale.
    ///
    /// A `ttl` of zero means "always stale": entries computed in the same
    /// instant as the check are considered expired. The `>=` (rather than
    /// strict `>`) also avoids a flake where two back-to-back `Instant::now()`
    /// calls return the same value on a fast machine.
    pub fn is_stale(&self, ttl: Duration) -> bool {
        self.computed_at.elapsed() >= ttl
    }

    /// Check if there are any changes
    pub fn has_changes(&self) -> bool {
        self.files_changed > 0 || self.lines_added > 0 || self.lines_removed > 0
    }

    /// Get a summary string
    pub fn summary(&self) -> String {
        if !self.has_changes() {
            "No changes".to_string()
        } else {
            format!(
                "{} file(s), +{} -{} lines",
                self.files_changed, self.lines_added, self.lines_removed
            )
        }
    }
}

/// Cached diff computation, generic over key type
pub struct DiffCache<K> {
    /// Cache of key -> diff info
    cache: Arc<RwLock<HashMap<K, Arc<DiffInfo>>>>,
    /// Cache TTL
    ttl: Duration,
}

impl<K: Eq + std::hash::Hash + Copy + std::fmt::Debug + std::fmt::Display + Send + Sync + 'static>
    DiffCache<K>
{
    /// Create a new diff cache
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_DIFF_CACHE_TTL)
    }

    /// Create with custom TTL
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            ttl,
        }
    }

    /// Get cached diff or compute fresh
    #[instrument(skip(self, worktree_path))]
    pub async fn get_diff(&self, key: &K, worktree_path: &Path) -> Result<Arc<DiffInfo>> {
        // Fast path: check cache
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(key)
                && !cached.is_stale(self.ttl)
            {
                debug!("Diff cache hit for {}", key);
                return Ok(Arc::clone(cached));
            }
        }

        // Slow path: compute fresh diff
        debug!("Diff cache miss for {}, computing", key);
        self.compute_diff(key, worktree_path).await
    }

    /// Compute a fresh diff
    pub async fn compute_diff(&self, key: &K, worktree_path: &Path) -> Result<Arc<DiffInfo>> {
        let info = Arc::new(compute_diff_for_path(worktree_path).await?);

        // Update cache
        {
            let mut cache = self.cache.write().await;
            cache.insert(*key, Arc::clone(&info));
        }

        Ok(info)
    }

    /// Invalidate cache for a key
    pub async fn invalidate(&self, key: &K) {
        let mut cache = self.cache.write().await;
        cache.remove(key);
    }

    /// Clear all cached diffs
    pub async fn clear(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
    }
}

impl<K: Eq + std::hash::Hash + Copy + std::fmt::Debug + std::fmt::Display + Send + Sync + 'static>
    Default for DiffCache<K>
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K> Clone for DiffCache<K> {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            ttl: self.ttl,
        }
    }
}

/// Compute a diff for the given path (no caching)
pub async fn compute_diff_for_path(path: &Path) -> Result<DiffInfo> {
    // Phase 1: Run the three independent git commands in parallel
    let (diff_output, untracked_output, stat_output) = tokio::join!(
        Command::new("git")
            .current_dir(path)
            .args(["diff", "HEAD"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
        Command::new("git")
            .current_dir(path)
            .args(["ls-files", "--others", "--exclude-standard"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
        Command::new("git")
            .current_dir(path)
            .args(["diff", "--stat", "HEAD"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    );

    let diff_output = diff_output.map_err(|e| GitError::DiffFailed(e.to_string()))?;
    let untracked_output = untracked_output.map_err(|e| GitError::DiffFailed(e.to_string()))?;
    let stat_output = stat_output.map_err(|e| GitError::DiffFailed(e.to_string()))?;

    let mut diff = if diff_output.status.success() {
        String::from_utf8_lossy(&diff_output.stdout).to_string()
    } else {
        String::new()
    };

    // Phase 2: Run untracked file diffs in parallel
    if untracked_output.status.success() {
        let untracked = String::from_utf8_lossy(&untracked_output.stdout);
        let untracked_files: Vec<&str> = untracked.lines().filter(|l| !l.is_empty()).collect();

        let mut diff_futures = Vec::with_capacity(untracked_files.len());
        for file in &untracked_files {
            diff_futures.push(
                Command::new("git")
                    .current_dir(path)
                    .args([
                        "diff",
                        "--no-index",
                        "--src-prefix=a/",
                        "--dst-prefix=b/",
                        "--",
                        "/dev/null",
                        file,
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output(),
            );
        }

        let untracked_diffs: Vec<_> = futures::stream::iter(diff_futures)
            .buffered(UNTRACKED_DIFF_CONCURRENCY)
            .collect()
            .await;

        for output in untracked_diffs.into_iter().flatten() {
            // git diff --no-index exits with 1 when files differ (expected)
            let file_diff_str = String::from_utf8_lossy(&output.stdout);
            if !file_diff_str.is_empty() {
                // Ensure blank line separator between file diffs
                if !diff.is_empty() && !diff.ends_with("\n\n") {
                    if diff.ends_with('\n') {
                        diff.push('\n');
                    } else {
                        diff.push_str("\n\n");
                    }
                }
                diff.push_str(&file_diff_str);
            }
        }
    }

    let (mut files_changed, lines_added, lines_removed) = if stat_output.status.success() {
        parse_diff_stat(&String::from_utf8_lossy(&stat_output.stdout))
    } else {
        (0, 0, 0)
    };

    // Count untracked files in the total
    if untracked_output.status.success() {
        let untracked_count = String::from_utf8_lossy(&untracked_output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count();
        files_changed += untracked_count;
    }

    let line_count = diff.lines().count();

    Ok(DiffInfo {
        diff,
        files_changed,
        lines_added,
        lines_removed,
        line_count,
        computed_at: Instant::now(),
        base_commit: "HEAD".to_string(),
    })
}

/// Parse git diff --stat output to extract statistics
fn parse_diff_stat(output: &str) -> (usize, usize, usize) {
    let mut files_changed = 0;
    let mut lines_added = 0;
    let mut lines_removed = 0;

    for line in output.lines() {
        // Look for summary line like: "3 files changed, 10 insertions(+), 5 deletions(-)"
        if line.contains("changed") {
            // Parse the summary line
            for part in line.split(',') {
                let part = part.trim();
                if part.contains("file") {
                    if let Some(num) = part.split_whitespace().next() {
                        files_changed = num.parse().unwrap_or(0);
                    }
                } else if part.contains("insertion") {
                    if let Some(num) = part.split_whitespace().next() {
                        lines_added = num.parse().unwrap_or(0);
                    }
                } else if part.contains("deletion")
                    && let Some(num) = part.split_whitespace().next()
                {
                    lines_removed = num.parse().unwrap_or(0);
                }
            }
            break;
        }
    }

    (files_changed, lines_added, lines_removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_diff_stat() {
        let output = " src/main.rs | 10 ++++------
 src/lib.rs  |  5 +++++
 2 files changed, 9 insertions(+), 6 deletions(-)";

        let (files, added, removed) = parse_diff_stat(output);
        assert_eq!(files, 2);
        assert_eq!(added, 9);
        assert_eq!(removed, 6);
    }

    #[test]
    fn test_parse_diff_stat_single_file() {
        let output = " README.md | 3 +++
 1 file changed, 3 insertions(+)";

        let (files, added, removed) = parse_diff_stat(output);
        assert_eq!(files, 1);
        assert_eq!(added, 3);
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_parse_diff_stat_empty() {
        let (files, added, removed) = parse_diff_stat("");
        assert_eq!(files, 0);
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_diff_info_empty() {
        let info = DiffInfo::empty();
        assert!(!info.has_changes());
        assert_eq!(info.summary(), "No changes");
    }

    #[test]
    fn test_diff_info_with_changes() {
        let info = DiffInfo {
            diff: "some diff".to_string(),
            files_changed: 2,
            lines_added: 10,
            lines_removed: 5,
            line_count: 1,
            computed_at: Instant::now(),
            base_commit: "abc123".to_string(),
        };

        assert!(info.has_changes());
        assert!(info.summary().contains("2 file(s)"));
        assert!(info.summary().contains("+10"));
        assert!(info.summary().contains("-5"));
    }

    #[test]
    fn test_diff_info_has_changes_only_added() {
        let info = DiffInfo {
            diff: String::new(),
            files_changed: 0,
            lines_added: 5,
            lines_removed: 0,
            line_count: 0,
            computed_at: Instant::now(),
            base_commit: String::new(),
        };
        assert!(info.has_changes());
    }

    #[test]
    fn test_diff_info_has_changes_only_files() {
        let info = DiffInfo {
            diff: String::new(),
            files_changed: 1,
            lines_added: 0,
            lines_removed: 0,
            line_count: 0,
            computed_at: Instant::now(),
            base_commit: String::new(),
        };
        assert!(info.has_changes());
    }

    #[test]
    fn test_diff_info_is_stale_zero_ttl() {
        let info = DiffInfo::empty();
        assert!(info.is_stale(Duration::ZERO));
    }

    #[test]
    fn test_parse_diff_stat_deletions_only() {
        let output = " file.rs | 3 ---\n 1 file changed, 3 deletions(-)";
        let (files, added, removed) = parse_diff_stat(output);
        assert_eq!(files, 1);
        assert_eq!(added, 0);
        assert_eq!(removed, 3);
    }

    #[test]
    fn test_parse_diff_stat_insertions_only() {
        let output = " file.rs | 5 +++++\n 1 file changed, 5 insertions(+)";
        let (files, added, removed) = parse_diff_stat(output);
        assert_eq!(files, 1);
        assert_eq!(added, 5);
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_diff_info_summary_exact_format() {
        let info = DiffInfo {
            diff: String::new(),
            files_changed: 3,
            lines_added: 15,
            lines_removed: 7,
            line_count: 0,
            computed_at: Instant::now(),
            base_commit: String::new(),
        };
        assert_eq!(info.summary(), "3 file(s), +15 -7 lines");
    }

    /// Closes mutant `diff.rs:68:9: replace DiffInfo::is_stale -> bool with true`.
    ///
    /// A `DiffInfo` computed just now is NOT stale against a generous TTL.
    /// If `is_stale` is replaced with constantly returning `true`, this fails.
    #[test]
    fn test_diff_info_is_stale_fresh_returns_false() {
        let info = DiffInfo::empty();
        assert!(!info.is_stale(Duration::from_secs(3600)));
    }

    /// Complements the fresh case: an entry older than TTL is stale.
    /// Constructs `computed_at` in the past via `Instant::now() - large_duration`
    /// so we don't have to sleep in tests.
    #[test]
    fn test_diff_info_is_stale_past_returns_true() {
        let mut info = DiffInfo::empty();
        info.computed_at = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("Instant arithmetic should succeed");
        assert!(info.is_stale(Duration::from_millis(500)));
    }

    /// Closes mutant `diff.rs:73:78: replace > with < in DiffInfo::has_changes`.
    ///
    /// Column 78 falls on the third comparison (`lines_removed > 0`). We pin
    /// it down by constructing a `DiffInfo` whose ONLY non-zero field is
    /// `lines_removed`. The mutated predicate (`lines_removed < 0`) would be
    /// false for `lines_removed == 5`, and the overall `||` chain would
    /// return false, failing this assertion.
    #[test]
    fn test_diff_info_has_changes_only_removed() {
        let info = DiffInfo {
            diff: String::new(),
            files_changed: 0,
            lines_added: 0,
            lines_removed: 5,
            line_count: 0,
            computed_at: Instant::now(),
            base_commit: String::new(),
        };
        assert!(info.has_changes());
    }

    /// Boundary case: all zeros means no changes. Ensures the `>` is strict
    /// (not `>=`) and that flipping it would not coincidentally still pass.
    #[test]
    fn test_diff_info_has_changes_all_zero() {
        let info = DiffInfo {
            diff: String::new(),
            files_changed: 0,
            lines_added: 0,
            lines_removed: 0,
            line_count: 0,
            computed_at: Instant::now(),
            base_commit: String::new(),
        };
        assert!(!info.has_changes());
    }

    /// Closes mutant `diff.rs:120:20: delete ! in DiffCache<K>::get_diff`.
    ///
    /// The `!` negates `is_stale` to gate the cache-hit early return. With
    /// `!` deleted, a fresh entry is treated as stale and `get_diff` falls
    /// through to `compute_diff_for_path` (which shells out to git on a
    /// non-git tempdir — at best returning a different `DiffInfo`).
    ///
    /// We pre-insert a fresh entry directly via the private `cache` field
    /// (same-module access), then assert `get_diff` returns the SAME `Arc`
    /// (pointer equality), proving the fast cache-hit path executed.
    #[tokio::test]
    async fn test_get_diff_returns_cached_when_fresh() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let cache: DiffCache<u32> = DiffCache::with_ttl(Duration::from_secs(3600));
        let sentinel = Arc::new(DiffInfo {
            diff: "sentinel".to_string(),
            files_changed: 42,
            lines_added: 0,
            lines_removed: 0,
            line_count: 0,
            computed_at: Instant::now(),
            base_commit: "sentinel-base".to_string(),
        });

        {
            let mut guard = cache.cache.write().await;
            guard.insert(7u32, Arc::clone(&sentinel));
        }

        let got = cache
            .get_diff(&7u32, tmp.path())
            .await
            .expect("get_diff should hit cache");
        assert!(
            Arc::ptr_eq(&got, &sentinel),
            "get_diff must return the cached Arc on fresh hit"
        );
        assert_eq!(got.files_changed, 42);
        assert_eq!(got.base_commit, "sentinel-base");
    }

    /// Closes mutant `diff.rs:147:9: replace DiffCache<K>::invalidate with ()`.
    ///
    /// Insert two entries, invalidate one, assert the targeted key is gone
    /// while the other remains. The `-> ()` mutation would leave both intact.
    #[tokio::test]
    async fn test_invalidate_removes_only_target_key() {
        let cache: DiffCache<u32> = DiffCache::with_ttl(Duration::from_secs(3600));
        let entry_a = Arc::new(DiffInfo::empty());
        let entry_b = Arc::new(DiffInfo::empty());

        {
            let mut guard = cache.cache.write().await;
            guard.insert(1u32, Arc::clone(&entry_a));
            guard.insert(2u32, Arc::clone(&entry_b));
        }

        cache.invalidate(&1u32).await;

        let guard = cache.cache.read().await;
        assert!(
            !guard.contains_key(&1u32),
            "invalidate must remove the targeted key"
        );
        assert!(
            guard.contains_key(&2u32),
            "invalidate must leave other keys untouched"
        );
    }

    /// Closes mutant `diff.rs:153:9: replace DiffCache<K>::clear with ()`.
    ///
    /// Insert two entries, clear, assert the cache is empty. The `-> ()`
    /// mutation would leave both entries in place.
    #[tokio::test]
    async fn test_clear_removes_all_entries() {
        let cache: DiffCache<u32> = DiffCache::with_ttl(Duration::from_secs(3600));

        {
            let mut guard = cache.cache.write().await;
            guard.insert(1u32, Arc::new(DiffInfo::empty()));
            guard.insert(2u32, Arc::new(DiffInfo::empty()));
        }

        cache.clear().await;

        let guard = cache.cache.read().await;
        assert!(guard.is_empty(), "clear must remove every cached entry");
    }
}
