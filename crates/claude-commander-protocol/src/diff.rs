//! Structured unified-diff model for the review/diff view.
//!
//! These types are the wire shape of a parsed `file -> hunk -> line` diff: the
//! server parses a unified diff into a [`ParsedDiff`] (via the parser in
//! `claude_commander_core`) and serializes it; clients deserialize it to render
//! gutters, drive line-range selection, and anchor comments.
//!
//! The *parsing* and *hashing* logic stays in core (it needs git plumbing); only
//! the data model lives here so it can cross the network and compile to mobile.

use serde::{Deserialize, Serialize};

/// Origin of a single diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum LineOrigin {
    Context,
    Addition,
    Deletion,
}

impl LineOrigin {
    /// The unified-diff column prefix for a line of this origin.
    pub fn prefix(self) -> char {
        match self {
            LineOrigin::Addition => '+',
            LineOrigin::Deletion => '-',
            LineOrigin::Context => ' ',
        }
    }
}

/// A single line within a hunk, with resolved old/new line numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DiffLine {
    pub origin: LineOrigin,
    /// Line number on the old side (`None` for additions).
    pub old_lineno: Option<usize>,
    /// Line number on the new side (`None` for deletions).
    pub new_lineno: Option<usize>,
    /// Line content without the leading `+`/`-`/space marker.
    pub content: String,
}

/// A contiguous block of changes (one `@@ ... @@` section).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Hunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    /// Text following the closing `@@` (the section heading), if any.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// How a file changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

impl FileStatus {
    /// The one-letter marker git uses for this status (`A`/`D`/`M`/`R`).
    pub fn marker(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Deleted => 'D',
            FileStatus::Modified => 'M',
            FileStatus::Renamed => 'R',
        }
    }
}

/// What kind of binary a file is, for consumers deciding how to render it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BinaryKind {
    /// A raster image we can render, tagged with its MIME type (e.g.
    /// `image/png`) so a GUI can build a `data:` URL directly.
    Image { mime: String },
    /// Any other binary blob (rendered as a placeholder, not an image).
    Other,
}

/// Metadata for a binary file's diff. The bytes themselves are NOT inlined
/// here — consumers lazy-load them via the `GET /sessions/{id}/blob` endpoint
/// keyed by `(side, path)`. `old_*`/`new_*` are `None` on the side that does
/// not exist (added files have no old side; deleted files have no new side).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct BinaryInfo {
    pub kind: BinaryKind,
    /// Base-side blob oid (from the diff `index` line), if present.
    pub old_oid: Option<String>,
    /// Working-tree-side blob oid (from the diff `index` line), if present.
    pub new_oid: Option<String>,
    /// Base-side blob size in bytes, filled in by `open_review` (not the parser).
    pub old_size: Option<u64>,
    /// Working-tree-side size in bytes, filled in by `open_review`.
    pub new_size: Option<u64>,
}

/// All changes to a single file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub status: FileStatus,
    pub added: usize,
    pub removed: usize,
    pub hunks: Vec<Hunk>,
    /// `Some` when this file is binary (no textual hunks); `None` for text.
    pub binary: Option<BinaryInfo>,
}

impl FileDiff {
    /// Path to show in the file list: the new path, except for deletions where
    /// only the old path is meaningful.
    pub fn display_path(&self) -> &str {
        if self.status == FileStatus::Deleted {
            &self.old_path
        } else {
            &self.new_path
        }
    }
}

/// A parsed unified diff: an ordered list of changed files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ParsedDiff {
    pub files: Vec<FileDiff>,
}

impl ParsedDiff {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The diff model round-trips through JSON — the contract a client relies on
    /// to deserialize what the server's `open_review` serialized.
    #[test]
    fn file_diff_round_trips() {
        let original = FileDiff {
            old_path: "a.rs".into(),
            new_path: "a.rs".into(),
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
                header: "fn x".into(),
                lines: vec![DiffLine {
                    origin: LineOrigin::Addition,
                    old_lineno: None,
                    new_lineno: Some(1),
                    content: "let y = 2;".into(),
                }],
            }],
            binary: None,
        };
        let wire = serde_json::to_string(&original).unwrap();
        let back: FileDiff = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, original);
    }

    /// The doubly-tagged binary-image case (the `tag = "kind"` enum) round-trips.
    #[test]
    fn binary_image_round_trips() {
        let original = FileDiff {
            old_path: "logo.png".into(),
            new_path: "logo.png".into(),
            status: FileStatus::Modified,
            added: 0,
            removed: 0,
            hunks: vec![],
            binary: Some(BinaryInfo {
                kind: BinaryKind::Image {
                    mime: "image/png".into(),
                },
                old_oid: Some("aaaa".into()),
                new_oid: Some("bbbb".into()),
                old_size: Some(128),
                new_size: Some(256),
            }),
        };
        let wire = serde_json::to_string(&original).unwrap();
        let back: FileDiff = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn display_path_prefers_old_for_deletions() {
        let mut f = FileDiff {
            old_path: "gone.rs".into(),
            new_path: "gone.rs".into(),
            status: FileStatus::Deleted,
            added: 0,
            removed: 1,
            hunks: vec![],
            binary: None,
        };
        assert_eq!(f.display_path(), "gone.rs");
        f.status = FileStatus::Modified;
        f.new_path = "new.rs".into();
        assert_eq!(f.display_path(), "new.rs");
    }
}
