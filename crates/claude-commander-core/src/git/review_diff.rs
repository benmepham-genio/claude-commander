//! Composition of the review diff, and the git-shaped enrichment around it.
//!
//! Unlike [`super::diff`] (which produces only summary stats for the preview
//! pane), the review view needs a `file -> hunk -> line` structure with per-line
//! old/new line numbers to render gutters, drive line-range selection, and
//! anchor comments. **Parsing that structure is [`diffgrid`]'s job**; this module
//! owns everything git-shaped around it that a diff library must not know about:
//! composing the diff in the first place (`git diff`, base/merge-base
//! resolution, the untracked-file half), reading blobs, git-LFS pointer
//! reclassification, and image MIME sniffing.

use crate::git::git_command;
use std::path::Path;
use std::process::Stdio;

use tracing::warn;

use super::diff::untracked_patch_and_count;
use crate::error::{GitError, Result};

/// A composed review diff, plus whether it is the diff the caller actually
/// asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposedDiff {
    /// The raw unified diff (empty means "no changes", never "git failed" —
    /// [`compose_review_diff`] errors instead of returning an empty diff).
    pub raw: String,
    /// `true` when `base` could not be resolved and this is a working-tree-vs-
    /// `HEAD` diff instead. That is a strictly *narrower* view than requested
    /// (everything already committed on the branch is missing from it), so a
    /// file's absence proves nothing.
    pub degraded_to_head: bool,
    /// `false` when the untracked-file half of the composition was incomplete
    /// (git couldn't enumerate them, or a per-file diff failed to spawn — seen
    /// under fd pressure on noisy worktrees), so a file that really is there can
    /// be missing from `raw`.
    pub untracked_complete: bool,
}

impl ComposedDiff {
    /// Whether a file's *absence* from this diff can be trusted to mean the file
    /// has genuinely left the review.
    ///
    /// Only true for a complete diff against the base the caller asked for. Both
    /// failure modes — degrading to `HEAD`, and an incomplete untracked half —
    /// drop files that are still under review, so anything that *deletes* state
    /// keyed on a file (review comments, reviewed marks) must check this first or
    /// a transient git hiccup silently destroys the user's review.
    pub fn absence_is_authoritative(&self) -> bool {
        !self.degraded_to_head && self.untracked_complete
    }
}

/// Compose the review diff for `worktree`: everything from the merge-base of
/// `base` and `HEAD` through the working tree (committed + staged + unstaged),
/// plus untracked files — i.e. the full set of changes a PR against `base`
/// would represent, plus any uncommitted work.
///
/// `base` is a commit-ish (branch name, SHA, or `"HEAD"`). When the merge-base
/// can't be resolved (e.g. a stacked-PR base that only exists remotely), the
/// diff degrades to working-tree-vs-`HEAD` rather than failing the view, and
/// [`ComposedDiff::degraded_to_head`] says so.
///
/// When *both* the base diff and the `HEAD` fallback fail, this errors rather
/// than returning an empty diff: an empty diff is indistinguishable from "the
/// session has no changes", and callers prune per-file state (comments,
/// reviewed marks) against it, so a swallowed git failure would silently delete
/// the user's review feedback.
pub async fn compose_review_diff(worktree: &Path, base: &str) -> Result<ComposedDiff> {
    let target = diff_target(worktree, base).await;

    // Force standard `a/`/`b/` prefixes so the parser is independent of the
    // user's `diff.mnemonicPrefix` config (which would emit `i/`/`w/`/`c/`).
    let git_diff = async |rev: &str| {
        git_command()
            .current_dir(worktree)
            .args(["diff", "--src-prefix=a/", "--dst-prefix=b/", rev])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| GitError::DiffFailed(e.to_string()))
    };

    let diff_out = git_diff(&target).await?;
    let (mut diff, degraded_to_head) = if diff_out.status.success() {
        (String::from_utf8_lossy(&diff_out.stdout).to_string(), false)
    } else {
        // `base`/`target` unresolvable — degrade to working-tree-vs-HEAD.
        let head = git_diff("HEAD").await?;
        if !head.status.success() {
            // Not a resolvable-base problem: git itself can't diff this
            // worktree. Fail loudly rather than reporting "no changes".
            return Err(GitError::DiffFailed(
                String::from_utf8_lossy(&head.stderr).trim().to_string(),
            )
            .into());
        }
        (String::from_utf8_lossy(&head.stdout).to_string(), true)
    };

    let untracked = untracked_patch_and_count(worktree).await;
    if !untracked.patch.is_empty() {
        if !diff.is_empty() && !diff.ends_with('\n') {
            diff.push('\n');
        }
        diff.push_str(&untracked.patch);
    }

    Ok(ComposedDiff {
        raw: diff,
        degraded_to_head,
        untracked_complete: untracked.complete,
    })
}

/// Resolve the diff base for a PR target branch, preferring the
/// `origin/<branch>` remote-tracking ref when it exists so the review diff
/// reflects the pushed upstream rather than a possibly-stale local branch.
/// Falls back to the bare (local) branch name when no remote-tracking ref is
/// present.
pub async fn prefer_remote_branch(worktree: &Path, branch: &str) -> String {
    let remote = format!("origin/{branch}");
    if ref_exists(worktree, &remote).await {
        remote
    } else {
        branch.to_string()
    }
}

/// Resolve the review base to record for a *checked-out* branch worktree being
/// imported as a session.
///
/// A freshly created managed session records its fork point (the HEAD it was
/// branched from) as the base, so the review diff shows every commit the
/// session adds. An imported worktree, by contrast, is already sitting on the
/// branch's tip — recording that tip as the base would make
/// `merge-base(base, HEAD)` resolve to HEAD itself and the review diff come up
/// empty even when the branch is well ahead of its target.
///
/// Instead compute the genuine fork point: the merge-base of the worktree's
/// HEAD and its `default_branch` (preferring the `origin/<default_branch>`
/// remote-tracking ref). Falls back to `head` when no default branch is known
/// or no merge-base can be computed, leaving behaviour no worse than before.
pub async fn import_base_commit(
    worktree: &Path,
    head: &str,
    default_branch: Option<&str>,
) -> String {
    let Some(default_branch) = default_branch else {
        return head.to_string();
    };
    let base_ref = prefer_remote_branch(worktree, default_branch).await;
    merge_base(worktree, &base_ref)
        .await
        .unwrap_or_else(|| head.to_string())
}

/// Resolve the review base to record for a *managed* session at creation time.
///
/// A freshly generated branch is created empty off its base, so its `head` *is*
/// the fork point — record it verbatim. But a session created by *checking out*
/// a branch that already carries commits (the Checkout Branch flow, or a
/// remote branch materialised locally) sits on that branch's tip, so recording
/// `head` would make `merge-base(base, HEAD)` resolve to HEAD and the review
/// diff come up empty — the same failure [`import_base_commit`] fixes for
/// imported worktrees. When `branch_preexisted` is set, resolve the genuine
/// fork point instead.
pub async fn managed_base_commit(
    worktree: &Path,
    head: &str,
    default_branch: Option<&str>,
    branch_preexisted: bool,
) -> String {
    if branch_preexisted {
        import_base_commit(worktree, head, default_branch).await
    } else {
        head.to_string()
    }
}

