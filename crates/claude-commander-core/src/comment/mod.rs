//! Local diff-review comments.
//!
//! An comment is a comment the user attaches to a line range in a session's
//! review diff. Comments are *staged* (persisted across restarts) until the
//! user applies them, at which point they are composed into a markdown brief
//! and handed to the agent (see the service layer). The captured `snippet` is
//! stored so a comment can be re-anchored even after the surrounding code
//! drifts; if it can no longer be located unambiguously, the comment is
//! marked [`CommentStatus::Drifted`] and blocks Apply.
//!
//! All logic here is pure or filesystem-only so it is testable without a TUI;
//! the presentation layer only renders and dispatches.

pub mod apply;
pub mod selection;

pub use apply::{SendDecision, decide_send};

use std::path::PathBuf;

use tokio::fs;

use uuid::Uuid;

use crate::error::{ConfigError, Result};
use crate::git::{FileDiff, LineOrigin, ParsedDiff};
use crate::session::SessionId;

// `Comment` and its enums are network wire types (the review API serializes and
// the client deserializes them), so they live in the shared
// `claude-commander-protocol` crate. Re-exported here so the persistence,
// re-anchoring, and composition logic below — and `crate::comment::Comment`
// paths — keep working unchanged.
pub use claude_commander_protocol::comment::{ApplyOutcome, Comment, CommentSide, CommentStatus};

/// Outcome of trying to locate an comment's snippet in a fresh diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorResult {
    Located {
        side: CommentSide,
        line_range: (usize, usize),
    },
    Drifted,
}

/// Per-session comment store, persisted as one JSON array per session under
/// a directory (typically `<data_dir>/comments/`).
struct PendingComments {
    mtime: Option<std::time::SystemTime>,
    checked_at: std::time::Instant,
    sessions: std::collections::HashSet<SessionId>,
}

pub struct CommentStore {
    dir: PathBuf,
    pending: tokio::sync::Mutex<Option<PendingComments>>,
}

impl CommentStore {
    /// Construct a store rooted at `dir` (created lazily on first save).
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            pending: tokio::sync::Mutex::new(None),
        }
    }

    fn path_for(&self, sid: SessionId) -> PathBuf {
        self.dir.join(format!("{}.json", sid.as_uuid()))
    }

    /// Load a session's comments (an absent file yields an empty list).
    ///
    /// Async + `tokio::fs` so per-session disk reads never block the executor
    /// (every caller runs on the TUI/CLI runtime).
    pub async fn load(&self, sid: SessionId) -> Result<Vec<Comment>> {
        match fs::read_to_string(self.path_for(sid)).await {
            Ok(s) => {
                Ok(serde_json::from_str(&s).map_err(|e| ConfigError::LoadFailed(e.to_string()))?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(ConfigError::LoadFailed(e.to_string()).into()),
        }
    }

    /// Persist a session's comments via a temp-file + rename (atomic).
    pub async fn save(&self, sid: SessionId, anns: &[Comment]) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| ConfigError::SaveFailed(e.to_string()))?;
        let json = serde_json::to_string_pretty(anns)
            .map_err(|e| ConfigError::SaveFailed(e.to_string()))?;
        let tmp = self
            .dir
            .join(format!(".{}.{}.tmp", sid.as_uuid(), uuid::Uuid::new_v4()));
        fs::write(&tmp, json)
            .await
            .map_err(|e| ConfigError::SaveFailed(e.to_string()))?;
        fs::rename(&tmp, self.path_for(sid))
            .await
            .map_err(|e| ConfigError::SaveFailed(e.to_string()))?;
        self.invalidate_pending().await;
        Ok(())
    }

    pub async fn invalidate_pending(&self) {
        *self.pending.lock().await = None;
    }

    /// Append an comment to a session.
    pub async fn add(&self, sid: SessionId, ann: Comment) -> Result<()> {
        let mut anns = self.load(sid).await?;
        anns.push(ann);
        self.save(sid, &anns).await
    }

    /// Remove the comment with `id` from a session (no-op if absent).
    pub async fn delete(&self, sid: SessionId, id: Uuid) -> Result<()> {
        let mut anns = self.load(sid).await?;
        anns.retain(|a| a.id != id);
        self.save(sid, &anns).await
    }

    /// Session ids with at least one not-yet-applied comment. Scans the store
    /// directory (a missing directory yields an empty set); unreadable or
    /// malformed files are skipped rather than failing the whole scan.
    pub async fn sessions_with_pending(&self) -> Result<std::collections::HashSet<SessionId>> {
        let mtime = fs::metadata(&self.dir)
            .await
            .ok()
            .and_then(|m| m.modified().ok());
        let mut cached = self.pending.lock().await;
        if let Some(previous) = &*cached
            && previous.mtime == mtime
            && previous.checked_at.elapsed() < std::time::Duration::from_secs(30)
        {
            return Ok(previous.sessions.clone());
        }
        let mut out = std::collections::HashSet::new();
        let mut entries = match fs::read_dir(&self.dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(ConfigError::LoadFailed(e.to_string()).into()),
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| ConfigError::LoadFailed(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(uuid) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            let sid = SessionId::from_uuid(uuid);
            if self
                .load(sid)
                .await
                .is_ok_and(|cs| cs.iter().any(|c| c.status != CommentStatus::Applied))
            {
                out.insert(sid);
            }
        }
        *cached = Some(PendingComments {
            mtime,
            checked_at: std::time::Instant::now(),
            sessions: out.clone(),
        });
        Ok(out)
    }

    /// Set the status of one comment.
    pub async fn set_status(&self, sid: SessionId, id: Uuid, status: CommentStatus) -> Result<()> {
        let mut anns = self.load(sid).await?;
        if let Some(a) = anns.iter_mut().find(|a| a.id == id) {
            a.status = status;
        }
        self.save(sid, &anns).await
    }
}

