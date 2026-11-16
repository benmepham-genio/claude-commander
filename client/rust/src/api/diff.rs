//! Diff **layout** exposed to Flutter: raw unified diff in, rows of styled runs
//! out.
//!
//! [`crate::api::review`] carries the review *data* (files, comments, reviewed
//! marks). This module carries its *presentation*, and it is where the Flutter
//! client stops hand-rolling a renderer: [`diffgrid`] parses the diff, word-diffs
//! it, lays it out inline or side by side, expands elided context, and emits
//! runs tagged with a semantic [role](diffgrid::style::Role). The TUI renders
//! through exactly the same pipeline, so the two frontends cannot drift apart
//! about what a diff *is* — only about what colours they paint it in.
//!
//! ## Bounded parsing, per-request layout
//!
//! Layout is rebuilt per request. A cache of two raw reviews (at most 8 MiB
//! of source keys) reuses parsing across file switches, mode flips and gap
//! expansion. Exact input keys keep changed reviews fresh. Dart holds returned
//! rows while scrolling. Expansion
//! state travels as a replayable list of [`DiffExpansion`]s for the same reason.
//!
//! ## Why the DTOs are flat
//!
//! Same rule as [`crate::api::review`]: frb renders data-carrying Rust enums as
//! Dart `freezed` classes, which needs build_runner. So `Role::Gutter(origin)`
//! flattens away entirely (the gutter is a Flutter widget, not text — see
//! [`content_gutter`]) and every enum that crosses is a unit enum.

use std::borrow::Cow;

use anyhow::Result;
use diffgrid::layout::{
    ExpandAction, FileLayout, FileSource, GapKind, LayoutMode, LayoutOptions, LogicalRow, RowKind,
};
use diffgrid::style::{
    cell_spans, gutter_cols, is_full_width, row_spans, GutterSides, GutterSpec, Role, RowContext,
    Span,
};
use diffgrid::{DiffLine, FileDiff, GapIdx, Hunk, LineNo, LineOrigin, LogicalIdx, Side};

use crate::api::review::{ReviewFileDto, ReviewFileStatus, ReviewLineOrigin};

// ---------------------------------------------------------------------------
// DTOs — plain structs + unit enums only (no freezed).
// ---------------------------------------------------------------------------

/// How to lay a file out.
pub enum DiffLayoutMode {
    /// One column: deletions and additions interleaved.
    Inline,
    /// Two columns: old on the left, new on the right.
    SideBySide,
}

impl From<DiffLayoutMode> for LayoutMode {
    fn from(m: DiffLayoutMode) -> Self {
        match m {
            DiffLayoutMode::Inline => Self::Inline,
            DiffLayoutMode::SideBySide => Self::SideBySide,
        }
    }
}

/// A direction a gap was asked to reveal in.
pub enum DiffExpandAction {
    /// A step more just below the hunk above the gap.
    Down,
    /// A step more just above the hunk below the gap.
    Up,
    /// The whole gap at once.
    All,
}

impl From<&DiffExpandAction> for ExpandAction {
    fn from(a: &DiffExpandAction) -> Self {
        match a {
            DiffExpandAction::Down => Self::Down,
            DiffExpandAction::Up => Self::Up,
            DiffExpandAction::All => Self::All,
        }
    }
}

/// One expansion the user has already asked for, replayed onto a fresh layout.
///
/// Dart appends to a list and passes the whole list back; that keeps the bridge
/// stateless without making expansion state the host's problem to *model* —
/// only to remember.
pub struct DiffExpansion {
    /// Index of the gap, as reported by [`DiffRowDto::gap`].
    pub gap: u32,
    pub action: DiffExpandAction,
}

/// What a row is.
pub enum DiffRowKind {
    /// An `@@ … @@` header introducing a hunk.
    HunkHeader,
    /// A diff line — the only commentable kind.
    Line,
    /// A context line revealed out of a gap. Display-only: not part of the
    /// diff, so it carries no [`DiffCellDto::sel`] and cannot be commented on.
    ExpandedContext,
    /// A "reveal more" control for a gap. Carries no spans: Flutter draws
    /// buttons from [`DiffRowDto::hidden`] and the `can_expand_*` flags rather
    /// than centring a row of glyphs the way a terminal must.
    ExpandControl,
    /// The blank half of a side-by-side pair.
    AlignmentGap,
}

