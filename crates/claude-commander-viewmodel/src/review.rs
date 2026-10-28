//! Reviewed-mark decisions over a parsed review diff.
//!
//! A reviewed mark pins one file of a session's review diff to a hash of that
//! file's hunks, so it holds only while the file's diff is exactly what was
//! marked. Every frontend has to apply that rule to the same data — the server
//! when it prunes, the `review` CLI when it lists, and any view folding in
//! marks polled from `GET /sessions/{id}/files/reviewed` — so the hash and the
//! rule live here, where the Flutter cdylib can reach them too, rather than in
//! core with each client re-deriving them.
//!
//! **The hash bytes are persisted** (every stored mark carries one), so
//! [`file_diff_hash`]'s canonical rendering must never change: a change would
//! silently clear every user's review progress.

use std::collections::{HashMap, HashSet};

use claude_commander_protocol::api::ReviewedFileMark;
use claude_commander_protocol::diff::{FileDiff, ParsedDiff};
use xxhash_rust::xxh3::Xxh3;

/// Stable content hash of a file's diff: xxh3_64 over a canonical byte
/// rendering of its hunks. Changes whenever any hunk header (position) or
/// line changes; deliberately excludes the path, which is the identity key.
pub fn file_diff_hash(file: &FileDiff) -> u64 {
    let mut h = Xxh3::new();
    for hunk in &file.hunks {
        h.update(
            format!(
                "@@ -{},{} +{},{} @@{}\n",
                hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines, hunk.header
            )
            .as_bytes(),
        );
        for line in &hunk.lines {
            h.update(&[line.origin.prefix() as u8]);
            h.update(line.content.as_bytes());
            h.update(b"\n");
        }
    }
    h.digest()
}

/// Hex rendering of a file's [`file_diff_hash`], as stored in a mark and
/// served in [`ReviewedFileMark::hash`].
pub fn hash_hex(file: &FileDiff) -> String {
    format!("{:016x}", file_diff_hash(file))
}

/// Every file's [`hash_hex`] in one diff, by display path. Hashing walks every
/// line of a file's hunks, so it is done once per diff and every mark check
/// after that is a lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffHashes(HashMap<String, String>);

impl DiffHashes {
    pub fn of(diff: &ParsedDiff) -> Self {
        Self(
            diff.files
                .iter()
                .map(|f| (f.display_path().to_string(), hash_hex(f)))
                .collect(),
        )
    }

    /// `path`'s current hash, when it is in the diff.
    pub fn get(&self, path: &str) -> Option<&str> {
        self.0.get(path).map(String::as_str)
    }

    /// The mark that pins `path` to its current content, when it is in the diff.
    pub fn mark_for(&self, path: &str) -> Option<ReviewedFileMark> {
        self.get(path).map(|hash| ReviewedFileMark {
            file: path.to_string(),
            hash: hash.to_string(),
        })
    }

    /// Whether a mark on `path` pinned to `hash` still holds: the file is in
    /// the diff and its hunks are exactly what was marked. The one rule
    /// deciding whether a mark counts.
    pub fn holds(&self, path: &str, hash: &str) -> bool {
        self.get(path).is_some_and(|h| h.eq_ignore_ascii_case(hash))
    }
}

/// Display paths of the `(file, hash)` marks that still hold against the diff
/// `hashes` describes — the reviewed set a view showing that diff should
/// render. A mark pinned to content the view isn't showing stays invisible.
pub fn live_paths<'a>(
    marks: impl IntoIterator<Item = (&'a str, &'a str)>,
    hashes: &DiffHashes,
) -> HashSet<String> {
    marks
        .into_iter()
        .filter(|(file, hash)| hashes.holds(file, hash))
        .map(|(file, _)| file.to_string())
        .collect()
}