/// Collect the `(lineno, content)` stream for `side` of a file's diff (the
/// lines that exist on that side: context + additions for New, context +
/// deletions for Old).
fn side_lines(file: &FileDiff, side: CommentSide) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    for hunk in &file.hunks {
        for line in &hunk.lines {
            let (keep, lineno) = match side {
                CommentSide::New => (
                    matches!(line.origin, LineOrigin::Context | LineOrigin::Addition),
                    line.new_lineno,
                ),
                CommentSide::Old => (
                    matches!(line.origin, LineOrigin::Context | LineOrigin::Deletion),
                    line.old_lineno,
                ),
            };
            if keep && let Some(n) = lineno {
                out.push((n, line.content.as_str()));
            }
        }
    }
    out
}

/// Try to re-locate an comment's snippet in `diff`. A single contiguous
/// match yields [`AnchorResult::Located`]; zero or multiple matches (ambiguous)
/// both yield [`AnchorResult::Drifted`]. Trailing whitespace is ignored when
/// comparing.
pub fn reanchor(ann: &Comment, diff: &ParsedDiff) -> AnchorResult {
    let Some(file) = file_for(ann, diff) else {
        return AnchorResult::Drifted;
    };

    let needle: Vec<&str> = ann.snippet.split('\n').collect();
    if needle.is_empty() {
        return AnchorResult::Drifted;
    }
    let hay = side_lines(file, ann.side);
    if hay.len() < needle.len() {
        return AnchorResult::Drifted;
    }

    let norm = str::trim_end;
    let mut matches: Vec<(usize, usize)> = Vec::new();
    for start in 0..=(hay.len() - needle.len()) {
        if (0..needle.len()).all(|i| norm(hay[start + i].1) == norm(needle[i])) {
            matches.push((hay[start].0, hay[start + needle.len() - 1].0));
        }
    }

    let located = |line_range| AnchorResult::Located {
        side: ann.side,
        line_range,
    };
    match matches.as_slice() {
        [] => AnchorResult::Drifted,
        [only] => located(*only),
        _ => {
            // The snippet occurs more than once — e.g. a short, repeated line
            // such as a parameter shared by two signatures. A plain "more than
            // one match → drifted" rule would falsely drift a comment that is
            // still sitting exactly on its line. Disambiguate by locality:
            // keep the occurrence nearest the comment's current anchor, and
            // only call it drifted when the nearest is a genuine tie.
            let target = ann.line_range.0;
            let nearest = matches.iter().map(|r| r.0.abs_diff(target)).min().unwrap();
            let mut closest = matches.iter().filter(|r| r.0.abs_diff(target) == nearest);
            match (closest.next(), closest.next()) {
                (Some(&line_range), None) => located(line_range),
                _ => AnchorResult::Drifted,
            }
        }
    }
}

/// The [`FileDiff`] a comment is anchored to, if the file is still in `diff`.
///
/// Matches the display path or either side's raw path, so a rename (which moves
/// the comment's recorded path to the diff's `old_path`) still resolves. Shared
/// by [`reanchor`] and [`prune_orphaned`] so the two can never disagree about
/// whether a comment's file is present.
fn file_for<'a>(ann: &Comment, diff: &'a ParsedDiff) -> Option<&'a FileDiff> {
    diff.files
        .iter()
        .find(|f| f.display_path() == ann.file || f.new_path == ann.file || f.old_path == ann.file)
}