/// What a run of text *is*, semantically — the flattened form of
/// [`diffgrid::style::Role`].
///
/// `Gutter(_)` and `Padding` never reach Dart (see [`content_gutter`]), and
/// `Role` is `#[non_exhaustive]`, so anything new lands on [`Self::Other`] and
/// renders as plain text rather than failing the build.
pub enum DiffRole {
    Context,
    Addition,
    Deletion,
    HunkHeader,
    ExpandedContext,
    Other,
}

impl From<Role> for DiffRole {
    fn from(r: Role) -> Self {
        match r {
            Role::Context => Self::Context,
            Role::Addition => Self::Addition,
            Role::Deletion => Self::Deletion,
            Role::HunkHeader => Self::HunkHeader,
            Role::ExpandedContext => Self::ExpandedContext,
            _ => Self::Other,
        }
    }
}

/// A run of text with a uniform style. Concatenating a cell's spans reproduces
/// its rendered text exactly (tabs already expanded).
pub struct DiffSpanDto {
    pub text: String,
    pub role: DiffRole,
    /// The intra-line word diff marked this run as changed. Flutter composites
    /// an emphasis tint *over* the line tint for these.
    pub emphasis: bool,
}

impl From<Span<'_>> for DiffSpanDto {
    fn from(s: Span<'_>) -> Self {
        Self {
            text: s.text.into_owned(),
            role: s.style.role.into(),
            emphasis: s.style.emphasis,
        }
    }
}

/// One side's contents of a row.
pub struct DiffCellDto {
    /// `false` for the blank half of an unbalanced side-by-side pair, and for
    /// [`DiffRowDto::right`] in inline mode.
    pub present: bool,
    pub origin: Option<ReviewLineOrigin>,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    /// This line's index among the file's selectable lines — the stable key for
    /// cursor, selection and comment anchoring. `None` for revealed context,
    /// which is not part of the diff.
    pub sel: Option<u32>,
    pub spans: Vec<DiffSpanDto>,
    /// The line's text *before* tab expansion, for a comment snippet. Taken
    /// from the model rather than the spans, because a snippet the server
    /// re-anchors against has to match the file byte for byte.
    pub text: String,
}

impl DiffCellDto {
    /// The empty cell: an alignment gap, or the unused right half inline.
    fn absent() -> Self {
        Self {
            present: false,
            origin: None,
            old_lineno: None,
            new_lineno: None,
            sel: None,
            spans: Vec::new(),
            text: String::new(),
        }
    }
}

/// One laid-out row.
pub struct DiffRowDto {
    pub kind: DiffRowKind,
    /// `true` for rows that span both halves side by side (hunk headers, expand
    /// controls). Only [`Self::left`] is populated for those.
    pub full_width: bool,
    /// Inline mode: the single reading of the row. Side by side: the old half.
    pub left: DiffCellDto,
    /// Side by side: the new half. Absent in inline mode.
    pub right: DiffCellDto,
    /// The gap this row belongs to, for both control and revealed-context rows.
    pub gap: Option<u32>,
    /// Lines still hidden by the gap ([`DiffRowKind::ExpandControl`] only).
    pub hidden: u32,
    /// Whether the control can still reveal upward / downward. "Reveal all" is
    /// always available while `hidden > 0`.
    pub can_expand_up: bool,
    pub can_expand_down: bool,
}

/// A file laid out into rows, plus what the host needs to drive it.
pub struct DiffLayoutDto {
    pub rows: Vec<DiffRowDto>,
    /// How many selectable lines the file has, so a host can clamp a cursor
    /// without walking the rows.
    pub selectable: u32,
    /// The diff elides unchanged regions, so fetching the file's text would buy
    /// working expand controls. Independent of whether text was *supplied*:
    /// this is the signal to go and fetch it.
    pub has_hidden_context: bool,
}

// ---------------------------------------------------------------------------
// cdylib entry point.
// ---------------------------------------------------------------------------

