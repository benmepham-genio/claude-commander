//! Cached diff computation
//!
//! Provides efficient diff computation with caching:
//! - TTL-based cache to avoid redundant computation
//! - Background refresh for active sessions
//! - Incremental updates when possible

use crate::git::git_command;
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
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

struct RootWatch {
    watcher: Option<crate::file_events::FileEvents>,
    attempted_at: Instant,
    requested_at: Instant,
}

/// Cached diff computation, generic over key type
pub struct DiffCache<K> {
    /// Cache of key -> diff info
    cache: Arc<RwLock<HashMap<K, Arc<DiffInfo>>>>,
    /// Cache TTL
    ttl: Duration,
    flights: Arc<crate::singleflight::Flights<K>>,
    stats_cache: Arc<RwLock<HashMap<K, Arc<DiffInfo>>>>,
    watchers: Arc<std::sync::Mutex<HashMap<K, RootWatch>>>,
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
            flights: Arc::new(crate::singleflight::Flights::default()),
            stats_cache: Arc::new(RwLock::new(HashMap::new())),
            watchers: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Watch at most 32 recently requested roots and report whether the
    /// key's watch saw a change. Callers apply the configured TTL regardless
    /// of watch health, so a failed/unavailable watch still expires entries,
    /// including for nested working-tree edits.
    async fn freshness(&self, key: K, path: &Path) -> bool {
        let should_watch = {
            let mut watchers = self.watchers.lock().expect("diff watchers poisoned");
            match watchers.get_mut(&key) {
                Some(entry) => {
                    entry.requested_at = Instant::now();
                    !entry.watcher.as_ref().is_some_and(|w| w.healthy())
                        && entry.attempted_at.elapsed() >= Duration::from_secs(30)
                }
                None => true,
            }
        };
        let replacing = should_watch
            && self
                .watchers
                .lock()
                .expect("diff watchers poisoned")
                .contains_key(&key);
        if should_watch {
            let path = path.to_path_buf();
            let watcher = tokio::task::spawn_blocking(move || {
                crate::file_events::FileEvents::git(&path).ok()
            })
            .await
            .unwrap_or_default();
            let mut watchers = self.watchers.lock().expect("diff watchers poisoned");
            // A root deleted by another process has no `invalidate` caller,
            // but its watch saw the root's own Remove/rename and went
            // unhealthy: release such slots now rather than at LRU eviction.
            // Slots without a watcher are kept — they carry the retry
            // cooldown for roots that could not be watched.
            watchers
                .retain(|k, entry| *k == key || entry.watcher.as_ref().is_none_or(|w| w.healthy()));
            if watchers.len() >= 32
                && !watchers.contains_key(&key)
                && let Some(oldest) = watchers
                    .iter()
                    .min_by_key(|(_, entry)| entry.requested_at)
                    .map(|(key, _)| *key)
            {
                watchers.remove(&oldest);
            }
            watchers.insert(
                key,
                RootWatch {
                    watcher,
                    attempted_at: Instant::now(),
                    requested_at: Instant::now(),
                },
            );
        }
        let mut watchers = self.watchers.lock().expect("diff watchers poisoned");
        match watchers
            .get_mut(&key)
            .and_then(|entry| entry.watcher.as_mut())
        {
            Some(watcher) => watcher.take_changed() || replacing,
            _ => false,
        }
    }