/// Fold marks polled from the store into the reviewed set a view of `diff`
/// shows, per file on screen:
///
/// - a mark pinned to the content shown → reviewed;
/// - no mark → not reviewed (unmarked elsewhere);
/// - a mark pinned to *other* content → left as the view shows it. Either the
///   view is stale (a mark set since pins to the working tree, which may be
///   newer than the diff on screen) or the mark is; the view's next refresh
///   settles it either way.
///
/// Returns the paths that became reviewed, in diff order, so the view can
/// react as it would to its own mark (collapsing a finished directory).
pub fn fold_marks(
    shown: &mut HashSet<String>,
    diff: &ParsedDiff,
    hashes: &DiffHashes,
    marks: &[ReviewedFileMark],
) -> Vec<String> {
    let live = live_paths(
        marks.iter().map(|m| (m.file.as_str(), m.hash.as_str())),
        hashes,
    );
    let stored: HashSet<&str> = marks.iter().map(|m| m.file.as_str()).collect();
    let mut newly_reviewed = Vec::new();
    for file in &diff.files {
        let path = file.display_path();
        if live.contains(path) {
            if shown.insert(path.to_string()) {
                newly_reviewed.push(path.to_string());
            }
        } else if !stored.contains(path) {
            shown.remove(path);
        }
    }
    newly_reviewed
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_commander_protocol::diff::{DiffLine, FileStatus, Hunk, LineOrigin};

    /// A one-hunk modified file whose single added line is `added`.
    fn file(path: &str, added: &str) -> FileDiff {
        FileDiff {
            old_path: path.to_string(),
            new_path: path.to_string(),
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
                header: String::new(),
                lines: vec![DiffLine {
                    origin: LineOrigin::Addition,
                    old_lineno: None,
                    new_lineno: Some(1),
                    content: added.to_string(),
                }],
            }],
            binary: None,
        }
    }

    fn diff(files: Vec<FileDiff>) -> ParsedDiff {
        ParsedDiff { files }
    }

    fn mark(file: &FileDiff) -> ReviewedFileMark {
        ReviewedFileMark {
            file: file.display_path().to_string(),
            hash: hash_hex(file),
        }
    }

    /// Stored marks carry this hash, so its bytes are pinned: a change to the
    /// canonical rendering would silently clear every user's marks.
    #[test]
    fn hash_is_pinned() {
        assert_eq!(hash_hex(&file("a.rs", "x")), "10aff8b8fbe085dd");
    }

    #[test]
    fn hash_ignores_path_but_not_content() {
        assert_eq!(hash_hex(&file("a.rs", "x")), hash_hex(&file("b.rs", "x")));
        assert_ne!(hash_hex(&file("a.rs", "x")), hash_hex(&file("a.rs", "y")));
    }

    #[test]
    fn holds_needs_the_path_and_matching_hash_in_any_case() {
        let a = file("a.rs", "x");
        let hashes = DiffHashes::of(&diff(vec![a.clone()]));
        assert!(hashes.holds("a.rs", &hash_hex(&a).to_ascii_uppercase()));
        assert!(!hashes.holds("a.rs", "0000000000000000"));
        assert!(!hashes.holds("gone.rs", &hash_hex(&a)));
    }

    #[test]
    fn fold_applies_live_marks_and_unmarks_but_leaves_marks_for_other_content() {
        let shown_diff = diff(vec![
            file("a.rs", "x"),
            file("b.rs", "x"),
            file("c.rs", "x"),
        ]);
        let hashes = DiffHashes::of(&shown_diff);
        let mut shown: HashSet<String> = ["b.rs", "c.rs"].map(String::from).into();
        let marks = [
            mark(&file("a.rs", "x")),
            // b.rs is marked for content this view isn't showing.
            mark(&file("b.rs", "newer")),
        ];

        let newly = fold_marks(&mut shown, &shown_diff, &hashes, &marks);

        assert_eq!(newly, ["a.rs"]);
        assert!(shown.contains("a.rs"), "a live mark folds in");
        assert!(
            shown.contains("b.rs"),
            "a mark for other content is left as shown"
        );
        assert!(!shown.contains("c.rs"), "no mark at all is an unmark");
    }
}