/// Two recently-used raw reviews, bounded by bytes as well as entry count.
/// Mode flips and gap expansion reuse parsing; layout remains per-call.
type ParsedFiles = std::sync::Arc<Vec<FileDiff<'static>>>;
#[flutter_rust_bridge::frb(ignore)]
#[derive(Default)]
struct ParseCache {
    entries: std::collections::VecDeque<(String, ParsedFiles)>,
}
impl ParseCache {
    const MAX_BYTES: usize = 8 * 1024 * 1024;
    fn get(&mut self, raw: &str) -> Option<ParsedFiles> {
        let index = self.entries.iter().position(|(key, _)| key == raw)?;
        let entry = self.entries.remove(index)?;
        let files = entry.1.clone();
        self.entries.push_back(entry);
        Some(files)
    }
    fn insert(&mut self, raw: &str, files: ParsedFiles) {
        if raw.len() > Self::MAX_BYTES {
            return;
        }
        if self.get(raw).is_some() {
            return;
        }
        while self.entries.len() >= 2
            || self.entries.iter().map(|(key, _)| key.len()).sum::<usize>() + raw.len()
                > Self::MAX_BYTES
        {
            self.entries.pop_front();
        }
        self.entries.push_back((raw.to_owned(), files));
    }
}

fn parsed_review(raw: &str) -> ParsedFiles {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<ParseCache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(ParseCache::default()));
    if let Some(files) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(raw)
    {
        return files;
    }
    // Parsing is fallible third-party CPU work: never hold the shared mutex here.
    let files = std::sync::Arc::new(parse_all(raw));
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(raw, files.clone());
    files
}

/// Lay one file of a review diff out into rows of styled runs.
///
/// `raw` is [`crate::api::review::ReviewSnapshotDto::raw`]; when it is `None`
/// (a server predating that field) the layout is built from `fallback` instead,
/// which is lossier — no `\ No newline at EOF`, no CRLF flag, no rename
/// similarity — but identical for everything that is drawn.
///
/// `file_text` is the working-tree text of the file's new side, as fetched via
/// [`crate::api::review::fetch_blob`]. Without it, gaps stay collapsed and no
/// expand controls are offered; [`DiffLayoutDto::has_hidden_context`] says
/// whether fetching it would buy anything.
pub fn diff_rows(
    raw: Option<String>,
    fallback: ReviewFileDto,
    mode: DiffLayoutMode,
    file_text: Option<String>,
    expansions: Vec<DiffExpansion>,
    tab_width: u32,
) -> Result<DiffLayoutDto> {
    let file = match raw.as_deref() {
        Some(raw) => {
            let files = parsed_review(raw);
            files
                .iter()
                .find(|f| f.display_path() == fallback.display_path)
                .cloned()
                .unwrap_or_else(|| from_wire(&fallback))
        }
        None => from_wire(&fallback),
    };
    Ok(build(
        file,
        mode.into(),
        file_text.as_deref(),
        &expansions,
        tab_width as usize,
    ))
}

// ---------------------------------------------------------------------------
// Model.
// ---------------------------------------------------------------------------

/// Parse a whole composed diff into owned files.
///
/// Warnings are dropped: they describe input the *server* produced, so there is
/// no user action behind them, and a partial parse still renders every file it
/// did understand.
fn parse_all(raw: &str) -> Vec<FileDiff<'static>> {
    let (files, _warnings) = diffgrid::parse(raw, &diffgrid::ParseOptions::default());
    files.into_iter().map(into_owned).collect()
}