    /// Get cached diff or compute fresh
    #[instrument(skip(self, worktree_path))]
    pub async fn get_diff(&self, key: &K, worktree_path: &Path) -> Result<Arc<DiffInfo>> {
        let flight = self.flights.for_key(*key);
        let _guard = flight.lock().await;
        let changed = self.freshness(*key, worktree_path).await;
        if changed {
            self.invalidate_values(key).await;
        }
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

    /// Stats consumers never generate or transfer a patch.
    pub async fn get_stats(&self, key: &K, path: &Path) -> Result<Arc<DiffInfo>> {
        let flight = self.flights.for_key(*key);
        let _guard = flight.lock().await;
        let changed = self.freshness(*key, path).await;
        if changed {
            self.invalidate_values(key).await;
        }
        if let Some(cached) = self.stats_cache.read().await.get(key)
            && !cached.is_stale(self.ttl)
        {
            return Ok(cached.clone());
        }
        let info = Arc::new(compute_diff_stats_for_path(path).await?);
        self.stats_cache.write().await.insert(*key, info.clone());
        Ok(info)
    }

    /// Compute a fresh diff
    pub async fn compute_diff(&self, key: &K, worktree_path: &Path) -> Result<Arc<DiffInfo>> {
        let info = Arc::new(compute_diff_for_path(worktree_path).await?);

        // Update cache
        {
            let mut cache = self.cache.write().await;
            cache.insert(*key, Arc::clone(&info));
            self.stats_cache
                .write()
                .await
                .insert(*key, Arc::clone(&info));
        }

        Ok(info)
    }

    /// Invalidate cache for a key.
    ///
    /// Takes the key's flight lock so a `get_diff`/`get_stats` already past
    /// its registration cannot insert a watch slot after this returns. Must
    /// not be called while holding that key's flight.
    pub async fn invalidate(&self, key: &K) {
        let flight = self.flights.for_key(*key);
        let _guard = flight.lock().await;
        self.watchers
            .lock()
            .expect("diff watchers poisoned")
            .remove(key);
        self.invalidate_values(key).await;
    }

    async fn invalidate_values(&self, key: &K) {
        let mut cache = self.cache.write().await;
        cache.remove(key);
        self.stats_cache.write().await.remove(key);
    }

    /// Clear all cached diffs
    pub async fn clear(&self) {
        self.watchers
            .lock()
            .expect("diff watchers poisoned")
            .clear();
        let mut cache = self.cache.write().await;
        cache.clear();
        self.stats_cache.write().await.clear();
    }

    /// Test hook: register (or refresh) the watch slot for `key`.
    #[cfg(test)]
    pub(crate) async fn watch_for_test(&self, key: K, path: &Path) {
        self.freshness(key, path).await;
    }

    /// Test hook: whether `key` still holds a watch slot.
    #[cfg(test)]
    pub(crate) fn is_watching(&self, key: &K) -> bool {
        self.watchers
            .lock()
            .expect("diff watchers poisoned")
            .contains_key(key)
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
            flights: self.flights.clone(),
            stats_cache: self.stats_cache.clone(),
            watchers: self.watchers.clone(),
        }
    }
}

