//! Review/diff + comments HTTP API exposed to Flutter via flutter_rust_bridge.
//!
//! Follows the same style as [`crate::api::simple`] (blocking reqwest, bearer
//! auth, `base`/`client`/`ok_or_status` helpers): each `pub fn` becomes an
//! async-callable Dart function, deserializing server JSON into the shared
//! `claude-commander-protocol` types so any drift fails here in Rust.
//!
//! ## Why DTOs instead of returning the protocol types
//!
//! frb renders Rust **data-carrying enums** as Dart `freezed` classes, which
//! needs the build_runner toolchain — we deliberately don't add it (same rule
//! as [`crate::api::terminal::TerminalEvent`]). The protocol review types carry
//! data enums (`ApplyOutcome`, `BinaryKind`) and tuples (`Comment.line_range`),
//! so we deserialize the server payload into the protocol types and then
//! convert into the plain-struct / unit-enum DTOs defined below before handing
//! them to frb.

use anyhow::Result;
use chrono::{DateTime, Utc};
use claude_commander_protocol::api::{DiffSide, NewComment, ReviewSnapshot};
use claude_commander_protocol::comment::{ApplyOutcome, Comment, CommentSide, CommentStatus};
use claude_commander_protocol::diff::{
    BinaryKind, DiffLine, FileDiff, FileStatus, Hunk, LineOrigin, ParsedDiff,
};

use crate::api::registry::{call, parse_session_id, with_client};

// ---------------------------------------------------------------------------
// DTOs — plain structs + unit enums only (no freezed).
// ---------------------------------------------------------------------------

/// Origin of a single diff line (unit-enum mirror of [`LineOrigin`]).
pub enum ReviewLineOrigin {
    Context,
    Addition,
    Deletion,
}

impl From<LineOrigin> for ReviewLineOrigin {
    fn from(o: LineOrigin) -> Self {
        match o {
            LineOrigin::Context => Self::Context,
            LineOrigin::Addition => Self::Addition,
            LineOrigin::Deletion => Self::Deletion,
        }
    }
}

/// How a file changed (unit-enum mirror of [`FileStatus`]).
pub enum ReviewFileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

impl From<FileStatus> for ReviewFileStatus {
    fn from(s: FileStatus) -> Self {
        match s {
            FileStatus::Added => Self::Added,
            FileStatus::Deleted => Self::Deleted,
            FileStatus::Modified => Self::Modified,
            FileStatus::Renamed => Self::Renamed,
        }
    }
}

/// Which side of the diff a comment range refers to (unit-enum mirror of
/// [`CommentSide`]).
pub enum ReviewCommentSide {
    Old,
    New,
}

impl From<CommentSide> for ReviewCommentSide {
    fn from(s: CommentSide) -> Self {
        match s {
            CommentSide::Old => Self::Old,
            CommentSide::New => Self::New,
        }
    }
}

/// Lifecycle status of a comment (unit-enum mirror of [`CommentStatus`]).
pub enum ReviewCommentStatus {
    Staged,
    Drifted,
    Applied,
}

impl From<CommentStatus> for ReviewCommentStatus {
    fn from(s: CommentStatus) -> Self {
        match s {
            CommentStatus::Staged => Self::Staged,
            CommentStatus::Drifted => Self::Drifted,
            CommentStatus::Applied => Self::Applied,
        }
    }
}

/// A single diff line with resolved old/new line numbers.
pub struct ReviewLineDto {
    pub origin: ReviewLineOrigin,
    /// `None` for additions.
    pub old_lineno: Option<u32>,
    /// `None` for deletions.
    pub new_lineno: Option<u32>,
    /// Line content without the leading `+`/`-`/space marker.
    pub content: String,
}

impl From<DiffLine> for ReviewLineDto {
    fn from(l: DiffLine) -> Self {
        Self {
            origin: l.origin.into(),
            old_lineno: l.old_lineno.map(|n| n as u32),
            new_lineno: l.new_lineno.map(|n| n as u32),
            content: l.content,
        }
    }
}

/// A contiguous block of changes (one `@@ ... @@` section).
pub struct ReviewHunkDto {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Section heading following the closing `@@`, if any.
    pub header: String,
    pub lines: Vec<ReviewLineDto>,
}

impl From<Hunk> for ReviewHunkDto {
    fn from(h: Hunk) -> Self {
        Self {
            old_start: h.old_start as u32,
            old_lines: h.old_lines as u32,
            new_start: h.new_start as u32,
            new_lines: h.new_lines as u32,
            header: h.header,
            lines: h.lines.into_iter().map(Into::into).collect(),
        }
    }
}