/// Detach a parsed file from the text it borrows, so it can outlive the `raw`
/// argument it was parsed out of.
fn into_owned(file: FileDiff<'_>) -> FileDiff<'static> {
    let hunks = file
        .hunks
        .into_iter()
        .map(|h| {
            let lines = h
                .lines
                .into_iter()
                .map(|l| {
                    DiffLine::new(l.origin, l.old_lineno, l.new_lineno, l.content.into_owned())
                        .with_no_newline_at_eof(l.no_newline_at_eof)
                })
                .collect();
            Hunk::new(h.old_start, h.old_lines, h.new_start, h.new_lines, lines)
                .with_section(h.section.into_owned())
        })
        .collect();
    let mut out = FileDiff::new(
        file.old_path.into_owned(),
        file.new_path.into_owned(),
        file.status,
        hunks,
    );
    if let Some(info) = file.binary {
        out = out.with_binary(info);
    }
    out
}

/// Map the wire model onto `diffgrid`'s, for the no-`raw` fallback.
///
/// A binary file is marked binary, which drops its hunks: the body renders the
/// image, never a diff.
fn from_wire(file: &ReviewFileDto) -> FileDiff<'static> {
    let status = match file.status {
        ReviewFileStatus::Added => diffgrid::FileStatus::Added,
        ReviewFileStatus::Deleted => diffgrid::FileStatus::Deleted,
        ReviewFileStatus::Modified => diffgrid::FileStatus::Modified,
        ReviewFileStatus::Renamed => diffgrid::FileStatus::Renamed,
    };
    let hunks = file
        .hunks
        .iter()
        .map(|h| {
            let lines = h
                .lines
                .iter()
                .map(|l| {
                    DiffLine::new(
                        match l.origin {
                            ReviewLineOrigin::Context => LineOrigin::Context,
                            ReviewLineOrigin::Addition => LineOrigin::Addition,
                            ReviewLineOrigin::Deletion => LineOrigin::Deletion,
                        },
                        l.old_lineno.and_then(|n| LineNo::new(n as usize)),
                        l.new_lineno.and_then(|n| LineNo::new(n as usize)),
                        l.content.clone(),
                    )
                })
                .collect();
            Hunk::new(
                h.old_start as usize,
                h.old_lines as usize,
                h.new_start as usize,
                h.new_lines as usize,
                lines,
            )
            .with_section(h.header.clone())
        })
        .collect();
    let mut out = FileDiff::new(file.old_path.clone(), file.new_path.clone(), status, hunks);
    if file.is_binary {
        out = out.with_binary(diffgrid::BinaryInfo::absent());
    }
    out
}

// ---------------------------------------------------------------------------
// Layout.
// ---------------------------------------------------------------------------

/// The gutter spec, sized so its emitted width is **exactly** what
/// [`diffgrid::style::gutter_cols`] reports.
///
/// A terminal has no choice but to draw line numbers as padded text, so
/// `diffgrid` emits them as spans. Flutter does have a choice: the gutter is a
/// real widget with its own fill and right alignment, driven by
/// [`DiffCellDto`]'s line numbers — and keeping it out of the text is what lets
/// a soft-wrapped continuation line stay in the content column instead of
/// running back under the numbers. So the emitted gutter is measured off the
/// front and thrown away by [`strip_gutter`].
///
/// It cannot simply be filtered by role: a revealed-context row tags its gutter
/// with the *body* role, not `Role::Gutter`, so role alone cannot tell the
/// number from the code. Stripping by width can — but only while the gutter's
/// real width matches its declared one, and `lineno_text` does not truncate, so
/// a number wider than `lineno_width` would silently overflow into the content.
/// Hence ten columns: the widest a `u32` line number can be, whatever the file.
fn content_gutter() -> GutterSpec {
    // Field assignment, not a struct literal: `GutterSpec` is `#[non_exhaustive]`.
    // `sign` off matters beyond width — that run carries a body role and would
    // otherwise read as leading whitespace in the code.
    let mut spec = GutterSpec::default()
        .with_lineno_width(10)
        .with_sides(GutterSides::One);
    spec.edge = false;
    spec.marker = false;
    spec.sign = false;
    spec.pad = 0;
    spec
}

/// Drops the leading runs that make up a row's gutter.
///
/// `emit_gutter` pushes the gutter as whole spans totalling exactly
/// [`gutter_cols`] display columns, so the boundary always falls between two
/// spans and this never splits one. Rows with no gutter (hunk headers, expand
/// controls) report zero columns and pass through untouched.
fn strip_gutter(spans: Vec<Span<'_>>, cols: usize) -> Vec<Span<'_>> {
    let mut taken = 0usize;
    let mut out = spans.into_iter();
    while taken < cols {
        match out.next() {
            Some(s) => taken += s.cols,
            None => break,
        }
    }
    out.collect()
}

/// Options for the intra-line word diff, matching the TUI's exactly (see
/// `tui::app::review::intraline_options`) so a line that shows emphasis in one
/// frontend shows the same emphasis in the other.
fn intraline_options() -> diffgrid::enrich::IntralineOptions {
    diffgrid::enrich::IntralineOptions::default().with_join_gap(0)
}

/// A row's content runs: gutter stripped, and with the background padding a
/// terminal needs to carry a row's tint to the edge of its viewport dropped
/// too (a `Container` does that here).
fn content_spans(spans: Vec<Span<'_>>, gutter: usize) -> Vec<DiffSpanDto> {
    strip_gutter(spans, gutter)
        .into_iter()
        .filter(|s| !matches!(s.style.role, Role::Gutter(_) | Role::Padding) && !s.is_empty())
        .map(Into::into)
        .collect()
}

/// The new-side file text, so gaps between hunks can be revealed.
///
/// Borrows its lines out of one owned `String`, and ignores the path: the
/// bridge lays out one file per call, so there is only ever one file to serve.
struct TextSource<'a> {
    lines: Vec<&'a str>,
}

