//! Per-file "reviewed" marks for the review-diff view.
//!
//! A mark records that the user has reviewed one file of a session's review
//! diff, keyed by the file's display path and pinned to a hash of the file's
//! hunks at mark time. Marks persist across runs (one JSON file per session)
//! and are invalidated when the file's diff content changes or the file
//! leaves the diff — GitHub "Viewed" semantics.
//!
//! All logic here is pure or filesystem-only so it is testable without a TUI;
//! the presentation layer only renders and dispatches.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::config::{atomic_write, open_lock_file};
use crate::error::{ConfigError, Result, SessionError};
use crate::git::{FileDiff, ParsedDiff};
use crate::session::SessionId;
use claude_commander_protocol::api::ReviewedFileMark;
use claude_commander_viewmodel::review::DiffHashes;

/// A persisted "reviewed" mark for one file of a session's review diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedMark {
    /// Display path of the file (new path, or old path for deletions).
    pub file: String,
    /// Hex hash of the file's hunks at mark time
    /// (`claude_commander_viewmodel::review::hash_hex`).
    pub hash: String,
    pub marked_at: DateTime<Utc>,
}

impl From<&ReviewedMark> for ReviewedFileMark {
    fn from(mark: &ReviewedMark) -> Self {
        Self {
            file: mark.file.clone(),
            hash: mark.hash.clone(),
        }
    }
}

/// Which files of a review diff a bulk reviewed-mark change applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSelection {
    /// Every file in the current diff.
    All,
    /// These files, each a display path optionally suffixed `@<hash>` (the
    /// 16-hex-digit `hash` `review files --json` reports). Each must be in the
    /// current diff, and a given hash must still match, so a mark can only be
    /// pinned to the content the caller actually read. See [`resolve_selection`].
    Paths(Vec<String>),
}

/// Split `arg` into a display path and an expected hash. A literal path in the
/// diff always wins, so a file genuinely named `x@0123456789abcdef` still
/// resolves; otherwise a trailing `@` + 16 hex digits is the hash.
fn split_file_arg<'a>(arg: &'a str, hashes: &DiffHashes) -> (&'a str, Option<&'a str>) {
    if hashes.get(arg).is_some() {
        return (arg, None);
    }
    match arg.rsplit_once('@') {
        Some((path, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            (path, Some(hash))
        }
        _ => (arg, None),
    }
}

/// Resolve a selection to the diff's files, in diff order and deduplicated.
/// All-or-nothing: every named path not in the diff, and every one whose
/// `@hash` no longer matches, is reported together in
/// [`SessionError::FileSelectionRejected`].
pub fn resolve_selection<'a>(
    diff: &'a ParsedDiff,
    hashes: &DiffHashes,
    selection: &FileSelection,
) -> std::result::Result<Vec<&'a FileDiff>, SessionError> {
    let args = match selection {
        FileSelection::All => return Ok(diff.files.iter().collect()),
        FileSelection::Paths(args) => args,
    };
    let (mut missing, mut changed) = (Vec::new(), Vec::new());
    let mut wanted = HashSet::new();
    for arg in args {
        let (path, hash) = split_file_arg(arg, hashes);
        if hashes.get(path).is_none() {
            missing.push(path.to_string());
        } else if hash.is_some_and(|h| !hashes.holds(path, h)) {
            changed.push(path.to_string());
        } else {
            wanted.insert(path);
        }
    }
    if !missing.is_empty() || !changed.is_empty() {
        return Err(SessionError::FileSelectionRejected { missing, changed });
    }
    Ok(diff
        .files
        .iter()
        .filter(|f| wanted.contains(f.display_path()))
        .collect())
}

/// What a bulk reviewed-mark change did, by display path: `changed` files had
/// their mark set or cleared, `unchanged` were already in the asked-for state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SetReviewedOutcome {
    pub changed: Vec<String>,
    pub unchanged: Vec<String>,
}

/// Per-session reviewed-mark store, persisted as one JSON array per session
/// under a directory (typically `<data_dir>/reviewed/`).
pub struct ReviewedStore {
    dir: PathBuf,
}