/// All changes to a single file. `BinaryKind`/`BinaryInfo` are flattened onto
/// `is_binary` + `binary_mime`; the blob bytes are fetched lazily via
/// [`fetch_blob`].
pub struct ReviewFileDto {
    /// Path to show in the file list (new path, or old path for deletions).
    pub display_path: String,
    pub old_path: String,
    pub new_path: String,
    pub status: ReviewFileStatus,
    pub added: u32,
    pub removed: u32,
    pub hunks: Vec<ReviewHunkDto>,
    pub is_binary: bool,
    /// `Some(mime)` when the binary is a renderable image; `None` otherwise.
    pub binary_mime: Option<String>,
}

impl From<FileDiff> for ReviewFileDto {
    fn from(f: FileDiff) -> Self {
        let display_path = f.display_path().to_string();
        let (is_binary, binary_mime) = match &f.binary {
            Some(info) => match &info.kind {
                BinaryKind::Image { mime } => (true, Some(mime.clone())),
                BinaryKind::Other => (true, None),
            },
            None => (false, None),
        };
        Self {
            display_path,
            old_path: f.old_path,
            new_path: f.new_path,
            status: f.status.into(),
            added: f.added as u32,
            removed: f.removed as u32,
            hunks: f.hunks.into_iter().map(Into::into).collect(),
            is_binary,
            binary_mime,
        }
    }
}

/// A parsed unified diff: an ordered list of changed files.
pub struct ReviewDiffDto {
    pub files: Vec<ReviewFileDto>,
}

impl From<ParsedDiff> for ReviewDiffDto {
    fn from(d: ParsedDiff) -> Self {
        Self {
            files: d.files.into_iter().map(Into::into).collect(),
        }
    }
}

/// A single review comment. `line_range: (usize, usize)` is split into
/// `line_start`/`line_end`; the id is exposed as a `String`.
pub struct CommentDto {
    pub id: String,
    pub file: String,
    pub side: ReviewCommentSide,
    pub line_start: u32,
    pub line_end: u32,
    pub snippet: String,
    pub comment: String,
    pub status: ReviewCommentStatus,
    pub created_at: DateTime<Utc>,
}

impl From<Comment> for CommentDto {
    fn from(c: Comment) -> Self {
        Self {
            id: c.id.to_string(),
            file: c.file,
            side: c.side.into(),
            line_start: c.line_range.0 as u32,
            line_end: c.line_range.1 as u32,
            snippet: c.snippet,
            comment: c.comment,
            status: c.status.into(),
            created_at: c.created_at,
        }
    }
}

/// Result of opening the review view. `content_hash` is a `u64` server-side;
/// it's exposed as a `String` because a Dart `int` can't hold all `u64` values
/// (and the value is only ever echoed back to `refresh_review`).
pub struct ReviewSnapshotDto {
    pub base: String,
    pub content_hash: String,
    pub files: Vec<ReviewFileDto>,
    pub comments: Vec<CommentDto>,
    /// Display paths of files still marked reviewed.
    pub reviewed: Vec<String>,
    /// The raw unified diff the snapshot was parsed from, to be handed straight
    /// back to [`diff_rows`] for layout. `None` against a server predating the
    /// field, in which case `diff_rows` falls back to `files`.
    pub raw: Option<String>,
}

impl From<ReviewSnapshot> for ReviewSnapshotDto {
    fn from(s: ReviewSnapshot) -> Self {
        Self {
            base: s.base,
            content_hash: s.content_hash.to_string(),
            files: s.diff.files.into_iter().map(Into::into).collect(),
            comments: s.comments.into_iter().map(Into::into).collect(),
            reviewed: s.reviewed,
            raw: s.raw,
        }
    }
}

/// Which kind of [`ApplyResult`] this is (unit enum, so frb renders a plain Dart
/// enum rather than a freezed class). Flattens the data-carrying
/// [`ApplyOutcome`].
pub enum ApplyResultKind {
    /// No staged comments to apply.
    Nothing,
    /// One or more comments are drifted; nothing was sent.
    Blocked,
    /// Comments were composed and the prompt injected.
    Applied,
    /// The brief was written but couldn't be delivered; the user can re-apply.
    Deferred,
}

/// Outcome of applying a session's staged comments — a tagged struct (not a
/// data enum), following the [`crate::api::terminal::TerminalEvent`] rule.
pub struct ApplyResult {
    pub kind: ApplyResultKind,
    /// Ids of drifted comments (populated only for [`ApplyResultKind::Blocked`]).
    pub drifted_ids: Vec<String>,
    /// Path the brief was composed to (`Applied`/`Deferred`); `None` otherwise.
    pub path: Option<String>,
    /// Number of comments composed (`Applied`/`Deferred`); `0` otherwise.
    pub count: u32,
}