impl<'a> TextSource<'a> {
    fn new(text: &'a str) -> Self {
        // A trailing newline terminates the last line rather than starting an
        // empty one, so `line_count` matches what an editor would report.
        let text = text.strip_suffix('\n').unwrap_or(text);
        let lines = if text.is_empty() {
            Vec::new()
        } else {
            text.split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l))
                .collect()
        };
        Self { lines }
    }
}

impl FileSource for TextSource<'_> {
    fn line_count(&self, _path: &str) -> Option<usize> {
        Some(self.lines.len())
    }

    fn line(&self, _path: &str, lineno: LineNo) -> Option<Cow<'_, str>> {
        self.lines.get(lineno.index()).map(|l| Cow::Borrowed(*l))
    }

    fn has_content(&self, _path: &str) -> bool {
        true
    }
}

/// The whole pipeline for one file: enrich, lay out, replay expansions, emit.
fn build(
    mut file: FileDiff<'static>,
    mode: LayoutMode,
    file_text: Option<&str>,
    expansions: &[DiffExpansion],
    tab_width: usize,
) -> DiffLayoutDto {
    diffgrid::enrich::word_diff(&mut file, &intraline_options());

    let owned_source = file_text.map(TextSource::new);
    let source: &dyn FileSource = match &owned_source {
        Some(s) => s,
        None => &(),
    };

    let mut layout = FileLayout::build(&file, source, &LayoutOptions::new(mode));
    for e in expansions {
        layout.expand(
            &file,
            source,
            GapIdx::new(e.gap as usize),
            (&e.action).into(),
        );
    }

    let ctx = RowContext::new(source)
        .with_gutter(content_gutter())
        .with_tab_width(tab_width);
    let rows = (0..layout.row_count())
        .filter_map(|i| emit_row(&file, &layout, LogicalIdx::new(i), mode, &ctx))
        .collect();

    DiffLayoutDto {
        rows,
        selectable: layout.selectable_count() as u32,
        has_hidden_context: file.is_expandable(),
    }
}

/// One logical row, in whichever of the two shapes the mode calls for.
fn emit_row(
    file: &FileDiff<'static>,
    layout: &FileLayout,
    idx: LogicalIdx,
    mode: LayoutMode,
    ctx: &RowContext<'_>,
) -> Option<DiffRowDto> {
    let row = layout.row(idx)?;
    let kind = match row.kind() {
        RowKind::HunkHeader => DiffRowKind::HunkHeader,
        RowKind::Line => DiffRowKind::Line,
        RowKind::ExpandedContext => DiffRowKind::ExpandedContext,
        RowKind::ExpandControl => DiffRowKind::ExpandControl,
        RowKind::AlignmentGap => DiffRowKind::AlignmentGap,
        // Host-drawn attached blocks. This bridge attaches none, and `RowKind`
        // is `#[non_exhaustive]`, so drop anything else rather than guessing.
        _ => return None,
    };
    let is_control = matches!(kind, DiffRowKind::ExpandControl);
    let (hidden, can_up, can_down) = match row.gap().filter(|_| is_control) {
        Some(gap) => match layout.gap_display(file, gap).control {
            Some(c) => (
                c.hidden as u32,
                matches!(c.kind, GapKind::Leading | GapKind::Middle),
                matches!(c.kind, GapKind::Trailing | GapKind::Middle),
            ),
            None => (0, false, false),
        },
        None => (0, false, false),
    };

    let full_width = is_full_width(row.kind());
    // An expand control's text is a terminal affordance — glyphs centred in a
    // row of known width. Flutter builds buttons from the counts instead, so
    // its spans are deliberately not emitted.
    let (left, right) = if is_control {
        (DiffCellDto::absent(), DiffCellDto::absent())
    } else if mode == LayoutMode::SideBySide && !full_width {
        (
            cell(file, layout, idx, &row, Some(Side::Old), ctx),
            cell(file, layout, idx, &row, Some(Side::New), ctx),
        )
    } else {
        (
            cell(file, layout, idx, &row, None, ctx),
            DiffCellDto::absent(),
        )
    };

    Some(DiffRowDto {
        kind,
        full_width,
        left,
        right,
        gap: row.gap().map(|g| g.get() as u32),
        hidden,
        can_expand_up: can_up,
        can_expand_down: can_down,
    })
}