/// Whether `refname` resolves to a commit in `worktree`. Exposed as
/// [`ref_resolves`] for the review-base resolver, which uses it to decide
/// whether a captured target branch is still usable before falling back to a
/// frozen SHA.
pub(crate) async fn ref_resolves(worktree: &Path, refname: &str) -> bool {
    ref_exists(worktree, refname).await
}

/// Whether `refname` resolves to a commit in `worktree`.
async fn ref_exists(worktree: &Path, refname: &str) -> bool {
    git_command()
        .current_dir(worktree)
        .args(["rev-parse", "--verify", "--quiet", refname])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The commit-ish the review diff treats as "base": the merge-base of `base`
/// and `HEAD`, falling back to `base` verbatim when it can't be computed. Both
/// the diff composition and binary blob reads resolve the old side through this
/// so they always agree on which commit the "before" image comes from.
pub(crate) async fn diff_target(worktree: &Path, base: &str) -> String {
    merge_base(worktree, base)
        .await
        .unwrap_or_else(|| base.to_string())
}

/// Read the base-side bytes of `path` as the review diff sees them: the blob at
/// [`diff_target`]. Returns an error when the path doesn't exist there (e.g. an
/// added file has no base side). Runs `git show` as a subprocess, so the read
/// happens off the async runtime's worker threads.
///
/// For git-LFS-tracked files the committed blob is a pointer, so the bytes are
/// resolved to the real content via [`super::lfs::resolve_if_pointer`] before
/// returning (best-effort: falls back to the pointer if LFS can't smudge).
pub async fn read_base_blob(worktree: &Path, base: &str, path: &str) -> Result<Vec<u8>> {
    let target = diff_target(worktree, base).await;
    let spec = format!("{target}:{path}");
    let out = git_command()
        .current_dir(worktree)
        .args(["show", &spec])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| GitError::OperationFailed(e.to_string()))?;
    if !out.status.success() {
        return Err(GitError::OperationFailed(format!(
            "git show {spec}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
        .into());
    }
    Ok(super::lfs::resolve_if_pointer(worktree, path, out.stdout).await)
}

/// Read the working-tree (new-side) bytes of `path`. Uses async `tokio::fs` so
/// the read never blocks the executor.
///
/// If the working-tree file is still an LFS pointer (LFS installed but the
/// object not pulled, or a fresh checkout), it is resolved to real bytes via
/// [`super::lfs::resolve_if_pointer`] before returning.
pub async fn read_worktree_file(worktree: &Path, path: &str) -> Result<Vec<u8>> {
    let bytes = tokio::fs::read(worktree.join(path))
        .await
        .map_err(|e| GitError::OperationFailed(e.to_string()))?;
    Ok(super::lfs::resolve_if_pointer(worktree, path, bytes).await)
}

/// `git cat-file -s <oid>` — the byte size of a blob without reading its
/// contents. `None` if the lookup fails (e.g. an absent/abbreviated oid).
async fn blob_size(worktree: &Path, oid: &str) -> Option<u64> {
    let out = git_command()
        .current_dir(worktree)
        .args(["cat-file", "-s", oid])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Fill in the binary file sizes the parser leaves `None`: the base blob size
/// via `git cat-file -s`, and the new-side size from the working-tree file on
/// disk. Best effort — a size stays `None` if its lookup fails. Sizes the
/// parser already resolved (e.g. an LFS pointer's `size` line, which is correct
/// where `git cat-file -s` would report only the ~130-byte pointer blob) are
/// left untouched.
pub async fn enrich_binary_sizes(diff: &mut ParsedDiff, worktree: &Path) {
    for f in &mut diff.files {
        let new_path = f.new_path.clone();
        let Some(info) = f.binary.as_mut() else {
            continue;
        };
        if info.old_size.is_none()
            && let Some(oid) = info.old_oid.clone()
        {
            info.old_size = blob_size(worktree, &oid).await;
        }
        if info.new_size.is_none() && info.new_oid.is_some() {
            // The new side is the working tree, so its size is the file on disk.
            info.new_size = tokio::fs::metadata(worktree.join(&new_path))
                .await
                .ok()
                .map(|m| m.len());
        }
    }
}

/// Resolve `git merge-base <base> HEAD`, returning `None` if it cannot be
/// computed (so the caller can fall back).
async fn merge_base(worktree: &Path, base: &str) -> Option<String> {
    let out = git_command()
        .current_dir(worktree)
        .args(["merge-base", base, "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

// The structured diff model is the network wire shape, so it lives in the
// shared `claude-commander-protocol` crate (`Serialize + Deserialize`, no git
// deps, mobile-safe). Re-exported here so existing `crate::git::{FileDiff,
// Hunk, ...}` paths — and the parser/hashing logic below, which operate on
// these types — keep working unchanged.
pub use claude_commander_protocol::diff::{
    BinaryInfo, BinaryKind, DiffLine, FileDiff, FileStatus, Hunk, LineOrigin, ParsedDiff,
};

/// The path git writes for the side a file does not exist on. `diffgrid`
/// resolves both sides to the real path (an added file's "old path" is the path
/// git *would* have used), but the wire model predates that and consumers —
/// including the Flutter client — key off this sentinel, so it is restored here.
pub(crate) const DEV_NULL: &str = "/dev/null";

/// Parse a unified diff (the output of `git diff`) into a [`ParsedDiff`].
///
/// The parse itself is [`diffgrid`]'s; this maps its model onto the wire shape in
/// `claude-commander-protocol` and layers on the two git-specific
/// reclassifications a diff library has no business knowing about: git-LFS
/// pointers (which git diffs as *text*) become binary, and a binary file's
/// extension picks an image MIME type.
///
/// Total, like the parser beneath it: entries `diffgrid` cannot represent are
/// logged and omitted rather than aborting the parse, so one malformed file
/// never costs the user the rest of the review.
pub fn parse_unified_diff(raw: &str) -> ParsedDiff {
    let (files, warnings) = diffgrid::parse(raw, &diffgrid::ParseOptions::default());
    for w in &warnings {
        // Warn rather than swallow: every one of these means a file the user
        // changed is missing from (or degraded in) the review they are looking
        // at, and the old hand-rolled parser dropped them silently.
        warn!(
            path = w.path.as_deref().unwrap_or("?"),
            byte_offset = w.byte_offset,
            "review diff: {:?}",
            w.kind
        );
    }
    ParsedDiff {
        files: files.iter().map(convert_file).collect(),
    }
}

/// Map one [`diffgrid::FileDiff`] onto the wire [`FileDiff`].
fn convert_file(f: &diffgrid::FileDiff<'_>) -> FileDiff {
    let status = match f.status {
        diffgrid::FileStatus::Added => FileStatus::Added,
        diffgrid::FileStatus::Deleted => FileStatus::Deleted,
        diffgrid::FileStatus::Modified => FileStatus::Modified,
        // A copy has the same observable shape as a rename here — two distinct
        // paths, both sides on disk, so context expansion works — and the wire
        // enum has no `Copied` variant to add without breaking older clients.
        // git only emits `copy from`/`copy to` under `-C`, which we never pass.
        diffgrid::FileStatus::Renamed | diffgrid::FileStatus::Copied => FileStatus::Renamed,
        // `FileStatus` is `#[non_exhaustive]`: a variant added upstream renders
        // as a plain modification rather than failing to compile the day it lands.
        _ => FileStatus::Modified,
    };
    let hunks: Vec<Hunk> = f.hunks.iter().map(convert_hunk).collect();

    // git diffs an LFS-tracked file as a textual diff of its pointer (never
    // "Binary files differ"), so a textual diff whose content is an LFS pointer
    // is really a binary change. Reclassify it as binary — but keep the hunks,
    // since the reviewed-mark hash (`claude_commander_viewmodel::review::file_diff_hash`) keys marks off them and the pointer's
    // `oid` lines make that hash invalidate on re-upload.
    let lfs_sizes = (!f.is_binary())
        .then(|| lfs_pointer_sizes(&hunks))
        .flatten();
    let display = if status == FileStatus::Deleted {
        f.old_path.as_ref()
    } else {
        f.new_path.as_ref()
    };
    let binary = (f.is_binary() || lfs_sizes.is_some()).then(|| {
        let kind = match image_mime_for_path(display) {
            Some(mime) => BinaryKind::Image {
                mime: mime.to_string(),
            },
            None => BinaryKind::Other,
        };
        let (old_size, new_size) = lfs_sizes.unwrap_or((None, None));
        BinaryInfo {
            kind,
            old_oid: zero_oid_to_none(f.old_oid.as_deref()),
            new_oid: zero_oid_to_none(f.new_oid.as_deref()),
            old_size,
            new_size,
        }
    });

    FileDiff {
        old_path: side_or_dev_null(&f.old_path, status == FileStatus::Added),
        new_path: side_or_dev_null(&f.new_path, status == FileStatus::Deleted),
        status,
        added: f.added(),
        removed: f.removed(),
        // A binary file's "hunks" are only ever the LFS pointer's text; a real
        // binary patch has none.
        hunks: if f.is_binary() { Vec::new() } else { hunks },
        binary,
    }
}

/// The wire spelling of one side's path: [`DEV_NULL`] on the side the file does
/// not exist on, else the real path.
fn side_or_dev_null(path: &str, absent: bool) -> String {
    if absent {
        DEV_NULL.to_string()
    } else {
        path.to_string()
    }
}

/// Map one [`diffgrid::Hunk`] onto the wire [`Hunk`].
fn convert_hunk(h: &diffgrid::Hunk<'_>) -> Hunk {
    Hunk {
        old_start: h.old_start,
        old_lines: h.old_lines,
        new_start: h.new_start,
        new_lines: h.new_lines,
        header: h.section.to_string(),
        lines: h
            .lines
            .iter()
            .map(|l| DiffLine {
                origin: match l.origin {
                    diffgrid::LineOrigin::Context => LineOrigin::Context,
                    diffgrid::LineOrigin::Addition => LineOrigin::Addition,
                    diffgrid::LineOrigin::Deletion => LineOrigin::Deletion,
                    // `LineOrigin` is `#[non_exhaustive]`; anything new reads as
                    // unchanged rather than breaking the build.
                    _ => LineOrigin::Context,
                },
                old_lineno: l.old_lineno.map(diffgrid::LineNo::get),
                new_lineno: l.new_lineno.map(diffgrid::LineNo::get),
                content: l.content.to_string(),
            })
            .collect(),
    }
}

/// Reconstruct one side of a file from its hunks: the new side keeps context +
/// additions, the old side keeps context + deletions. Used to recover an LFS
/// pointer's text from a diff so it can be recognised as binary.
///
/// Bails out as soon as the output exceeds the LFS pointer-size cap: past that
/// point `is_lfs_pointer` rejects it anyway, so for a large modified text file
/// (thousands of diff lines) we avoid building a near-full-size `String` only
/// to discard it. The returned (truncated) string is still `> MAX_POINTER_LEN`,
/// so the caller's pointer check fails exactly as it would on the full string.
fn reconstruct_side(hunks: &[Hunk], want_new: bool) -> String {
    let mut out = String::new();
    for hunk in hunks {
        for line in &hunk.lines {
            let keep = match line.origin {
                LineOrigin::Context => true,
                LineOrigin::Addition => want_new,
                LineOrigin::Deletion => !want_new,
            };
            if keep {
                out.push_str(&line.content);
                out.push('\n');
                if out.len() > super::lfs::MAX_POINTER_LEN {
                    return out;
                }
            }
        }
    }
    out
}

/// The `size <n>` value from an LFS pointer (the real blob size LFS records),
/// or `None` if absent/unparseable.
fn lfs_pointer_size(pointer: &str) -> Option<u64> {
    pointer
        .lines()
        .find_map(|l| l.strip_prefix("size "))
        .and_then(|n| n.trim().parse().ok())
}

/// If a file's hunks reconstruct to a git-LFS pointer on either side, return the
/// `(old, new)` real blob sizes parsed from the pointer `size` lines (each
/// `None` on the side that isn't a pointer, e.g. an added or deleted file).
/// Returns `None` when neither side is a pointer. This is how an LFS image —
/// which git diffs as pointer text — is recognised as a binary change.
fn lfs_pointer_sizes(hunks: &[Hunk]) -> Option<(Option<u64>, Option<u64>)> {
    if hunks.is_empty() {
        return None;
    }
    let old = reconstruct_side(hunks, false);
    let new = reconstruct_side(hunks, true);
    let old_ptr = super::lfs::is_lfs_pointer(old.as_bytes());
    let new_ptr = super::lfs::is_lfs_pointer(new.as_bytes());
    if !old_ptr && !new_ptr {
        return None;
    }
    Some((
        old_ptr.then(|| lfs_pointer_size(&old)).flatten(),
        new_ptr.then(|| lfs_pointer_size(&new)).flatten(),
    ))
}

/// Map an all-zero blob oid (git's "absent" sentinel for the missing side of an
/// add or delete) to `None`; pass any real oid through unchanged.
fn zero_oid_to_none(oid: Option<&str>) -> Option<String> {
    oid.filter(|o| !o.is_empty() && o.bytes().any(|c| c != b'0'))
        .map(str::to_owned)
}

/// MIME type for a path's extension when it names a raster image we can render,
/// else `None` (treated as a generic binary). SVG is intentionally excluded:
/// git diffs it as text, and rasterizing it would need an extra dependency.
fn image_mime_for_path(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "tif" | "tiff" => "image/tiff",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::fixture_git;

    #[test]
    fn parses_empty_to_empty() {
        let d = parse_unified_diff("");
        assert!(d.is_empty());
        assert_eq!(d, ParsedDiff::default());
    }

    #[test]
    fn tracks_old_and_new_line_numbers_across_two_hunks() {
        let raw = "\
diff --git a/src/foo.rs b/src/foo.rs
index 1111111..2222222 100644
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,3 +1,4 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    println!(\"added\");
 }
@@ -10,2 +11,2 @@ fn other() {
 let x = 1;
-let y = 2;
+let y = 3;
";
        let d = parse_unified_diff(raw);
        assert_eq!(d.files.len(), 1);
        let f = &d.files[0];
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!(f.old_path, "src/foo.rs");
        assert_eq!(f.new_path, "src/foo.rs");
        assert_eq!(f.added, 3);
        assert_eq!(f.removed, 2);
        assert_eq!(f.hunks.len(), 2);

        let h0 = &f.hunks[0];
        assert_eq!((h0.old_start, h0.new_start), (1, 1));
        // " fn main() {" context, old 1 / new 1
        assert_eq!(h0.lines[0].origin, LineOrigin::Context);
        assert_eq!(h0.lines[0].old_lineno, Some(1));
        assert_eq!(h0.lines[0].new_lineno, Some(1));
        assert_eq!(h0.lines[0].content, "fn main() {");
        // "-    println!(\"old\");" deletion, old 2 / new None
        assert_eq!(h0.lines[1].origin, LineOrigin::Deletion);
        assert_eq!(h0.lines[1].old_lineno, Some(2));
        assert_eq!(h0.lines[1].new_lineno, None);
        // "+    println!(\"new\");" addition, old None / new 2
        assert_eq!(h0.lines[2].origin, LineOrigin::Addition);
        assert_eq!(h0.lines[2].old_lineno, None);
        assert_eq!(h0.lines[2].new_lineno, Some(2));
        // "+    println!(\"added\");" addition, new 3
        assert_eq!(h0.lines[3].new_lineno, Some(3));
        // " }" context, old 3 / new 4
        assert_eq!(h0.lines[4].origin, LineOrigin::Context);
        assert_eq!(h0.lines[4].old_lineno, Some(3));
        assert_eq!(h0.lines[4].new_lineno, Some(4));

        let h1 = &f.hunks[1];
        assert_eq!((h1.old_start, h1.new_start), (10, 11));
        assert_eq!(h1.header, "fn other() {");
        assert_eq!(h1.lines[0].old_lineno, Some(10));
        assert_eq!(h1.lines[0].new_lineno, Some(11));
        assert_eq!(h1.lines[1].origin, LineOrigin::Deletion);
        assert_eq!(h1.lines[1].old_lineno, Some(11));
        assert_eq!(h1.lines[2].origin, LineOrigin::Addition);
        assert_eq!(h1.lines[2].new_lineno, Some(12));
    }

    #[test]
    fn parses_multiple_files() {
        let raw = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-a
+A
diff --git a/b.txt b/b.txt
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-b
+B
";
        let d = parse_unified_diff(raw);
        assert_eq!(d.files.len(), 2);
        assert_eq!(d.files[0].new_path, "a.txt");
        assert_eq!(d.files[1].new_path, "b.txt");
        // count omitted in "@@ -1 +1 @@" defaults to 1 line each side
        assert_eq!(d.files[0].hunks[0].old_lines, 1);
        assert_eq!(d.files[0].hunks[0].new_lines, 1);
    }

    #[test]
    fn detects_added_file_via_dev_null() {
        let raw = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
";
        let d = parse_unified_diff(raw);
        let f = &d.files[0];
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.old_path, "/dev/null");
        assert_eq!(f.new_path, "new.txt");
        assert_eq!(f.display_path(), "new.txt");
        assert_eq!(f.added, 2);
        assert_eq!(f.removed, 0);
    }

    #[test]
    fn detects_deleted_file() {
        let raw = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 1111111..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        let d = parse_unified_diff(raw);
        let f = &d.files[0];
        assert_eq!(f.status, FileStatus::Deleted);
        assert_eq!(f.new_path, "/dev/null");
        // deletions display the old path
        assert_eq!(f.display_path(), "gone.txt");
        assert_eq!(f.removed, 1);
    }

    /// A copy (`git diff -C`) has two distinct paths with both sides on disk,
    /// which the wire enum spells `Renamed`. The hand-rolled parser did not
    /// recognise `copy from`/`copy to` at all and reported these as plain
    /// modifications of the destination.
    #[test]
    fn detects_copy_as_a_two_path_change() {
        let raw = "\
diff --git a/src.txt b/dst.txt
similarity index 90%
copy from src.txt
copy to dst.txt
--- a/src.txt
+++ b/dst.txt
@@ -1 +1 @@
-a
+b
";
        let f = &parse_unified_diff(raw).files[0];
        assert_eq!(f.status, FileStatus::Renamed);
        assert_eq!(f.old_path, "src.txt");
        assert_eq!(f.new_path, "dst.txt");
        assert_eq!(f.display_path(), "dst.txt");
    }

    /// A conflicted working tree makes `git diff` emit combined (`diff --cc`)
    /// entries. They have no shape in this model, so they are dropped — but the
    /// rest of the diff must still parse, rather than the `--cc` body being
    /// mistaken for the previous file's content.
    #[test]
    fn combined_diff_is_dropped_without_swallowing_neighbours() {
        let raw = "\
diff --git a/before.txt b/before.txt
--- a/before.txt
+++ b/before.txt
@@ -1 +1 @@
-a
+b
diff --cc merged.txt
index 1111111,2222222..3333333
--- a/merged.txt
+++ b/merged.txt
@@@ -1,1 -1,1 +1,1 @@@
- ours
 -theirs
++resolved
diff --git a/after.txt b/after.txt
--- a/after.txt
+++ b/after.txt
@@ -1 +1 @@
-c
+d
";
        let d = parse_unified_diff(raw);
        let paths: Vec<&str> = d.files.iter().map(FileDiff::display_path).collect();
        assert_eq!(paths, ["before.txt", "after.txt"]);
        // The neighbours are intact — no `--cc` body leaked into either.
        assert_eq!(d.files[0].hunks[0].lines.len(), 2);
        assert_eq!(d.files[1].hunks[0].lines.len(), 2);
    }

    /// A CRLF file's lines must not carry a stray `\r` into the rendered body
    /// (or into a comment snippet, which is matched against them verbatim). The
    /// hand-rolled parser kept it, because `str::lines()` strips `\n` only when
    /// it splits — and it split on `\n`.
    #[test]
    fn crlf_content_is_normalised() {
        // git frames the diff with LF and lets the file's own `\r` ride on the
        // content lines, which is exactly where the stray carriage return came
        // from.
        let raw = "\
diff --git a/w.txt b/w.txt
--- a/w.txt
+++ b/w.txt
@@ -1 +1 @@
-old\r
+new\r
";
        let f = &parse_unified_diff(raw).files[0];
        assert_eq!(f.new_path, "w.txt");
        assert_eq!(f.hunks[0].lines[0].content, "old");
        assert_eq!(f.hunks[0].lines[1].content, "new");
    }

    /// `core.quotePath` (on by default) makes git C-quote any path with
    /// non-ASCII bytes. The display path must be the real name, or the file
    /// list shows octal escapes and reviewed marks key off a path that never
    /// matches the working tree.
    #[test]
    fn quoted_non_ascii_path_is_decoded() {
        let raw = "\
diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"
--- \"a/caf\\303\\251.txt\"
+++ \"b/caf\\303\\251.txt\"
@@ -1 +1 @@
-a
+b
";
        let f = &parse_unified_diff(raw).files[0];
        assert_eq!(f.display_path(), "café.txt");
    }

    #[test]
    fn detects_rename_without_hunks() {
        let raw = "\
diff --git a/old.txt b/new.txt
similarity index 100%
rename from old.txt
rename to new.txt
";
        let d = parse_unified_diff(raw);
        let f = &d.files[0];
        assert_eq!(f.status, FileStatus::Renamed);
        assert_eq!(f.old_path, "old.txt");
        assert_eq!(f.new_path, "new.txt");
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn modified_file_display_path_is_new_path() {
        let raw = "\
diff --git a/keep.txt b/keep.txt
--- a/keep.txt
+++ b/keep.txt
@@ -1 +1 @@
-x
+y
";
        let f = &parse_unified_diff(raw).files[0];
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!(f.display_path(), "keep.txt");
    }

    #[test]
    fn ignores_no_newline_marker() {
        let raw = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        let f = &parse_unified_diff(raw).files[0];
        assert_eq!(f.added, 1);
        assert_eq!(f.removed, 1);
        // the marker lines must not appear as content
        assert_eq!(f.hunks[0].lines.len(), 2);
    }

    // --- binary files ---

    #[test]
    fn text_file_is_not_binary() {
        let raw = one_file_diff("src/a.rs", "-1,2 +1,2", " ctx\n-old\n+new\n");
        let f = &parse_unified_diff(&raw).files[0];
        assert_eq!(f.binary, None);
    }

    #[test]
    fn parses_binary_modified_image() {
        let raw = "\
diff --git a/assets/logo.png b/assets/logo.png
index 1111111..2222222 100644
Binary files a/assets/logo.png and b/assets/logo.png differ
";
        let f = &parse_unified_diff(raw).files[0];
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!(f.display_path(), "assets/logo.png");
        let b = f.binary.as_ref().expect("binary info present");
        assert_eq!(
            b.kind,
            BinaryKind::Image {
                mime: "image/png".to_string()
            }
        );
        assert_eq!(b.old_oid.as_deref(), Some("1111111"));
        assert_eq!(b.new_oid.as_deref(), Some("2222222"));
        // Sizes are enriched by open_review, not the parser.
        assert_eq!(b.old_size, None);
        assert_eq!(b.new_size, None);
        // Binary files carry no textual hunks.
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn parses_binary_added_image_with_no_old_side() {
        let raw = "\
diff --git a/new.jpg b/new.jpg
new file mode 100644
index 0000000..3333333
Binary files /dev/null and b/new.jpg differ
";
        let f = &parse_unified_diff(raw).files[0];
        assert_eq!(f.status, FileStatus::Added);
        let b = f.binary.as_ref().expect("binary info present");
        assert_eq!(
            b.kind,
            BinaryKind::Image {
                mime: "image/jpeg".to_string()
            }
        );
        // All-zero base oid is the "absent side" sentinel -> None.
        assert_eq!(b.old_oid, None);
        assert_eq!(b.new_oid.as_deref(), Some("3333333"));
    }

    #[test]
    fn parses_binary_deleted_image_with_no_new_side() {
        let raw = "\
diff --git a/old.gif b/old.gif
deleted file mode 100644
index 4444444..0000000
Binary files a/old.gif and /dev/null differ
";
        let f = &parse_unified_diff(raw).files[0];
        assert_eq!(f.status, FileStatus::Deleted);
        // Deletions resolve kind from the old (display) path.
        assert_eq!(f.display_path(), "old.gif");
        let b = f.binary.as_ref().expect("binary info present");
        assert_eq!(
            b.kind,
            BinaryKind::Image {
                mime: "image/gif".to_string()
            }
        );
        assert_eq!(b.old_oid.as_deref(), Some("4444444"));
        assert_eq!(b.new_oid, None);
    }

    #[test]
    fn non_image_binary_is_kind_other() {
        let raw = "\
diff --git a/data.bin b/data.bin
index 5555555..6666666 100644
Binary files a/data.bin and b/data.bin differ
";
        let f = &parse_unified_diff(raw).files[0];
        let b = f.binary.as_ref().expect("binary info present");
        assert_eq!(b.kind, BinaryKind::Other);
    }

    // --- git-LFS pointer diffs (rendered as text by git, reclassified binary) ---

    #[test]
    fn lfs_pointer_modify_is_reclassified_binary() {
        // git diffs an LFS-tracked file as a textual diff of the pointer, never
        // "Binary files differ". The parser must still surface it as binary so
        // the image render + smudge path handles it.
        let raw = "\
diff --git a/img.png b/img.png
index 1111111..2222222 100644
--- a/img.png
+++ b/img.png
@@ -1,3 +1,3 @@
 version https://git-lfs.github.com/spec/v1
-oid sha256:930e747ddba87c999aca665444a5cf1f2430572b5d082ffc5f43fa30b303427c
-size 10880
+oid sha256:ab65d9a6269a47be4c6ab439904ca1c889c19f91b78011951cfee0bab401b373
+size 23676
";
        let f = &parse_unified_diff(raw).files[0];
        assert_eq!(f.status, FileStatus::Modified);
        let b = f.binary.as_ref().expect("LFS pointer must be binary");
        assert_eq!(
            b.kind,
            BinaryKind::Image {
                mime: "image/png".to_string()
            }
        );
        // Sizes come from the pointer `size` lines, not git cat-file (which
        // would report the ~130-byte pointer blob).
        assert_eq!(b.old_size, Some(10880));
        assert_eq!(b.new_size, Some(23676));
        // Hunks are kept so the reviewed-mark hash stays content-sensitive (the oid
        // lines change when the image is re-uploaded), not collapsed to empty.
        assert!(!f.hunks.is_empty());
    }

    #[test]
    fn lfs_pointer_add_has_no_old_side() {
        let raw = "\
diff --git a/new.png b/new.png
new file mode 100644
index 0000000..2222222
--- /dev/null
+++ b/new.png
@@ -0,0 +1,3 @@
+version https://git-lfs.github.com/spec/v1
+oid sha256:ab65d9a6269a47be4c6ab439904ca1c889c19f91b78011951cfee0bab401b373
+size 23676
";
        let f = &parse_unified_diff(raw).files[0];
        assert_eq!(f.status, FileStatus::Added);
        let b = f.binary.as_ref().expect("added LFS pointer must be binary");
        assert_eq!(b.old_size, None);
        assert_eq!(b.new_size, Some(23676));
    }

    #[test]
    fn text_diff_resembling_pointer_is_not_reclassified() {
        // A real text file whose content merely mentions "version ..." must not
        // be mistaken for an LFS pointer.
        let raw = one_file_diff(
            "notes.txt",
            "-1,2 +1,2",
            " version https://git-lfs.github.com/spec/v1\n-old note\n+new note\n",
        );
        let f = &parse_unified_diff(&raw).files[0];
        assert_eq!(f.binary, None);
    }

    // --- composition (real git repos via TempDir) ---

    use std::fs;
    // Mode bits, for the "git can enumerate it but not read it" case. Unix-only,
    // like the rest of this module (it shells out to `/dev/null`).
    use std::os::unix::fs::PermissionsExt;
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

    async fn git_capture(dir: &Path, args: &[&str]) -> String {
        let out = fixture_git()
            .current_dir(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .expect("git command runs");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    async fn init_repo() -> TempDir {
        let tmp = TempDir::new().expect("tempdir");
        let p = tmp.path();
        git(p, &["init", "-q"]).await;
        git(p, &["config", "user.email", "test@example.com"]).await;
        git(p, &["config", "user.name", "Test"]).await;
        tmp
    }

    #[tokio::test]
    async fn includes_committed_change_against_base() {
        let tmp = init_repo().await;
        let p = tmp.path();

        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        let base = git_capture(p, &["rev-parse", "HEAD"]).await;

        // A committed change on top of the base. `git diff HEAD` would be empty.
        fs::write(p.join("file.txt"), "v2\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "B"]).await;

        let diff = compose_review_diff(p, &base).await.unwrap().raw;
        assert!(diff.contains("-v1"), "committed deletion missing:\n{diff}");
        assert!(diff.contains("+v2"), "committed addition missing:\n{diff}");

        // Sanity: the committed change is invisible to a plain working-tree diff.
        let head = git_capture(p, &["diff", "HEAD"]).await;
        assert!(head.is_empty(), "working tree should be clean: {head}");

        // And it round-trips through the parser.
        let parsed = parse_unified_diff(&diff);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].display_path(), "file.txt");
    }

    #[tokio::test]
    async fn binary_image_change_survives_composition_and_parsing() {
        let tmp = init_repo().await;
        let p = tmp.path();

        // Bytes with embedded NULs so git classifies the file as binary.
        let v1: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00v1\x00";
        let v2: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00v2-longer\x00";
        fs::write(p.join("logo.png"), v1).unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        let base = git_capture(p, &["rev-parse", "HEAD"]).await;

        fs::write(p.join("logo.png"), v2).unwrap();

        let diff = compose_review_diff(p, &base).await.unwrap().raw;
        let parsed = parse_unified_diff(&diff);
        let f = parsed
            .files
            .iter()
            .find(|f| f.display_path() == "logo.png")
            .expect("binary file present in parsed diff");
        let info = f.binary.as_ref().expect("binary info present");
        assert_eq!(
            info.kind,
            BinaryKind::Image {
                mime: "image/png".to_string()
            }
        );
        assert!(f.hunks.is_empty(), "binary file has no textual hunks");
    }

    /// Whether `git lfs` is installed, so LFS-dependent tests can skip cleanly
    /// (mirrors the tmux-guarded integration tests).
    async fn git_lfs_available() -> bool {
        fixture_git()
            .args(["lfs", "version"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn lfs_image_change_is_binary_end_to_end() {
        if !git_lfs_available().await {
            eprintln!("Skipping test: git-lfs not available");
            return;
        }
        let tmp = init_repo().await;
        let p = tmp.path();

        // Distinct LFS-tracked PNGs of different lengths so the pointer `size`
        // lines differ. git stores these as pointers and diffs them as text.
        let v1: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00v1\x00";
        let v2: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00v2-is-longer\x00";

        git(p, &["lfs", "install", "--local"]).await;
        git(p, &["lfs", "track", "*.png"]).await;
        fs::write(p.join("logo.png"), v1).unwrap();
        git(p, &["add", ".gitattributes", "logo.png"]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        let base = git_capture(p, &["rev-parse", "HEAD"]).await;

        fs::write(p.join("logo.png"), v2).unwrap();

        // git diffs the LFS file as pointer *text*, yet it must surface as binary.
        let diff = compose_review_diff(p, &base).await.unwrap().raw;
        assert!(
            diff.contains("version https://git-lfs.github.com/spec/"),
            "expected a textual pointer diff from git:\n{diff}"
        );
        let parsed = parse_unified_diff(&diff);
        let f = parsed
            .files
            .iter()
            .find(|f| f.display_path() == "logo.png")
            .expect("logo.png present in parsed diff");
        let info = f.binary.as_ref().expect("LFS image must be binary");
        assert_eq!(
            info.kind,
            BinaryKind::Image {
                mime: "image/png".to_string()
            }
        );
        // Sizes come from the pointer `size` lines = the real object sizes.
        assert_eq!(info.old_size, Some(v1.len() as u64));
        assert_eq!(info.new_size, Some(v2.len() as u64));

        // The full loop: read_base_blob smudges the base pointer back to v1.
        let old = read_base_blob(p, &base, "logo.png").await.unwrap();
        assert_eq!(old, v1, "base side should smudge to the real image bytes");
    }

    #[tokio::test]
    async fn read_base_blob_returns_committed_bytes_not_worktree() {
        let tmp = init_repo().await;
        let p = tmp.path();
        let v1: &[u8] = b"\x00base-bytes\x00";
        let v2: &[u8] = b"\x00worktree-bytes-changed\x00";
        fs::write(p.join("img.png"), v1).unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        let base = git_capture(p, &["rev-parse", "HEAD"]).await;
        fs::write(p.join("img.png"), v2).unwrap();

        // Old side reads the committed blob; new side reads the working tree.
        let old = read_base_blob(p, &base, "img.png").await.unwrap();
        assert_eq!(old, v1);
        let new = read_worktree_file(p, "img.png").await.unwrap();
        assert_eq!(new, v2);
    }

    #[tokio::test]
    async fn read_base_blob_errors_for_file_absent_at_base() {
        let tmp = init_repo().await;
        let p = tmp.path();
        fs::write(p.join("seed.txt"), "x\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        let base = git_capture(p, &["rev-parse", "HEAD"]).await;

        // An added file has no base side -> error rather than bogus bytes.
        fs::write(p.join("added.png"), b"\x00new\x00").unwrap();
        assert!(read_base_blob(p, &base, "added.png").await.is_err());
    }

    #[tokio::test]
    async fn enrich_binary_sizes_fills_old_and_new() {
        let tmp = init_repo().await;
        let p = tmp.path();
        let v1: &[u8] = b"\x00abc\x00"; // 5 bytes
        let v2: &[u8] = b"\x00abcdefgh\x00"; // 10 bytes
        fs::write(p.join("logo.png"), v1).unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        let base = git_capture(p, &["rev-parse", "HEAD"]).await;
        fs::write(p.join("logo.png"), v2).unwrap();

        let raw = compose_review_diff(p, &base).await.unwrap().raw;
        let mut diff = parse_unified_diff(&raw);
        enrich_binary_sizes(&mut diff, p).await;
        let f = diff
            .files
            .iter()
            .find(|f| f.display_path() == "logo.png")
            .unwrap();
        let b = f.binary.as_ref().unwrap();
        assert_eq!(b.old_size, Some(v1.len() as u64));
        assert_eq!(b.new_size, Some(v2.len() as u64));
    }

    #[tokio::test]
    async fn includes_unstaged_and_untracked_against_base() {
        let tmp = init_repo().await;
        let p = tmp.path();

        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        let base = git_capture(p, &["rev-parse", "HEAD"]).await;

        // Unstaged modification + untracked file (neither committed).
        fs::write(p.join("file.txt"), "v3\n").unwrap();
        fs::write(p.join("untracked.txt"), "u\n").unwrap();

        let diff = compose_review_diff(p, &base).await.unwrap().raw;
        assert!(diff.contains("+v3"), "unstaged change missing:\n{diff}");
        assert!(
            diff.contains("untracked.txt") && diff.contains("+u"),
            "untracked file missing:\n{diff}"
        );
    }

    #[tokio::test]
    async fn empty_when_clean_against_head() {
        let tmp = init_repo().await;
        let p = tmp.path();
        fs::write(p.join("file.txt"), "x\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;

        let diff = compose_review_diff(p, "HEAD").await.unwrap().raw;
        assert!(diff.is_empty(), "expected empty diff, got:\n{diff}");
        assert!(parse_unified_diff(&diff).is_empty());
    }

    #[tokio::test]
    async fn degrades_to_head_when_base_unresolvable() {
        let tmp = init_repo().await;
        let p = tmp.path();
        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;

        // Unstaged change present; base is a branch that does not exist.
        fs::write(p.join("file.txt"), "v2\n").unwrap();

        let composed = compose_review_diff(p, "no-such-branch").await.unwrap();
        // Falls back to working-tree-vs-HEAD, which still shows the change...
        assert!(
            composed.raw.contains("+v2"),
            "fallback diff missing change:\n{}",
            composed.raw
        );
        // ...and flags itself as the narrower view, so callers know a file's
        // absence here does not mean it left the review.
        assert!(composed.degraded_to_head);
    }

    #[tokio::test]
    async fn resolvable_base_is_not_marked_degraded() {
        let tmp = init_repo().await;
        let p = tmp.path();
        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        fs::write(p.join("file.txt"), "v2\n").unwrap();

        let composed = compose_review_diff(p, "HEAD").await.unwrap();
        assert!(composed.raw.contains("+v2"));
        assert!(!composed.degraded_to_head);
    }

    /// A git failure must not masquerade as "no changes". Callers prune
    /// comments and reviewed marks against the composed diff, so returning an
    /// empty diff here would silently delete the user's review feedback.
    #[tokio::test]
    async fn errors_when_not_a_git_worktree_instead_of_reporting_no_changes() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A `.git` *file* pointing nowhere makes git fail right here instead of
        // walking up: without it, an OS temp dir that happened to sit inside a
        // repo would make the diff succeed and this test pass vacuously. Done
        // with a file rather than `GIT_CEILING_DIRECTORIES` because `set_var` is
        // unsound while sibling tests spawn subprocesses on other threads.
        fs::write(tmp.path().join(".git"), "gitdir: /nonexistent\n").unwrap();

        let err = compose_review_diff(tmp.path(), "main").await;
        assert!(
            err.is_err(),
            "expected an error for a non-repo worktree, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn untracked_failure_makes_absence_non_authoritative() {
        let tmp = init_repo().await;
        let p = tmp.path();
        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;

        // A healthy composition can be trusted to prove a file's absence...
        let composed = compose_review_diff(p, "HEAD").await.unwrap();
        assert!(composed.untracked_complete);
        assert!(composed.absence_is_authoritative());

        // ...but an untracked half that couldn't even be enumerated cannot: the
        // files it would have contributed are missing from `raw` yet present on
        // disk, which is indistinguishable from "reverted" to a pruning caller.
        let patch = crate::git::diff::untracked_patch_and_count(Path::new(
            "/definitely/not/a/worktree/anywhere",
        ))
        .await;
        assert!(!patch.complete);

        // An untracked file git can enumerate but not read is the nastier case:
        // `git diff --no-index` spawns fine and exits 128 ("cannot hash") with an
        // empty stdout, so the file is absent from a patch that otherwise looks
        // perfectly successful.
        let unreadable = p.join("unreadable.txt");
        fs::write(&unreadable, "secret\n").unwrap();
        let mut perms = fs::metadata(&unreadable).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&unreadable, perms).unwrap();

        let patch = crate::git::diff::untracked_patch_and_count(p).await;
        let complete = patch.complete;
        let patch_text = patch.patch.clone();
        // Restore before asserting so a failure can't leave the TempDir
        // undeletable.
        let mut perms = fs::metadata(&unreadable).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&unreadable, perms).unwrap();

        assert!(
            !patch_text.contains("unreadable.txt"),
            "precondition: the unreadable file should be missing from the patch"
        );
        assert!(
            !complete,
            "a file git failed to diff must mark the patch incomplete, or pruning \
             will treat it as reverted and delete its comments"
        );
        assert!(
            !ComposedDiff {
                untracked_complete: false,
                ..Default::default()
            }
            .absence_is_authoritative()
        );
    }

    #[tokio::test]
    async fn prefers_origin_branch_when_remote_ref_exists() {
        let tmp = init_repo().await;
        let p = tmp.path();
        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        let sha = git_capture(p, &["rev-parse", "HEAD"]).await;

        // Simulate a pushed upstream by creating the remote-tracking ref
        // directly (no real remote needed).
        git(p, &["update-ref", "refs/remotes/origin/main", &sha]).await;

        assert_eq!(prefer_remote_branch(p, "main").await, "origin/main");
    }

    #[tokio::test]
    async fn falls_back_to_local_branch_when_no_remote_ref() {
        let tmp = init_repo().await;
        let p = tmp.path();
        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;

        // No `origin/main` remote-tracking ref → bare (local) branch name.
        assert_eq!(prefer_remote_branch(p, "main").await, "main");
    }

    #[tokio::test]
    async fn import_base_resolves_fork_point_not_branch_tip() {
        let tmp = init_repo().await;
        let p = tmp.path();

        // Fork point on the default branch.
        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        git(p, &["branch", "-M", "main"]).await;
        let fork = git_capture(p, &["rev-parse", "HEAD"]).await;

        // A feature branch with committed changes on top — analogous to a PR
        // branch checked out into a worktree, then imported.
        git(p, &["checkout", "-q", "-b", "feature"]).await;
        fs::write(p.join("file.txt"), "v2\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "B"]).await;
        let tip = git_capture(p, &["rev-parse", "HEAD"]).await;

        // Importing must record the fork point, not the branch tip. Recording
        // the tip is the bug: `merge-base(tip, HEAD)` is HEAD, so the review
        // diff comes up empty even though the branch is ahead of main.
        let base = import_base_commit(p, &tip, Some("main")).await;
        assert_eq!(base, fork, "import base should be the merge-base with main");
        assert_ne!(base, tip, "import base must not be the branch tip");

        // The committed change is visible against the resolved base.
        let diff = compose_review_diff(p, &base).await.unwrap().raw;
        assert!(
            diff.contains("-v1") && diff.contains("+v2"),
            "diff against fork point is empty:\n{diff}"
        );
    }

    #[tokio::test]
    async fn import_base_falls_back_to_head_without_default_branch() {
        let tmp = init_repo().await;
        let p = tmp.path();
        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        let head = git_capture(p, &["rev-parse", "HEAD"]).await;

        // No default branch known → leave behaviour as it was (record HEAD).
        assert_eq!(import_base_commit(p, &head, None).await, head);
    }

    #[tokio::test]
    async fn managed_base_records_fork_point_for_checked_out_branch() {
        let tmp = init_repo().await;
        let p = tmp.path();

        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        git(p, &["branch", "-M", "main"]).await;
        let fork = git_capture(p, &["rev-parse", "HEAD"]).await;

        // An existing branch with committed work — as the Checkout Branch flow
        // checks out into a session worktree, sitting on the branch tip.
        git(p, &["checkout", "-q", "-b", "feature"]).await;
        fs::write(p.join("file.txt"), "v2\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "B"]).await;
        let tip = git_capture(p, &["rev-parse", "HEAD"]).await;

        // A pre-existing branch must record its fork point, not its tip.
        // Recording the tip is the bug: `merge-base(tip, HEAD)` is HEAD, so the
        // review diff is empty even though the branch is ahead of main.
        let base = managed_base_commit(p, &tip, Some("main"), true).await;
        assert_eq!(
            base, fork,
            "checkout base should be the merge-base with main"
        );
        assert_ne!(base, tip, "checkout base must not be the branch tip");

        let diff = compose_review_diff(p, &base).await.unwrap().raw;
        assert!(
            diff.contains("-v1") && diff.contains("+v2"),
            "diff against fork point is empty:\n{diff}"
        );
    }

    #[tokio::test]
    async fn managed_base_records_head_for_fresh_branch() {
        let tmp = init_repo().await;
        let p = tmp.path();
        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        git(p, &["branch", "-M", "main"]).await;
        let head = git_capture(p, &["rev-parse", "HEAD"]).await;

        // A freshly generated branch is created empty off its base, so HEAD is
        // already the fork point — record it verbatim rather than recomputing.
        assert_eq!(
            managed_base_commit(p, &head, Some("main"), false).await,
            head
        );
    }

    #[tokio::test]
    async fn diff_stat_against_branch_uses_merge_base_not_raw_base() {
        let tmp = init_repo().await;
        let p = tmp.path();

        // Fork point on main.
        fs::write(p.join("file.txt"), "v1\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "A"]).await;
        git(p, &["branch", "-M", "main"]).await;

        // feature adds one file…
        git(p, &["checkout", "-q", "-b", "feature"]).await;
        fs::write(p.join("feature.txt"), "f\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "B"]).await;

        // …while main diverges with a different file.
        git(p, &["checkout", "-q", "main"]).await;
        fs::write(p.join("main.txt"), "m\n").unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-q", "-m", "C"]).await;
        git(p, &["checkout", "-q", "feature"]).await;

        // Through the merge-base the stat counts only feature's own change.
        let target = diff_target(p, "main").await;
        let stat = crate::git::diff_stat_summary(p, &target)
            .await
            .expect("non-empty stat");
        assert!(
            stat.contains("1 file changed"),
            "merge-base stat should count only feature's change: {stat}"
        );

        // Diffing the raw (diverged) branch tip also counts main's commit as a
        // deletion — the inflated count the merge-base routing avoids.
        let raw = crate::git::diff_stat_summary(p, "main")
            .await
            .expect("non-empty stat");
        assert!(
            raw.contains("2 files changed"),
            "raw-base stat should be inflated by main's divergence: {raw}"
        );
    }

    /// Minimal one-file diff with the given path, hunk ranges, and body lines.
    fn one_file_diff(path: &str, ranges: &str, body: &str) -> String {
        format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ {ranges} @@\n{body}")
    }

    #[test]
    fn file_diff_hash_is_deterministic() {
        let raw = one_file_diff("src/a.rs", "-1,2 +1,2", " ctx\n-old\n+new\n");
        let a = parse_unified_diff(&raw);
        let b = parse_unified_diff(&raw);
        assert_eq!(
            claude_commander_viewmodel::review::file_diff_hash(&a.files[0]),
            claude_commander_viewmodel::review::file_diff_hash(&b.files[0])
        );
    }

    #[test]
    fn file_diff_hash_changes_on_line_content_change() {
        let a = parse_unified_diff(&one_file_diff(
            "src/a.rs",
            "-1,2 +1,2",
            " ctx\n-old\n+new\n",
        ));
        let b = parse_unified_diff(&one_file_diff(
            "src/a.rs",
            "-1,2 +1,2",
            " ctx\n-old\n+newer\n",
        ));
        assert_ne!(
            claude_commander_viewmodel::review::file_diff_hash(&a.files[0]),
            claude_commander_viewmodel::review::file_diff_hash(&b.files[0])
        );
    }

    #[test]
    fn file_diff_hash_changes_on_added_line() {
        let a = parse_unified_diff(&one_file_diff(
            "src/a.rs",
            "-1,2 +1,2",
            " ctx\n-old\n+new\n",
        ));
        let b = parse_unified_diff(&one_file_diff(
            "src/a.rs",
            "-1,2 +1,3",
            " ctx\n-old\n+new\n+extra\n",
        ));
        assert_ne!(
            claude_commander_viewmodel::review::file_diff_hash(&a.files[0]),
            claude_commander_viewmodel::review::file_diff_hash(&b.files[0])
        );
    }

    #[test]
    fn file_diff_hash_changes_when_hunk_positions_shift() {
        // Identical lines, but the hunk has moved within the file.
        let a = parse_unified_diff(&one_file_diff(
            "src/a.rs",
            "-1,2 +1,2",
            " ctx\n-old\n+new\n",
        ));
        let b = parse_unified_diff(&one_file_diff(
            "src/a.rs",
            "-10,2 +10,2",
            " ctx\n-old\n+new\n",
        ));
        assert_ne!(
            claude_commander_viewmodel::review::file_diff_hash(&a.files[0]),
            claude_commander_viewmodel::review::file_diff_hash(&b.files[0])
        );
    }

    #[test]
    fn file_diff_hash_ignores_path() {
        // Identity is the display path (the map key), not the hash, so two
        // files with identical hunks hash the same.
        let a = parse_unified_diff(&one_file_diff(
            "src/a.rs",
            "-1,2 +1,2",
            " ctx\n-old\n+new\n",
        ));
        let b = parse_unified_diff(&one_file_diff(
            "src/b.rs",
            "-1,2 +1,2",
            " ctx\n-old\n+new\n",
        ));
        assert_eq!(
            claude_commander_viewmodel::review::file_diff_hash(&a.files[0]),
            claude_commander_viewmodel::review::file_diff_hash(&b.files[0])
        );
    }
}