impl From<ApplyOutcome> for ApplyResult {
    fn from(o: ApplyOutcome) -> Self {
        match o {
            ApplyOutcome::Nothing => Self {
                kind: ApplyResultKind::Nothing,
                drifted_ids: Vec::new(),
                path: None,
                count: 0,
            },
            ApplyOutcome::Blocked { drifted } => Self {
                kind: ApplyResultKind::Blocked,
                drifted_ids: drifted.into_iter().map(|id| id.to_string()).collect(),
                path: None,
                count: 0,
            },
            ApplyOutcome::Applied { path, count } => Self {
                kind: ApplyResultKind::Applied,
                drifted_ids: Vec::new(),
                path: Some(path.to_string_lossy().into_owned()),
                count: count as u32,
            },
            ApplyOutcome::Deferred { path, count } => Self {
                kind: ApplyResultKind::Deferred,
                drifted_ids: Vec::new(),
                path: Some(path.to_string_lossy().into_owned()),
                count: count as u32,
            },
        }
    }
}

/// Parse the Dart-facing `"old"`/`"new"` side string into a protocol enum.
fn parse_side(side: &str) -> Result<CommentSide> {
    match side {
        "old" => Ok(CommentSide::Old),
        "new" => Ok(CommentSide::New),
        other => anyhow::bail!("invalid comment side {other:?} (expected \"old\" or \"new\")"),
    }
}

/// Same, for the diff-blob side (`DiffSide`).
fn parse_diff_side(side: &str) -> Result<DiffSide> {
    match side {
        "old" => Ok(DiffSide::Old),
        "new" => Ok(DiffSide::New),
        other => anyhow::bail!("invalid diff side {other:?} (expected \"old\" or \"new\")"),
    }
}

// ---------------------------------------------------------------------------
// cdylib functions. Each resolves the server `handle` to a `RemoteClient`,
// drives the async route, and converts the protocol type to a DTO.
// ---------------------------------------------------------------------------

/// Open the review snapshot (parsed diff + re-anchored comments + reviewed
/// marks) for a session, converted to DTOs.
pub fn open_review(handle: String, session_id: String) -> Result<ReviewSnapshotDto> {
    let client = with_client(&handle)?;
    let sid = parse_session_id(&session_id)?;
    Ok(call(client.open_review(sid))?.into())
}

/// A fresh snapshot, or `None` when the diff is unchanged. `prev_hash` is the
/// `content_hash` string from a prior snapshot, parsed back to a `u64`.
pub fn refresh_review(
    handle: String,
    session_id: String,
    prev_hash: String,
) -> Result<Option<ReviewSnapshotDto>> {
    let client = with_client(&handle)?;
    let sid = parse_session_id(&session_id)?;
    let prev_hash: u64 = prev_hash
        .parse()
        .map_err(|_| anyhow::anyhow!("prev_hash was not a valid content hash"))?;
    Ok(call(client.refresh_review_if_changed(sid, prev_hash))?.map(Into::into))
}

/// The session's comments (re-anchored), as DTOs.
pub fn list_comments(handle: String, session_id: String) -> Result<Vec<CommentDto>> {
    let client = with_client(&handle)?;
    let sid = parse_session_id(&session_id)?;
    Ok(call(client.list_comments(sid))?
        .into_iter()
        .map(Into::into)
        .collect())
}

/// Stage a new comment, returning its id. `side` is `"old"` or `"new"`.
#[allow(clippy::too_many_arguments)]
pub fn create_comment(
    handle: String,
    session_id: String,
    file: String,
    side: String,
    line_start: u32,
    line_end: u32,
    snippet: String,
    comment: String,
) -> Result<String> {
    let client = with_client(&handle)?;
    let sid = parse_session_id(&session_id)?;
    let draft = NewComment {
        file,
        side: parse_side(&side)?,
        line_range: (line_start as usize, line_end as usize),
        snippet,
        comment,
    };
    Ok(call(client.create_comment(sid, draft))?.to_string())
}

/// Delete a staged comment by id.
pub fn delete_comment(handle: String, session_id: String, comment_id: String) -> Result<()> {
    let client = with_client(&handle)?;
    let sid = parse_session_id(&session_id)?;
    let cid = uuid::Uuid::parse_str(&comment_id)
        .map_err(|_| anyhow::anyhow!("invalid comment id {comment_id:?}"))?;
    call(client.delete_comment(sid, cid))
}