/// One cell: `side` selects a side-by-side half, `None` the inline reading.
fn cell(
    file: &FileDiff<'static>,
    layout: &FileLayout,
    idx: LogicalIdx,
    row: &LogicalRow<'_>,
    side: Option<Side>,
    ctx: &RowContext<'_>,
) -> DiffCellDto {
    let spans = match side {
        Some(s) => cell_spans(file, layout, idx, s, ctx),
        None => row_spans(file, layout, idx, ctx),
    };
    let half = side.and_then(|s| row.cell(s));
    let (sel, origin, old, new) = match (side, half) {
        (Some(_), Some(c)) => (c.sel, c.origin, c.old_lineno, c.new_lineno),
        (Some(_), None) => (None, None, None, None),
        (None, _) => (row.sel(), row.origin(), row.old_lineno(), row.new_lineno()),
    };
    DiffCellDto {
        // A side-by-side half with no cell is the blank one; the inline reading
        // always exists, and full-width rows announce themselves by kind.
        present: side.is_none() || half.is_some(),
        origin: origin.map(|o| match o {
            LineOrigin::Addition => ReviewLineOrigin::Addition,
            LineOrigin::Deletion => ReviewLineOrigin::Deletion,
            // `LineOrigin` is `#[non_exhaustive]`. An origin this build does not
            // know renders as an unchanged line rather than failing to compile
            // the day one is added.
            _ => ReviewLineOrigin::Context,
        }),
        old_lineno: old.map(|n| n.get() as u32),
        new_lineno: new.map(|n| n.get() as u32),
        sel: sel.map(|s| s.get() as u32),
        text: sel
            .and_then(|s| file.line(s))
            .map(|l| l.content.clone().into_owned())
            .unwrap_or_default(),
        spans: content_spans(spans, gutter_cols(row.kind(), &ctx.gutter)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::review::{ReviewHunkDto, ReviewLineDto};

    const RAW: &str = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,3 +1,3 @@ fn main
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;
";

    /// A diff whose two hunks sit 28 lines apart, so the region between them is
    /// elided and expandable.
    const GAPPED: &str = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-one
+ONE
@@ -30 +30 @@
-thirty
+THIRTY
";

    #[test]
    fn parse_cache_uses_exact_keys_and_bounds_entries_and_bytes() {
        let mut cache = ParseCache::default();
        let files = std::sync::Arc::new(Vec::new());
        cache.insert("old", files.clone());
        assert!(std::sync::Arc::ptr_eq(&cache.get("old").unwrap(), &files));
        assert!(cache.get("changed").is_none());
        cache.insert("second", files.clone());
        cache.insert("third", files.clone());
        assert!(cache.get("old").is_none());
        assert_eq!(cache.entries.len(), 2);
        let large = "x".repeat(ParseCache::MAX_BYTES);
        cache.insert(&large, files.clone());
        assert_eq!(cache.entries.len(), 1);
        cache.insert(&(large + "x"), files);
        assert_eq!(cache.entries.len(), 1);
        assert!(
            cache
                .entries
                .iter()
                .map(|(key, _)| key.len())
                .sum::<usize>()
                <= ParseCache::MAX_BYTES
        );
    }

    /// Concatenated span text of a cell — what the user actually reads.
    fn text_of(cell: &DiffCellDto) -> String {
        cell.spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn stub_file() -> ReviewFileDto {
        ReviewFileDto {
            display_path: "a.rs".into(),
            old_path: "a.rs".into(),
            new_path: "a.rs".into(),
            status: ReviewFileStatus::Modified,
            added: 1,
            removed: 1,
            hunks: vec![],
            is_binary: false,
            binary_mime: None,
        }
    }

    fn lay_out(
        raw: Option<&str>,
        file: ReviewFileDto,
        mode: DiffLayoutMode,
        file_text: Option<String>,
        expansions: Vec<DiffExpansion>,
    ) -> DiffLayoutDto {
        diff_rows(
            raw.map(str::to_string),
            file,
            mode,
            file_text,
            expansions,
            4,
        )
        .unwrap()
    }

    fn rows(raw: Option<&str>, file: ReviewFileDto, mode: DiffLayoutMode) -> DiffLayoutDto {
        lay_out(raw, file, mode, None, Vec::new())
    }

    /// The core contract: the raw text produces a hunk header plus one row per
    /// diff line, each carrying its line numbers, its origin and its content —
    /// with the gutter runs stripped, because Flutter draws that as a widget.
    #[test]
    fn inline_rows_carry_content_without_gutter_runs() {
        let out = rows(Some(RAW), stub_file(), DiffLayoutMode::Inline);
        assert_eq!(out.selectable, 4);
        assert_eq!(out.rows.len(), 5, "header + four lines");
        assert!(matches!(out.rows[0].kind, DiffRowKind::HunkHeader));
        assert_eq!(text_of(&out.rows[0].left), "@@ -1,3 +1,3 @@ fn main");

        let deletion = &out.rows[2];
        assert!(matches!(
            deletion.left.origin,
            Some(ReviewLineOrigin::Deletion)
        ));
        assert_eq!(deletion.left.old_lineno, Some(2));
        assert_eq!(deletion.left.new_lineno, None);
        // No leading `-` and no gutter padding: the sign is the host's to draw.
        assert_eq!(text_of(&deletion.left), "let b = 2;");
        assert_eq!(deletion.left.text, "let b = 2;");
        assert_eq!(deletion.left.sel, Some(1));
        assert!(!deletion.right.present, "inline rows have no right half");
    }

    /// The headline parity feature: a changed line's *differing* run is marked
    /// so the host can tint it distinctly from the rest of the line.
    #[test]
    fn word_diff_marks_only_the_changed_run() {
        let out = rows(Some(RAW), stub_file(), DiffLayoutMode::Inline);
        let emphasised: Vec<&str> = out
            .rows
            .iter()
            .flat_map(|r| r.left.spans.iter())
            .filter(|s| s.emphasis)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(
            emphasised,
            vec!["2", "3"],
            "only the digit differs between `let b = 2;` and `let b = 3;`"
        );
    }

    /// Side by side pairs the deletion with the addition on one row, and the
    /// hunk header stays a single full-width row rather than being drawn twice.
    #[test]
    fn side_by_side_pairs_the_change_and_keeps_headers_full_width() {
        let out = rows(Some(RAW), stub_file(), DiffLayoutMode::SideBySide);

        let header = &out.rows[0];
        assert!(header.full_width);
        assert!(!header.right.present, "a full-width row is emitted once");

        let changed = out
            .rows
            .iter()
            .find(|r| matches!(r.left.origin, Some(ReviewLineOrigin::Deletion)))
            .expect("the deletion must appear");
        assert_eq!(text_of(&changed.left), "let b = 2;");
        assert_eq!(text_of(&changed.right), "let b = 3;");
        assert_eq!(changed.left.old_lineno, Some(2));
        assert_eq!(changed.right.new_lineno, Some(2));
    }

    /// Without the raw text — an older server — the wire model still lays out,
    /// enrichment included, so emphasis does not silently disappear.
    #[test]
    fn falls_back_to_the_wire_model_when_raw_is_absent() {
        let mut file = stub_file();
        file.hunks = vec![ReviewHunkDto {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            header: "fn main".into(),
            lines: vec![
                ReviewLineDto {
                    origin: ReviewLineOrigin::Deletion,
                    old_lineno: Some(1),
                    new_lineno: None,
                    content: "let b = 2;".into(),
                },
                ReviewLineDto {
                    origin: ReviewLineOrigin::Addition,
                    old_lineno: None,
                    new_lineno: Some(1),
                    content: "let b = 3;".into(),
                },
            ],
        }];
        let out = rows(None, file, DiffLayoutMode::Inline);
        assert_eq!(out.selectable, 2);
        assert_eq!(text_of(&out.rows[1].left), "let b = 2;");
        assert!(out.rows[1].left.spans.iter().any(|s| s.emphasis));
    }

    /// Tabs are expanded at layout time (so a terminal and Flutter agree on the
    /// stops), but the snippet text keeps the original bytes — the server's
    /// re-anchor searches the file for it and would never find a detabbed copy.
    #[test]
    fn tabs_expand_in_spans_but_not_in_the_snippet_text() {
        let raw = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-\tx
+\ty
";
        let out = rows(Some(raw), stub_file(), DiffLayoutMode::Inline);
        let del = &out.rows[1].left;
        assert_eq!(text_of(del), "    x");
        assert_eq!(del.text, "\tx");
    }

    /// A collapsed gap offers a control; replaying an expansion puts real file
    /// lines in its place.
    #[test]
    fn a_gap_offers_a_control_and_expanding_reveals_file_lines() {
        let text: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        let collapsed = lay_out(
            Some(GAPPED),
            stub_file(),
            DiffLayoutMode::Inline,
            Some(text.clone()),
            Vec::new(),
        );
        assert!(collapsed.has_hidden_context);
        let control = collapsed
            .rows
            .iter()
            .find(|r| matches!(r.kind, DiffRowKind::ExpandControl))
            .expect("a collapsed gap must offer a control");
        assert!(
            control.can_expand_up && control.can_expand_down,
            "a gap between two hunks reveals in both directions"
        );
        assert!(control.hidden > 0);
        assert!(
            control.left.spans.is_empty(),
            "controls are buttons in Flutter, not centred glyphs"
        );
        let gap = control.gap.expect("a control names its gap");

        let expanded = lay_out(
            Some(GAPPED),
            stub_file(),
            DiffLayoutMode::Inline,
            Some(text),
            vec![DiffExpansion {
                gap,
                action: DiffExpandAction::All,
            }],
        );
        let revealed: Vec<DiffCellDto> = expanded
            .rows
            .into_iter()
            .filter(|r| matches!(r.kind, DiffRowKind::ExpandedContext))
            .map(|r| r.left)
            .collect();
        assert!(revealed.iter().any(|c| text_of(c) == "line 15"));
        assert!(
            revealed.iter().all(|c| c.sel.is_none()),
            "revealed context is display-only, never commentable"
        );
    }

    /// The gutter is stripped by *width*, so a line number wider than the
    /// gutter declares would leak digits into the code. Revealed context is the
    /// only place this can happen — its numbers come from the file, not the
    /// hunks the gutter was sized against — so it is pinned here.
    #[test]
    fn a_four_digit_line_number_does_not_leak_into_the_content() {
        let raw = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-one
+ONE
@@ -2000 +2000 @@
-far
+FAR
";
        let text: String = (1..=2100).map(|n| format!("line {n}\n")).collect();
        let out = lay_out(
            Some(raw),
            stub_file(),
            DiffLayoutMode::Inline,
            Some(text),
            vec![DiffExpansion {
                gap: 1,
                action: DiffExpandAction::All,
            }],
        );
        let wide = out
            .rows
            .iter()
            .find(|r| r.left.new_lineno == Some(1500))
            .expect("line 1500 sits inside the revealed gap");
        assert_eq!(text_of(&wide.left), "line 1500");
    }

    /// Without the file text there is nothing to reveal *from*, so no control is
    /// offered — but the host is still told hidden context exists, so it knows
    /// to go and fetch it.
    #[test]
    fn no_expand_controls_without_file_text() {
        let out = rows(Some(GAPPED), stub_file(), DiffLayoutMode::Inline);
        assert!(out.has_hidden_context);
        assert!(!out
            .rows
            .iter()
            .any(|r| matches!(r.kind, DiffRowKind::ExpandControl)));
    }
}