/// Whether `ann`'s file has left `diff` entirely, so there is no longer any code
/// under the comment (and no file row in the review tree to reach it through).
///
/// Only meaningful for a diff whose absences are authoritative — see
/// [`ComposedDiff::absence_is_authoritative`](crate::git::ComposedDiff::absence_is_authoritative).
pub fn is_orphaned(ann: &Comment, diff: &ParsedDiff) -> bool {
    file_for(ann, diff).is_none()
}

/// Drop every non-applied comment whose file is no longer in `diff` at all,
/// returning the dropped comments.
///
/// A comment can only be reviewed, re-anchored, or deleted through a file row in
/// the review tree, and those rows come from the diff. So once the change a
/// comment was written against is reverted — the file leaves the diff entirely —
/// the comment is unreachable: it renders nowhere, `d` can't select it, yet it
/// still counts toward ancestor directory badges and blocks Apply with an
/// instruction ("review or delete them") the user has no way to follow. Dropping
/// it is the resolution; there is no longer any code under it to discuss.
///
/// Applied comments are kept — they are delivered history, not pending work.
///
/// `diff` MUST be a full diff against the session's real review base. Never
/// prune against a [`ComposedDiff::degraded_to_head`](crate::git::ComposedDiff)
/// diff: it omits committed work, so live comments would look orphaned.
pub fn prune_orphaned(anns: &mut Vec<Comment>, diff: &ParsedDiff) -> Vec<Comment> {
    let mut dropped = Vec::new();
    anns.retain(|ann| {
        let keep = ann.status == CommentStatus::Applied || !is_orphaned(ann, diff);
        if !keep {
            dropped.push(ann.clone());
        }
        keep
    });
    dropped
}

/// Re-anchor every non-applied comment against `diff` in place: located ones
/// become [`CommentStatus::Staged`] with an updated range; unlocatable ones
/// become [`CommentStatus::Drifted`]. Applied comments are left untouched.
///
/// A comment whose file has left the diff also becomes `Drifted` here (there is
/// nothing to locate it in). Callers that can prove the absence is real drop
/// those first via [`prune_orphaned`]; when the diff can't prove it, they stay —
/// `Drifted` is the safe holding state.
pub fn reanchor_comments(anns: &mut [Comment], diff: &ParsedDiff) {
    for ann in anns.iter_mut() {
        if ann.status == CommentStatus::Applied {
            continue;
        }
        match reanchor(ann, diff) {
            AnchorResult::Located { line_range, .. } => {
                ann.line_range = line_range;
                ann.status = CommentStatus::Staged;
            }
            AnchorResult::Drifted => ann.status = CommentStatus::Drifted,
        }
    }
}

/// Status-line notice for comments [`prune_orphaned`] discarded, or `None` when
/// nothing was dropped.
///
/// Dropping is the right resolution but it destroys something the user typed, so
/// it is always announced — naming the file for a single drop, since that is the
/// one case where they can act on it (re-comment once the change comes back).
pub fn dropped_notice(dropped: &[Comment]) -> Option<String> {
    match dropped {
        [] => None,
        [one] => {
            let name = one.file.rsplit('/').next().unwrap_or(&one.file);
            Some(format!("Dropped 1 comment — {name} left the diff"))
        }
        many => Some(format!(
            "Dropped {} comments — their files left the diff",
            many.len()
        )),
    }
}

/// Whether any comment is drifted (and therefore blocks Apply).
pub fn has_blocking_drift(anns: &[Comment]) -> bool {
    anns.iter().any(|a| a.status == CommentStatus::Drifted)
}

/// Markdown fence language hint for a path's extension (empty when unknown).
fn lang_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("js") | Some("jsx") => "javascript",
        Some("ts") | Some("tsx") => "typescript",
        Some("go") => "go",
        Some("toml") => "toml",
        Some("json") => "json",
        Some("md") => "markdown",
        Some("sh") | Some("bash") => "bash",
        Some("yml") | Some("yaml") => "yaml",
        _ => "",
    }
}