impl ReviewedStore {
    /// Construct a store rooted at `dir` (created lazily on first save).
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path_for(&self, sid: SessionId) -> PathBuf {
        self.dir.join(format!("{}.json", sid.as_uuid()))
    }

    fn lock_path_for(&self, sid: SessionId) -> PathBuf {
        self.dir.join(format!("{}.lock", sid.as_uuid()))
    }

    /// Delete a session's marks and lock file, once the session itself is
    /// gone. Absent files are fine.
    pub async fn remove(&self, sid: SessionId) -> Result<()> {
        for path in [self.path_for(sid), self.lock_path_for(sid)] {
            match fs::remove_file(&path).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(ConfigError::SaveFailed(e.to_string()).into());
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Load a session's marks (an absent file yields an empty list). Off the
    /// async executor, like [`Self::update`].
    pub async fn load(&self, sid: SessionId) -> Result<Vec<ReviewedMark>> {
        let path = self.path_for(sid);
        tokio::task::spawn_blocking(move || read_marks(&path))
            .await
            .map_err(|e| ConfigError::LoadFailed(format!("Blocking task panicked: {e}")))?
    }

    /// Read-modify-write a session's marks under an exclusive `flock`, so the
    /// CLI and a frontend changing the same session's marks at once can't
    /// overwrite each other's changes. `f` returns its result and whether it
    /// changed the marks; only a change is written back (atomically).
    pub async fn update<F, R>(&self, sid: SessionId, f: F) -> Result<R>
    where
        F: FnOnce(&mut Vec<ReviewedMark>) -> (R, bool) + Send + 'static,
        R: Send + 'static,
    {
        let dir = self.dir.clone();
        let path = self.path_for(sid);
        let lock_path = self.lock_path_for(sid);
        // flock() blocks, so the whole cycle runs off the async executor.
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir).map_err(|e| ConfigError::SaveFailed(e.to_string()))?;
            let _lock = Flock::lock(open_lock_file(&lock_path)?, FlockArg::LockExclusive)
                .map_err(|(_, e)| ConfigError::SaveFailed(format!("Failed to lock marks: {e}")))?;
            let mut marks = read_marks(&path)?;
            let (result, changed) = f(&mut marks);
            if changed {
                atomic_write(&path, &marks)?;
            }
            Ok(result)
        })
        .await
        .map_err(|e| ConfigError::SaveFailed(format!("Blocking task panicked: {e}")))?
    }
}

/// A session's marks file, read synchronously (an absent file is empty).
fn read_marks(path: &Path) -> Result<Vec<ReviewedMark>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(serde_json::from_str(&s).map_err(|e| ConfigError::LoadFailed(e.to_string()))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(ConfigError::LoadFailed(e.to_string()).into()),
    }
}

/// Whether `marks` holds exactly `target` (same file, same content).
fn is_live(marks: &[ReviewedMark], target: &ReviewedFileMark) -> bool {
    marks
        .iter()
        .any(|m| m.file == target.file && m.hash.eq_ignore_ascii_case(&target.hash))
}

/// Drop marks whose file is absent from the diff `hashes` describes, or whose
/// stored hash no longer matches the file's. Only marks set before
/// `composed_before` (when the composition of that diff began) are judged: a
/// mark set since may be pinned to content newer than the diff — the CLI
/// marking a file the agent just edited — and pruning it would discard a
/// valid mark. Returns whether anything was removed.
pub fn prune_invalidated(
    marks: &mut Vec<ReviewedMark>,
    hashes: &DiffHashes,
    composed_before: DateTime<Utc>,
) -> bool {
    let before = marks.len();
    marks.retain(|m| !invalidated(m, hashes, composed_before));
    marks.len() != before
}

/// Whether [`prune_invalidated`] would remove anything — checked on an
/// unlocked read, so the common case (nothing stale) never takes the lock.
pub fn has_invalidated(
    marks: &[ReviewedMark],
    hashes: &DiffHashes,
    composed_before: DateTime<Utc>,
) -> bool {
    marks
        .iter()
        .any(|m| invalidated(m, hashes, composed_before))
}