/// Apply a session's staged comments, returning the flattened [`ApplyResult`].
pub fn apply_comments(handle: String, session_id: String) -> Result<ApplyResult> {
    let client = with_client(&handle)?;
    let sid = parse_session_id(&session_id)?;
    Ok(call(client.apply_comments(sid))?.into())
}

/// Raw file bytes for one side of a (binary) diff. `side` is `"old"`/`"new"`;
/// `path` is the file's display path. Used to render binary images.
pub fn fetch_blob(
    handle: String,
    session_id: String,
    side: String,
    path: String,
) -> Result<Vec<u8>> {
    let client = with_client(&handle)?;
    let sid = parse_session_id(&session_id)?;
    call(client.fetch_diff_blob(sid, parse_diff_side(&side)?, path))
}

/// Set (`reviewed`) or clear a file's reviewed mark, returning the new state.
/// Only the display path crosses the wire — the server resolves the file in
/// the *current* review diff. The page says which way it means rather than
/// toggling, because the store may hold a mark the page isn't showing (set by
/// the `review` CLI since it loaded), which a toggle would flip the wrong way.
pub fn set_file_reviewed(
    handle: String,
    session_id: String,
    display_path: String,
    reviewed: bool,
) -> Result<bool> {
    let client = with_client(&handle)?;
    let sid = parse_session_id(&session_id)?;
    call(client.set_file_reviewed(sid, display_path, reviewed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_commander_protocol::comment::Comment;
    use std::path::PathBuf;

    #[test]
    fn parse_side_and_diff_side_accept_wire_forms() {
        assert!(matches!(parse_side("old").unwrap(), CommentSide::Old));
        assert!(matches!(parse_side("new").unwrap(), CommentSide::New));
        assert!(parse_side("sideways").is_err());
        assert!(matches!(parse_diff_side("old").unwrap(), DiffSide::Old));
        assert!(matches!(parse_diff_side("new").unwrap(), DiffSide::New));
        assert!(parse_diff_side("").is_err());
    }

    #[test]
    fn comment_dto_splits_line_range_and_stringifies_id() {
        let c = Comment::new("a.rs", CommentSide::New, (3, 7), "snip", "note");
        let id = c.id.to_string();
        let dto: CommentDto = c.into();
        assert_eq!(dto.line_start, 3);
        assert_eq!(dto.line_end, 7);
        assert_eq!(dto.id, id);
        assert!(matches!(dto.side, ReviewCommentSide::New));
        assert!(matches!(dto.status, ReviewCommentStatus::Staged));
    }

    #[test]
    fn apply_result_flattens_outcome_variants() {
        let nothing: ApplyResult = ApplyOutcome::Nothing.into();
        assert!(matches!(nothing.kind, ApplyResultKind::Nothing));

        let id = uuid::Uuid::new_v4();
        let blocked: ApplyResult = ApplyOutcome::Blocked { drifted: vec![id] }.into();
        assert!(matches!(blocked.kind, ApplyResultKind::Blocked));
        assert_eq!(blocked.drifted_ids, vec![id.to_string()]);

        let applied: ApplyResult = ApplyOutcome::Applied {
            path: PathBuf::from("/tmp/brief.md"),
            count: 4,
        }
        .into();
        assert!(matches!(applied.kind, ApplyResultKind::Applied));
        assert_eq!(applied.count, 4);
        assert_eq!(applied.path.as_deref(), Some("/tmp/brief.md"));
    }

    #[test]
    fn snapshot_dto_exposes_content_hash_as_string() {
        let snap = ReviewSnapshot {
            base: "main".into(),
            diff: ParsedDiff::default(),
            comments: vec![],
            reviewed: vec![],
            content_hash: u64::MAX,
            dropped_comments: vec![],
            raw: Some("diff --git a/x b/x\n".to_string()),
        };
        let dto: ReviewSnapshotDto = snap.into();
        assert_eq!(dto.content_hash, u64::MAX.to_string());
        assert_eq!(dto.raw.as_deref(), Some("diff --git a/x b/x\n"));
    }

    #[test]
    fn binary_image_file_flattens_to_mime() {
        use claude_commander_protocol::diff::{BinaryInfo, BinaryKind};
        let f = FileDiff {
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
                old_oid: None,
                new_oid: None,
                old_size: None,
                new_size: None,
            }),
        };
        let dto: ReviewFileDto = f.into();
        assert!(dto.is_binary);
        assert_eq!(dto.binary_mime.as_deref(), Some("image/png"));
    }
}