/// Compute a diff for the given path (no caching)
pub async fn compute_diff_for_path(path: &Path) -> Result<DiffInfo> {
    // Run the tracked-change commands in parallel; untracked files are handled
    // by the shared `untracked_patch_and_count` helper (also used by the
    // review-diff composition) to avoid duplicating the per-file diff loop.
    let (diff_output, stat_output) = tokio::join!(
        git_command()
            .current_dir(path)
            .args(["diff", "HEAD"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
        git_command()
            .current_dir(path)
            .args(["diff", "--stat", "HEAD"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    );

    let diff_output = diff_output.map_err(|e| GitError::DiffFailed(e.to_string()))?;
    let stat_output = stat_output.map_err(|e| GitError::DiffFailed(e.to_string()))?;

    let mut diff = if diff_output.status.success() {
        String::from_utf8_lossy(&diff_output.stdout).to_string()
    } else {
        String::new()
    };

    let UntrackedPatch {
        patch: untracked_diff,
        count: untracked_count,
        // Stats/preview only: a missing untracked file understates the counts,
        // which is not worth failing this call over (nothing is pruned here).
        complete: _,
    } = untracked_patch_and_count(path).await;
    if !untracked_diff.is_empty() {
        // Ensure a blank-line separator between the tracked and untracked diffs.
        if !diff.is_empty() && !diff.ends_with("\n\n") {
            if diff.ends_with('\n') {
                diff.push('\n');
            } else {
                diff.push_str("\n\n");
            }
        }
        diff.push_str(&untracked_diff);
    }

    let (mut files_changed, lines_added, lines_removed) = if stat_output.status.success() {
        parse_diff_stat(&String::from_utf8_lossy(&stat_output.stdout))
    } else {
        (0, 0, 0)
    };
    files_changed += untracked_count;

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

/// Match preview counts without generating the tracked or untracked patch.
/// As in `compute_diff_for_path`, untracked files contribute to file count only.
pub async fn compute_diff_stats_for_path(path: &Path) -> Result<DiffInfo> {
    let (stat, untracked) = tokio::join!(
        git_command()
            .current_dir(path)
            .args(["diff", "--stat", "HEAD"])
            .stdin(Stdio::null())
            .output(),
        git_command()
            .current_dir(path)
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .stdin(Stdio::null())
            .output(),
    );
    let stat = stat.map_err(|e| GitError::DiffFailed(e.to_string()))?;
    let (mut files_changed, lines_added, lines_removed) = if stat.status.success() {
        parse_diff_stat(&String::from_utf8_lossy(&stat.stdout))
    } else {
        (0, 0, 0)
    };
    if let Ok(untracked) = untracked
        && untracked.status.success()
    {
        files_changed += untracked
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .count();
    }
    Ok(DiffInfo {
        files_changed,
        lines_added,
        lines_removed,
        base_commit: "HEAD".into(),
        ..DiffInfo::empty()
    })
}

/// An untracked-file patch: the `/dev/null`→file diffs concatenated, the count
/// of untracked files, and whether every one of them made it in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UntrackedPatch {
    pub(crate) patch: String,
    pub(crate) count: usize,
    /// `false` when git couldn't be run or a per-file diff failed, so `patch` is
    /// missing files that do exist. Callers that infer something from a file's
    /// *absence* (the review diff prunes comments and reviewed marks that way)
    /// must not trust an incomplete patch — see
    /// [`ComposedDiff::absence_is_authoritative`](super::ComposedDiff::absence_is_authoritative).
    pub(crate) complete: bool,
}

impl UntrackedPatch {
    /// An empty-but-trustworthy patch: there genuinely are no untracked files.
    fn none() -> Self {
        Self {
            complete: true,
            ..Default::default()
        }
    }
}

/// Build the untracked-file half of a worktree's diff.
///
/// Each untracked file is diffed against `/dev/null` via `git diff --no-index`
/// (which exits 1 when files differ — expected), capped at
/// [`UNTRACKED_DIFF_CONCURRENCY`] concurrent subprocesses to avoid EMFILE on
/// noisy worktrees. A git failure yields an empty patch flagged
/// [`UntrackedPatch::complete`]`= false` rather than an error, so a stats/preview
/// caller still gets the tracked half. Shared by [`compute_diff_for_path`] and
/// the review-diff composition in [`super::review_diff`].
pub(crate) async fn untracked_patch_and_count(path: &Path) -> UntrackedPatch {
    let ls = match git_command()
        .current_dir(path)
        .args(["ls-files", "--others", "--exclude-standard"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    {
        Ok(out) if out.status.success() => out,
        // Can't even enumerate: report incompleteness, since untracked files
        // may well exist and would look "absent" to a pruning caller.
        _ => return UntrackedPatch::default(),
    };

    let stdout = String::from_utf8_lossy(&ls.stdout);
    let files: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    let count = files.len();
    if count == 0 {
        return UntrackedPatch::none();
    }

    let mut diff_futures = Vec::with_capacity(count);
    for file in &files {
        diff_futures.push(
            git_command()
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

    let outputs: Vec<_> = futures::stream::iter(diff_futures)
        .buffered(UNTRACKED_DIFF_CONCURRENCY)
        .collect()
        .await;

    let mut patch = String::new();
    // A file missing from an otherwise successful composition is exactly the
    // shape that makes a still-present file look deleted, so every way one can
    // go missing has to be tracked rather than discarded:
    //
    // - the spawn failed (EMFILE under the concurrency this cap exists to bound);
    // - git ran but errored. `--no-index` exits 1 for "files differ", which is
    //   the expected success here, and 0 when they don't; anything else is a
    //   real failure (e.g. exit 128 `cannot hash` for a file whose mode denies
    //   read), and it writes nothing to stdout, so it would slip through
    //   unnoticed as an empty patch.
    let mut complete = true;
    for output in outputs {
        let Ok(output) = output else {
            complete = false;
            continue;
        };
        if !matches!(output.status.code(), Some(0 | 1)) {
            complete = false;
            continue;
        }
        let file_diff = String::from_utf8_lossy(&output.stdout);
        if file_diff.is_empty() {
            continue;
        }
        if !patch.is_empty() && !patch.ends_with('\n') {
            patch.push('\n');
        }
        patch.push_str(&file_diff);
    }

    UntrackedPatch {
        patch,
        count,
        complete,
    }
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

/// Render parsed diff counts as a git-style one-line summary, e.g.
/// `"3 files changed, 10 insertions(+), 5 deletions(-)"`. Insertion/deletion
/// clauses are omitted when zero, matching `git diff --stat` output.
fn format_diff_stat_summary(files: usize, added: usize, removed: usize) -> String {
    let mut summary = format!(
        "{files} {} changed",
        if files == 1 { "file" } else { "files" }
    );
    if added > 0 {
        summary.push_str(&format!(
            ", {added} {}(+)",
            if added == 1 {
                "insertion"
            } else {
                "insertions"
            }
        ));
    }
    if removed > 0 {
        summary.push_str(&format!(
            ", {removed} {}(-)",
            if removed == 1 {
                "deletion"
            } else {
                "deletions"
            }
        ));
    }
    summary
}

/// Compute a one-line diff-stat summary for the worktree at `path` relative to
/// `base` (a commit-ish such as a session's fork-point commit, or `HEAD`).
///
/// Untracked files are counted toward the file total so the figure matches the
/// diff view rendered in the TUI (see [`compute_diff_for_path`]). Returns `None` when
/// there are no changes, or when git cannot be run.
pub async fn diff_stat_summary(path: &Path, base: &str) -> Option<String> {
    let (stat_output, untracked_output) = tokio::join!(
        git_command()
            .current_dir(path)
            .args(["diff", "--stat", base])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
        git_command()
            .current_dir(path)
            .args(["ls-files", "--others", "--exclude-standard"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    );

    let stat_output = stat_output.ok().filter(|o| o.status.success())?;
    let (mut files, added, removed) =
        parse_diff_stat(&String::from_utf8_lossy(&stat_output.stdout));

    // Count untracked files into the total, mirroring `compute_diff`.
    if let Ok(out) = untracked_output
        && out.status.success()
    {
        files += String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count();
    }

    if files == 0 {
        return None;
    }

    Some(format_diff_stat_summary(files, added, removed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::fixture_git;

    #[tokio::test]
    async fn nested_worktree_edits_reconcile_without_recursive_watches() {
        let dir = tracked_repo().await;
        let nested = dir.path().join("target/deep");
        std::fs::create_dir_all(&nested).unwrap();
        let cache = DiffCache::<u64>::with_ttl(Duration::from_millis(20));
        cache.get_diff(&1, dir.path()).await.unwrap();
        let take_changed = || {
            cache
                .watchers
                .lock()
                .unwrap()
                .get_mut(&1)
                .and_then(|watch| watch.watcher.as_mut())
                .expect("this regression requires a registered watcher")
                .take_changed()
        };
        // Drain anything the diff's own git invocations produced.
        tokio::time::sleep(Duration::from_millis(200)).await;
        take_changed();

        // Pin non-recursion directly: a nested edit must not reach the watch
        // (a recursive registration would report it), while a root-level
        // edit still does.
        std::fs::write(nested.join("new.txt"), "nested change").unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!take_changed(), "nested edits must not be watched");
        std::fs::write(dir.path().join("root.txt"), "root change").unwrap();
        let observed = tokio::time::timeout(Duration::from_secs(2), async {
            while !take_changed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(observed.is_ok(), "root-level edits must still be watched");

        // The nested edit is still picked up, by the TTL alone.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            cache
                .get_diff(&1, dir.path())
                .await
                .unwrap()
                .diff
                .contains("nested change")
        );
    }

    #[tokio::test]
    async fn watcher_slots_follow_recent_roots_and_are_released() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = DiffCache::<u64>::new();
        for key in 0..32 {
            cache.freshness(key, dir.path()).await;
        }
        cache.freshness(0, dir.path()).await;
        cache.freshness(32, dir.path()).await;
        {
            let watchers = cache.watchers.lock().unwrap();
            assert_eq!(watchers.len(), 32);
            assert!(watchers.contains_key(&0));
            assert!(!watchers.contains_key(&1));
            assert!(watchers.contains_key(&32));
        }
        cache.invalidate(&0).await;
        assert!(!cache.watchers.lock().unwrap().contains_key(&0));
        cache.clear().await;
        assert!(cache.watchers.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalidate_waits_for_an_in_flight_registration() {
        // A get_diff whose watch registration began before teardown must not
        // re-insert its slot after `invalidate` has already run.
        let dir = tempfile::TempDir::new().unwrap();
        let cache = DiffCache::<u64>::new();
        let flight = cache.flights.for_key(1);
        let guard = flight.lock().await;
        let invalidating = tokio::spawn({
            let cache = cache.clone();
            async move { cache.invalidate(&1).await }
        });
        // Current-thread runtime: this lets `invalidate` run until it blocks.
        tokio::task::yield_now().await;
        cache.freshness(1, dir.path()).await;
        drop(guard);
        invalidating.await.unwrap();
        assert!(
            !cache.is_watching(&1),
            "invalidate must release a slot registered by an in-flight get_diff"
        );
    }

    #[tokio::test]
    async fn watchers_for_roots_deleted_elsewhere_are_pruned() {
        // Another process deleted the worktree: nothing calls `invalidate`,
        // and LRU eviction alone would hold its slot until the 33rd key.
        let gone = tempfile::TempDir::new().unwrap();
        let live = tempfile::TempDir::new().unwrap();
        let cache = DiffCache::<u64>::new();
        cache.freshness(1, gone.path()).await;
        assert!(cache.is_watching(&1));
        drop(gone);
        // The slot is released once its watch has seen the root go.
        let unhealthy = tokio::time::timeout(Duration::from_secs(5), async {
            while cache.watchers.lock().unwrap()[&1]
                .watcher
                .as_ref()
                .is_some_and(|w| w.healthy())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            unhealthy.is_ok(),
            "the watch must observe the root's removal"
        );
        cache.freshness(2, live.path()).await;
        assert!(
            !cache.is_watching(&1),
            "a vanished root must release its slot"
        );
        assert!(cache.is_watching(&2));
    }

    #[tokio::test]
    async fn unavailable_watches_retry_after_a_cooldown() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("not-yet-created");
        let cache = DiffCache::<u64>::new();
        let has_watcher = |cache: &DiffCache<u64>| {
            cache
                .watchers
                .lock()
                .unwrap()
                .get(&1)
                .unwrap()
                .watcher
                .is_some()
        };
        assert!(!cache.freshness(1, &path).await);
        assert!(!has_watcher(&cache));
        std::fs::create_dir(&path).unwrap();
        assert!(!cache.freshness(1, &path).await);
        assert!(!has_watcher(&cache), "no retry inside the cooldown");
        cache
            .watchers
            .lock()
            .unwrap()
            .get_mut(&1)
            .unwrap()
            .attempted_at = Instant::now() - Duration::from_secs(31);
        cache.freshness(1, &path).await;
        assert!(has_watcher(&cache), "retried after the cooldown");
    }

    async fn tracked_repo() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            assert!(
                fixture_git()
                    .current_dir(dir.path())
                    .args(args)
                    .output()
                    .await
                    .unwrap()
                    .status
                    .success()
            );
        }
        std::fs::write(dir.path().join("Cargo.lock"), "one\n").unwrap();
        assert!(
            fixture_git()
                .current_dir(dir.path())
                .args(["add", "."])
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        assert!(
            fixture_git()
                .current_dir(dir.path())
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "-m",
                    "tracked"
                ])
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        dir
    }

    #[tokio::test]
    async fn stats_only_matches_preview_counts_without_a_patch() {
        let dir = tracked_repo().await;
        std::fs::write(dir.path().join("Cargo.lock"), "two\n").unwrap();
        std::fs::write(dir.path().join("new file\nwith newline"), "new\n").unwrap();
        let stats = compute_diff_stats_for_path(dir.path()).await.unwrap();
        assert_eq!(
            (stats.files_changed, stats.lines_added, stats.lines_removed),
            (2, 1, 1)
        );
        assert!(stats.diff.is_empty());
        assert_eq!(stats.line_count, 0);
    }

    #[tokio::test]
    async fn cloned_diff_caches_observe_worktree_edits_before_reconciliation() {
        let dir = tracked_repo().await;
        let cache = DiffCache::<u64>::with_ttl(Duration::from_secs(60));
        let initial = cache.get_diff(&1, dir.path()).await.unwrap();
        assert!(!initial.has_changes());
        // `None` when no watch is registered, else whether it is healthy.
        let watcher_health = || {
            cache
                .watchers
                .lock()
                .unwrap()
                .get(&1)
                .and_then(|watch| watch.watcher.as_ref())
                .map(|watcher| watcher.healthy())
        };
        assert!(
            watcher_health().is_some(),
            "this regression requires a registered watcher"
        );
        let clone = cache.clone();
        std::fs::write(dir.path().join("Cargo.lock"), "updated\n").unwrap();
        // Allow event delivery and Git work under parallel CI load, while
        // staying well below the 60-second TTL, so only the watch can satisfy
        // this. Health is not asserted afterwards: a watcher marked failed
        // still invalidates, and (assumption, untested on macOS) notify's
        // FSEvents backend can flag Rescan (notify 8.2.0 src/fsevent.rs:116-117,
        // MUST_SCAN_SUBDIRS).
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if clone
                    .get_diff(&1, dir.path())
                    .await
                    .unwrap()
                    .diff
                    .contains("+updated")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "filesystem edits should invalidate the shared cache; watcher healthy: {:?}, cache age: {:?}",
            watcher_health(),
            initial.computed_at.elapsed(),
        );
    }

    #[tokio::test]
    async fn concurrent_diff_misses_share_the_computed_value() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(
            crate::git::fixture::fixture_git_std()
                .current_dir(dir.path())
                .args(["init"])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            crate::git::fixture::fixture_git_std()
                .current_dir(dir.path())
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "initial"
                ])
                .output()
                .unwrap()
                .status
                .success()
        );
        let cache = DiffCache::<u64>::with_ttl(Duration::from_secs(60));
        let (first, second) = tokio::join!(
            cache.get_diff(&1, dir.path()),
            cache.get_diff(&1, dir.path())
        );
        assert!(Arc::ptr_eq(&first.unwrap(), &second.unwrap()));
    }

    #[test]
    fn test_format_diff_stat_summary_pluralization() {
        assert_eq!(
            format_diff_stat_summary(1, 1, 1),
            "1 file changed, 1 insertion(+), 1 deletion(-)"
        );
        assert_eq!(
            format_diff_stat_summary(3, 10, 5),
            "3 files changed, 10 insertions(+), 5 deletions(-)"
        );
    }

    #[test]
    fn test_format_diff_stat_summary_omits_zero_clauses() {
        assert_eq!(format_diff_stat_summary(2, 0, 0), "2 files changed");
        assert_eq!(
            format_diff_stat_summary(1, 4, 0),
            "1 file changed, 4 insertions(+)"
        );
        assert_eq!(
            format_diff_stat_summary(1, 0, 4),
            "1 file changed, 4 deletions(-)"
        );
    }

    /// End-to-end check against a real git repo: a tracked modification plus an
    /// untracked file should both be reflected in the summary.
    #[tokio::test]
    async fn test_diff_stat_summary_counts_tracked_and_untracked() {
        use std::fs;
        use tempfile::TempDir;

        async fn git(dir: &Path, args: &[&str]) {
            let status = fixture_git()
                .current_dir(dir)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .expect("git command runs");
            assert!(status.success(), "git {args:?} failed");
        }

        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path();
        git(path, &["init", "-q"]).await;
        git(path, &["config", "user.email", "test@example.com"]).await;
        git(path, &["config", "user.name", "Test"]).await;

        fs::write(path.join("tracked.txt"), "line one\n").unwrap();
        git(path, &["add", "."]).await;
        git(path, &["commit", "-q", "-m", "initial"]).await;

        // Modify the tracked file and add an untracked one.
        fs::write(path.join("tracked.txt"), "line one changed\n").unwrap();
        fs::write(path.join("untracked.txt"), "new\n").unwrap();

        let summary = diff_stat_summary(path, "HEAD")
            .await
            .expect("summary present");
        // 1 tracked modification + 1 untracked file = 2 files.
        assert!(
            summary.starts_with("2 files changed"),
            "unexpected summary: {summary}"
        );
        assert!(
            summary.contains("insertion"),
            "unexpected summary: {summary}"
        );
    }

    #[tokio::test]
    async fn test_diff_stat_summary_none_when_clean() {
        use std::fs;
        use tempfile::TempDir;

        async fn git(dir: &Path, args: &[&str]) {
            fixture_git()
                .current_dir(dir)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .expect("git command runs");
        }

        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path();
        git(path, &["init", "-q"]).await;
        git(path, &["config", "user.email", "test@example.com"]).await;
        git(path, &["config", "user.name", "Test"]).await;
        fs::write(path.join("f.txt"), "x\n").unwrap();
        git(path, &["add", "."]).await;
        git(path, &["commit", "-q", "-m", "initial"]).await;

        assert!(diff_stat_summary(path, "HEAD").await.is_none());
    }

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
        // Exercise the TTL hit independently of filesystem notifications. Model
        // an unavailable watcher inside its retry cooldown so the first read
        // cannot invalidate the sentinel while registering a real watcher.
        cache.watchers.lock().unwrap().insert(
            7,
            RootWatch {
                watcher: None,
                attempted_at: Instant::now(),
                requested_at: Instant::now(),
            },
        );
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