fn invalidated(mark: &ReviewedMark, hashes: &DiffHashes, composed_before: DateTime<Utc>) -> bool {
    mark.marked_at < composed_before && !hashes.holds(&mark.file, &mark.hash)
}

/// Set (`reviewed`) or clear the mark for `target`'s file, pinned to
/// `target`'s hash (from [`DiffHashes::mark_for`]). Idempotent, unlike
/// [`toggle`], so a bulk caller can re-run without flipping anything back; a
/// mark pinned to other content is re-pinned. Returns whether the stored marks
/// changed.
pub fn set(marks: &mut Vec<ReviewedMark>, target: &ReviewedFileMark, reviewed: bool) -> bool {
    if reviewed && is_live(marks, target) {
        return false;
    }
    let before = marks.len();
    marks.retain(|m| m.file != target.file);
    if !reviewed {
        return marks.len() != before;
    }
    marks.push(ReviewedMark {
        file: target.file.clone(),
        hash: target.hash.clone(),
        marked_at: Utc::now(),
    });
    true
}

/// Toggle the mark for `target`'s file. A mark pinned to other content counts
/// as unreviewed (the file reads that way everywhere), so toggling re-pins it
/// rather than clearing it. Returns the new reviewed state.
pub fn toggle(marks: &mut Vec<ReviewedMark>, target: &ReviewedFileMark) -> bool {
    let now_reviewed = !is_live(marks, target);
    set(marks, target, now_reviewed);
    now_reviewed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::parse_unified_diff;
    use claude_commander_viewmodel::review::{hash_hex, live_paths};
    use tempfile::TempDir;

    /// Minimal one-file diff with the given path and body lines.
    fn diff_for(path: &str, body: &str) -> ParsedDiff {
        parse_unified_diff(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,2 +1,2 @@\n{body}"
        ))
    }

    /// The mark pinning `file` to its content, as `set`/`toggle` take it.
    fn target(file: &FileDiff) -> ReviewedFileMark {
        ReviewedFileMark {
            file: file.display_path().to_string(),
            hash: hash_hex(file),
        }
    }

    fn mark_for(file: &FileDiff) -> ReviewedMark {
        ReviewedMark {
            file: file.display_path().to_string(),
            hash: hash_hex(file),
            marked_at: Utc::now(),
        }
    }

    // --- store ---

    #[tokio::test]
    async fn load_missing_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = ReviewedStore::new(tmp.path().join("reviewed"));
        assert!(store.load(SessionId::new()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let store = ReviewedStore::new(tmp.path().join("reviewed"));
        let sid = SessionId::new();
        let diff = diff_for("src/a.rs", " ctx\n-old\n+new\n");
        let marks = vec![mark_for(&diff.files[0])];

        let written = marks.clone();
        store
            .update(sid, move |m| {
                *m = written;
                ((), true)
            })
            .await
            .unwrap();
        assert_eq!(store.load(sid).await.unwrap(), marks);
        // Other sessions are unaffected.
        assert!(store.load(SessionId::new()).await.unwrap().is_empty());
    }

    /// The CLI and a frontend can change one session's marks at the same
    /// moment; each read-modify-write is serialised, so neither change is lost.
    #[tokio::test]
    async fn concurrent_updates_lose_no_marks() {
        let tmp = TempDir::new().unwrap();
        let store = std::sync::Arc::new(ReviewedStore::new(tmp.path().join("reviewed")));
        let sid = SessionId::new();
        let writers: Vec<_> = (0..16)
            .map(|i| {
                let store = store.clone();
                tokio::spawn(async move {
                    store
                        .update(sid, move |m| {
                            // Widen the read→write window a lock must cover.
                            std::thread::sleep(std::time::Duration::from_millis(2));
                            m.push(ReviewedMark {
                                file: format!("f{i}.rs"),
                                hash: "0".into(),
                                marked_at: Utc::now(),
                            });
                            ((), true)
                        })
                        .await
                        .unwrap()
                })
            })
            .collect();
        for w in writers {
            w.await.unwrap();
        }
        assert_eq!(store.load(sid).await.unwrap().len(), 16);
    }

    // --- toggle ---

    #[test]
    fn toggle_adds_then_removes_mark() {
        let diff = diff_for("src/a.rs", " ctx\n-old\n+new\n");
        let file = &diff.files[0];
        let mut marks = Vec::new();

        assert!(toggle(&mut marks, &target(file)));
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].file, "src/a.rs");
        assert_eq!(marks[0].hash, hash_hex(file));

        assert!(!toggle(&mut marks, &target(file)));
        assert!(marks.is_empty());
    }

    /// A mark pinned to other content reads as unreviewed, so toggling it
    /// marks the file (re-pinned) instead of clearing a mark nobody can see.
    #[test]
    fn toggle_repins_a_mark_pinned_to_other_content() {
        let before = diff_for("src/a.rs", " ctx\n-old\n+new\n");
        let after = diff_for("src/a.rs", " ctx\n-old\n+newer\n");
        let mut marks = vec![mark_for(&before.files[0])];

        assert!(toggle(&mut marks, &target(&after.files[0])));
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].hash, hash_hex(&after.files[0]));
    }

    // --- set ---

    #[test]
    fn set_is_idempotent_unlike_toggle() {
        let diff = diff_for("src/a.rs", " ctx\n-old\n+new\n");
        let file = &diff.files[0];
        let mut marks = Vec::new();

        assert!(
            set(&mut marks, &target(file), true),
            "first mark is a change"
        );
        assert!(
            !set(&mut marks, &target(file), true),
            "re-marking is a no-op"
        );
        assert_eq!(marks.len(), 1);

        assert!(set(&mut marks, &target(file), false), "unmark is a change");
        assert!(
            !set(&mut marks, &target(file), false),
            "re-unmarking is a no-op"
        );
        assert!(marks.is_empty());
    }

    #[test]
    fn set_repins_a_mark_whose_file_changed_since() {
        let before = diff_for("src/a.rs", " ctx\n-old\n+new\n");
        let after = diff_for("src/a.rs", " ctx\n-old\n+newer\n");
        let mut marks = vec![mark_for(&before.files[0])];

        assert!(
            set(&mut marks, &target(&after.files[0]), true),
            "a stale mark re-pinned to the current content is a change"
        );
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].hash, hash_hex(&after.files[0]));
    }

    // --- resolve_selection ---

    fn two_files() -> ParsedDiff {
        parse_unified_diff(concat!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n",
            "diff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1,1 +1,1 @@\n-p\n+q\n",
        ))
    }

    fn resolved(
        diff: &ParsedDiff,
        args: &[&str],
    ) -> std::result::Result<Vec<String>, SessionError> {
        resolve_selection(
            diff,
            &DiffHashes::of(diff),
            &FileSelection::Paths(args.iter().map(|a| a.to_string()).collect()),
        )
        .map(|files| files.iter().map(|f| f.display_path().to_string()).collect())
    }

    #[test]
    fn selection_accepts_a_matching_hash_and_dedupes_in_diff_order() {
        let diff = two_files();
        let b_at = format!("b.rs@{}", hash_hex(&diff.files[1]));
        assert_eq!(
            resolved(&diff, &[&b_at, "a.rs", "b.rs"]).unwrap(),
            ["a.rs", "b.rs"]
        );
    }

    /// The point of `path@hash`: a file edited after the caller read it is
    /// refused, so a mark can't be pinned to content nobody looked at.
    #[test]
    fn selection_refuses_a_file_changed_since_its_hash_was_read() {
        let diff = two_files();
        let err = resolved(&diff, &["a.rs", "b.rs@00000000deadbeef", "nope.rs"]).unwrap_err();
        let SessionError::FileSelectionRejected { missing, changed } = &err else {
            panic!("unexpected {err}");
        };
        assert_eq!(changed, &["b.rs"]);
        assert_eq!(missing, &["nope.rs"]);
        // Both problems reach the caller at once, so one retry can fix both.
        let message = err.to_string();
        assert!(
            message.contains("nope.rs") && message.contains("b.rs"),
            "{message}"
        );
    }

    #[test]
    fn selection_hash_is_case_insensitive() {
        let diff = two_files();
        let upper = format!("a.rs@{}", hash_hex(&diff.files[0]).to_ascii_uppercase());
        assert_eq!(resolved(&diff, &[&upper]).unwrap(), ["a.rs"]);
    }

    #[test]
    fn selection_prefers_a_literal_path_that_looks_hashed() {
        let name = "odd@0123456789abcdef";
        let diff = parse_unified_diff(&format!(
            "diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n@@ -1,1 +1,1 @@\n-x\n+y\n"
        ));
        assert_eq!(resolved(&diff, &[name]).unwrap(), [name]);
    }

    // --- live_paths ---

    #[test]
    fn live_paths_keeps_only_marks_matching_the_displayed_content() {
        let shown = parse_unified_diff(concat!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n",
            "diff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n",
        ));
        let stale_b = diff_for("b.rs", " ctx\n-old\n+new\n");
        let marks: Vec<ReviewedFileMark> = [
            mark_for(&shown.files[0]),
            mark_for(&stale_b.files[0]),
            ReviewedMark {
                file: "gone.rs".into(),
                hash: "0".into(),
                marked_at: Utc::now(),
            },
        ]
        .iter()
        .map(ReviewedFileMark::from)
        .collect();

        let live = live_paths(
            marks.iter().map(|m| (m.file.as_str(), m.hash.as_str())),
            &DiffHashes::of(&shown),
        );
        assert_eq!(live, HashSet::from(["a.rs".to_string()]));
    }

    // --- prune ---

    #[test]
    fn prune_keeps_mark_when_hash_matches() {
        let diff = diff_for("src/a.rs", " ctx\n-old\n+new\n");
        let mut marks = vec![mark_for(&diff.files[0])];

        assert!(!prune_invalidated(
            &mut marks,
            &DiffHashes::of(&diff),
            Utc::now()
        ));
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn prune_drops_mark_when_diff_content_changes() {
        let before = diff_for("src/a.rs", " ctx\n-old\n+new\n");
        let after = diff_for("src/a.rs", " ctx\n-old\n+newer\n");
        let mut marks = vec![mark_for(&before.files[0])];

        assert!(prune_invalidated(
            &mut marks,
            &DiffHashes::of(&after),
            Utc::now()
        ));
        assert!(marks.is_empty());
    }

    #[test]
    fn prune_drops_mark_when_file_leaves_diff() {
        let before = diff_for("src/a.rs", " ctx\n-old\n+new\n");
        let after = diff_for("src/other.rs", " ctx\n-old\n+new\n");
        let mut marks = vec![mark_for(&before.files[0])];

        assert!(prune_invalidated(
            &mut marks,
            &DiffHashes::of(&after),
            Utc::now()
        ));
        assert!(marks.is_empty());
    }

    /// A mark set after the diff's composition began may be pinned to newer
    /// content than that diff shows, so it isn't judged by it.
    #[test]
    fn prune_spares_marks_set_after_the_diff_was_composed() {
        let composed = diff_for("src/a.rs", " ctx\n-old\n+new\n");
        let newer = diff_for("src/a.rs", " ctx\n-old\n+newer\n");
        let composed_before = Utc::now() - chrono::Duration::seconds(1);
        let mut marks = vec![mark_for(&newer.files[0])];

        assert!(!prune_invalidated(
            &mut marks,
            &DiffHashes::of(&composed),
            composed_before
        ));
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn prune_returns_false_when_nothing_changes() {
        let diff = diff_for("src/a.rs", " ctx\n-old\n+new\n");
        let mut marks: Vec<ReviewedMark> = Vec::new();
        assert!(!prune_invalidated(
            &mut marks,
            &DiffHashes::of(&diff),
            Utc::now()
        ));
    }
}