/// Compose a markdown brief from a set of comments, suitable for handing to
/// the agent. Deterministic (no ids or timestamps) so it is snapshot-stable.
pub fn compose_markdown(title: &str, anns: &[Comment]) -> String {
    let mut out = format!("# Review comments: {title}\n");
    for a in anns {
        let (lo, hi) = a.line_range;
        let loc = if lo == hi {
            lo.to_string()
        } else {
            format!("{lo}-{hi}")
        };
        out.push_str(&format!("\n## {}:{}\n", a.file, loc));
        out.push_str(&format!("```{}\n{}\n```\n", lang_for(&a.file), a.snippet));
        out.push_str(&format!("{}\n", a.comment));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::parse_unified_diff;
    use tempfile::TempDir;

    fn ann(file: &str, side: CommentSide, range: (usize, usize), snippet: &str) -> Comment {
        Comment::new(file, side, range, snippet, "do the thing")
    }

    // --- store ---

    #[tokio::test]
    async fn concurrent_saves_use_independent_temporary_files() {
        let dir = TempDir::new().unwrap();
        let store = std::sync::Arc::new(CommentStore::new(dir.path().join("store")));
        let sid = SessionId::new();
        let mut saves = tokio::task::JoinSet::new();
        let mut submitted = Vec::new();
        for i in 0..32 {
            let payload = {
                vec![ann(
                    &format!("file-{i}.rs"),
                    CommentSide::New,
                    (1, 1),
                    "line",
                )]
            };
            submitted.push(payload.clone());
            let store = store.clone();
            saves.spawn(async move { store.save(sid, &payload).await });
        }
        while let Some(result) = saves.join_next().await {
            result.unwrap().unwrap();
        }
        let final_value = store.load(sid).await.unwrap();
        assert!(
            submitted.contains(&final_value),
            "a complete submitted value must survive"
        );
        let files: Vec<_> = std::fs::read_dir(dir.path().join("store"))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1, "only the final JSON should remain");
    }

    #[tokio::test]
    async fn load_missing_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = CommentStore::new(tmp.path().join("comments"));
        assert!(store.load(SessionId::new()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let store = CommentStore::new(tmp.path().join("comments"));
        let sid = SessionId::new();
        let a = ann("src/foo.rs", CommentSide::New, (10, 12), "let x = 1;");
        store.add(sid, a.clone()).await.unwrap();

        let loaded = store.load(sid).await.unwrap();
        assert_eq!(loaded, vec![a]);
    }

    #[tokio::test]
    async fn delete_removes_only_target() {
        let tmp = TempDir::new().unwrap();
        let store = CommentStore::new(tmp.path().join("comments"));
        let sid = SessionId::new();
        let a = ann("a.rs", CommentSide::New, (1, 1), "a");
        let b = ann("b.rs", CommentSide::New, (2, 2), "b");
        store.add(sid, a.clone()).await.unwrap();
        store.add(sid, b.clone()).await.unwrap();

        store.delete(sid, a.id).await.unwrap();
        let loaded = store.load(sid).await.unwrap();
        assert_eq!(loaded, vec![b]);
    }

    #[tokio::test]
    async fn set_status_updates_target() {
        let tmp = TempDir::new().unwrap();
        let store = CommentStore::new(tmp.path().join("comments"));
        let sid = SessionId::new();
        let a = ann("a.rs", CommentSide::New, (1, 1), "a");
        store.add(sid, a.clone()).await.unwrap();

        store
            .set_status(sid, a.id, CommentStatus::Applied)
            .await
            .unwrap();
        assert_eq!(
            store.load(sid).await.unwrap()[0].status,
            CommentStatus::Applied
        );
    }

    #[tokio::test]
    async fn sessions_with_pending_lists_only_unapplied() {
        let tmp = TempDir::new().unwrap();
        let store = CommentStore::new(tmp.path().join("comments"));

        // Empty (missing dir) → empty set.
        assert!(store.sessions_with_pending().await.unwrap().is_empty());

        let staged = SessionId::new();
        store
            .add(staged, ann("a.rs", CommentSide::New, (1, 1), "a"))
            .await
            .unwrap();

        // A session whose only comment has been applied is not pending.
        let applied = SessionId::new();
        let a = ann("b.rs", CommentSide::New, (1, 1), "b");
        store.add(applied, a.clone()).await.unwrap();
        store
            .set_status(applied, a.id, CommentStatus::Applied)
            .await
            .unwrap();

        let pending = store.sessions_with_pending().await.unwrap();
        assert!(pending.contains(&staged));
        assert!(!pending.contains(&applied));
    }

    // --- reanchor ---

    fn diff_with_inserted_line() -> ParsedDiff {
        parse_unified_diff(
            "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,2 +1,3 @@
 fn main() {
+    let y = 3;
 }
",
        )
    }

    #[test]
    fn reanchor_single_match_relocates_range() {
        let diff = diff_with_inserted_line();
        // Stored range is stale (99); the snippet actually sits at new line 2.
        let a = ann("a.rs", CommentSide::New, (99, 99), "    let y = 3;");
        assert_eq!(
            reanchor(&a, &diff),
            AnchorResult::Located {
                side: CommentSide::New,
                line_range: (2, 2),
            }
        );
    }

    #[test]
    fn reanchor_multiline_snippet() {
        let diff = diff_with_inserted_line();
        let a = ann(
            "a.rs",
            CommentSide::New,
            (1, 2),
            "fn main() {\n    let y = 3;",
        );
        assert_eq!(
            reanchor(&a, &diff),
            AnchorResult::Located {
                side: CommentSide::New,
                line_range: (1, 2),
            }
        );
    }

    #[test]
    fn reanchor_no_match_is_drifted() {
        let diff = diff_with_inserted_line();
        let a = ann("a.rs", CommentSide::New, (2, 2), "    let z = 9;");
        assert_eq!(reanchor(&a, &diff), AnchorResult::Drifted);
    }

    #[test]
    fn reanchor_ambiguous_match_is_drifted() {
        // The snippet repeats and the comment's anchor is equidistant from two
        // occurrences (lines 1 and 3, anchored at 2): a genuine tie that
        // locality can't break, so it drifts.
        let diff = parse_unified_diff(
            "\
diff --git a/d.rs b/d.rs
--- /dev/null
+++ b/d.rs
@@ -0,0 +1,3 @@
+dup
+mid
+dup
",
        );
        let a = ann("d.rs", CommentSide::New, (2, 2), "dup");
        assert_eq!(reanchor(&a, &diff), AnchorResult::Drifted);
    }

    #[test]
    fn reanchor_repeated_snippet_keeps_nearest_anchor() {
        // Regression: a short line that legitimately repeats in the file (here
        // a `startSec` parameter shared by two signatures) must not drift a
        // comment that is sitting exactly on its line. The stored line_range
        // disambiguates to the nearest occurrence rather than flagging drift.
        let diff = parse_unified_diff(
            "\
diff --git a/x.ts b/x.ts
--- /dev/null
+++ b/x.ts
@@ -0,0 +1,6 @@
+const a = (
+  startSec: number,
+) => {}
+const b = (
+  startSec: number,
+) => {}
",
        );
        // First occurrence (new line 2) and second (new line 5) each anchor to
        // themselves, not to the other and not to drift.
        let first = ann("x.ts", CommentSide::New, (2, 2), "  startSec: number,");
        assert_eq!(
            reanchor(&first, &diff),
            AnchorResult::Located {
                side: CommentSide::New,
                line_range: (2, 2),
            }
        );
        let second = ann("x.ts", CommentSide::New, (5, 5), "  startSec: number,");
        assert_eq!(
            reanchor(&second, &diff),
            AnchorResult::Located {
                side: CommentSide::New,
                line_range: (5, 5),
            }
        );
    }

    #[test]
    fn reanchor_missing_file_is_drifted() {
        let diff = diff_with_inserted_line();
        let a = ann("other.rs", CommentSide::New, (1, 1), "fn main() {");
        assert_eq!(reanchor(&a, &diff), AnchorResult::Drifted);
    }

    #[test]
    fn reanchor_comments_updates_status_and_skips_applied() {
        let diff = diff_with_inserted_line();
        let mut located = ann("a.rs", CommentSide::New, (99, 99), "    let y = 3;");
        let mut gone = ann("a.rs", CommentSide::New, (1, 1), "    let z = 9;");
        let mut applied = ann("a.rs", CommentSide::New, (1, 1), "    let z = 9;");
        applied.status = CommentStatus::Applied;

        let mut all = vec![located.clone(), gone.clone(), applied.clone()];
        reanchor_comments(&mut all, &diff);

        located = all[0].clone();
        gone = all[1].clone();
        applied = all[2].clone();
        assert_eq!(located.status, CommentStatus::Staged);
        assert_eq!(located.line_range, (2, 2));
        assert_eq!(gone.status, CommentStatus::Drifted);
        // Applied comments are not re-evaluated.
        assert_eq!(applied.status, CommentStatus::Applied);
    }

    // --- orphan pruning ---

    /// The bug this fixes: the agent reverted the change a comment was written
    /// against, so its file left the diff entirely. Nothing renders it (file rows
    /// come from the diff), `d` can't reach it, yet it counted toward directory
    /// badges and blocked Apply forever. It must be dropped instead.
    #[test]
    fn prune_orphaned_drops_comment_whose_file_left_the_diff() {
        let diff = diff_with_inserted_line(); // touches a.rs only
        let orphan = ann("gone.rs", CommentSide::New, (1, 1), "fn vanished() {");
        let kept = ann("a.rs", CommentSide::New, (2, 2), "    let y = 3;");

        let mut all = vec![orphan.clone(), kept.clone()];
        let dropped = prune_orphaned(&mut all, &diff);

        assert_eq!(all, vec![kept]);
        assert_eq!(dropped, vec![orphan]);
    }

    /// Drift *within* a file that is still in the diff is not orphaning — those
    /// comments stay so `reanchor_comments` can mark them `Drifted`, where the
    /// review view can render the ⚠ on the file's row and the user can act.
    #[test]
    fn prune_orphaned_keeps_unlocatable_comment_while_its_file_remains() {
        let diff = diff_with_inserted_line();
        let unlocatable = ann("a.rs", CommentSide::New, (1, 1), "    let z = 9;");

        let mut all = vec![unlocatable.clone()];
        assert!(prune_orphaned(&mut all, &diff).is_empty());
        assert_eq!(all, vec![unlocatable.clone()]);

        reanchor_comments(&mut all, &diff);
        assert_eq!(all[0].status, CommentStatus::Drifted);
    }

    /// Applied comments are delivered history, not pending work: they must
    /// survive their file leaving the diff (which is the *normal* end state once
    /// the agent acts on the feedback and the code moves on).
    #[test]
    fn prune_orphaned_keeps_applied_comments() {
        let diff = diff_with_inserted_line();
        let mut applied = ann("gone.rs", CommentSide::New, (1, 1), "whatever");
        applied.status = CommentStatus::Applied;

        let mut all = vec![applied.clone()];
        assert!(prune_orphaned(&mut all, &diff).is_empty());
        assert_eq!(all, vec![applied]);
    }

    /// A rename moves the comment's recorded path onto the diff's `old_path`;
    /// the file is still under review, so the comment is not orphaned.
    #[test]
    fn prune_orphaned_keeps_comment_matched_by_rename_side() {
        let diff = parse_unified_diff(
            "\
diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
--- a/old.rs
+++ b/new.rs
@@ -1,1 +1,1 @@
-let x = 1;
+let x = 2;
",
        );
        let a = ann("old.rs", CommentSide::Old, (1, 1), "let x = 1;");

        let mut all = vec![a.clone()];
        assert!(prune_orphaned(&mut all, &diff).is_empty());
        assert_eq!(all, vec![a]);
    }

    #[test]
    fn dropped_notice_names_the_file_for_a_single_drop() {
        assert_eq!(dropped_notice(&[]), None);

        let one = ann(
            "frontend/common/src/testing/utils.testHelper.ts",
            CommentSide::New,
            (1, 1),
            "x",
        );
        assert_eq!(
            dropped_notice(std::slice::from_ref(&one)).unwrap(),
            "Dropped 1 comment — utils.testHelper.ts left the diff"
        );

        let two = vec![one.clone(), ann("b.rs", CommentSide::New, (1, 1), "y")];
        assert_eq!(
            dropped_notice(&two).unwrap(),
            "Dropped 2 comments — their files left the diff"
        );
    }

    #[test]
    fn has_blocking_drift_detects_drifted() {
        let mut a = ann("a.rs", CommentSide::New, (1, 1), "x");
        assert!(!has_blocking_drift(std::slice::from_ref(&a)));
        a.status = CommentStatus::Drifted;
        assert!(has_blocking_drift(std::slice::from_ref(&a)));
    }

    // --- markdown ---

    #[test]
    fn compose_markdown_is_deterministic() {
        let mut a = ann(
            "src/foo.rs",
            CommentSide::New,
            (10, 12),
            "let x = 1;\nlet y = 2;",
        );
        a.comment = "extract a helper".to_string();
        let mut b = ann("README.md", CommentSide::New, (3, 3), "# Title");
        b.comment = "fix the heading".to_string();

        let md = compose_markdown("my session", &[a, b]);
        let expected = "\
# Review comments: my session

## src/foo.rs:10-12
```rust
let x = 1;
let y = 2;
```
extract a helper

## README.md:3
```markdown
# Title
```
fix the heading
";
        assert_eq!(md, expected);
    }
}
