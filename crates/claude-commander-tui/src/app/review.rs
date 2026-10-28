//! Full-screen review-diff-and-comment view.
//!
//! Presentation only: [`DiffReviewState`] holds what's on screen and is opened
//! via `CommanderService::open_review`. The view is hosted as a maximised
//! modal (`Modal::ReviewDiff`); all diff composition, parsing, and comment
//! logic lives in the library.
//!
//! # What is here, and what is `diffgrid`'s
//!
//! Everything about *laying a diff out* — logical rows, gap expansion, the
//! side-by-side pairing, soft wrapping, role-tagged spans and hit-testing —
//! belongs to [`diffgrid`] and is reached through
//! [`FileLayout`](diffgrid::layout::FileLayout) and [`Grid`](diffgrid::wrap::Grid).
//! What stays here is the *session* half: which file is shown, where the cursor
//! is, which comments are staged and where their boxes go, what is marked
//! reviewed, and the mapping from `diffgrid`'s semantic roles onto the theme
//! (`ReviewPalette`) and onto `syntect` (`SyntectHighlighter`).
//!
//! Comment boxes reach the layout as **attached blocks**: the state tells
//! `diffgrid` only an anchor line and an opaque key, and answers a height
//! callback at render width. That is what keeps every row↔line mapping — cursor,
//! scroll, click — in step with what is drawn, in both layouts, without this
//! module walking rows itself.

use super::*;
use crossterm::event::KeyEvent;

use crate::syntax_highlight::{SyntectHighlighter, warm_highlight_cache};
use crate::theme::ReviewPalette;
use claude_commander_core::api::{DiffSide, NewComment};
use claude_commander_core::comment::{Comment, CommentSide, CommentStatus};
use claude_commander_core::git::{DiffLine, FileDiff, FileStatus, LineOrigin, ParsedDiff};
use claude_commander_core::term_caps::ColorMode;
use claude_commander_protocol::api::ReviewedFileMark;
use claude_commander_viewmodel::review::{self, DiffHashes};
use diffgrid::layout::{
    Attach, AttachKey, ExpandAction, FileLayout, FileSource, LayoutMode, LayoutOptions, RowKind,
};
use diffgrid::style::{
    GutterSides, GutterSpec, Highlighter, Palette, Role, RowContext, SpanStyle, cell_spans,
    gutter_cols, is_full_width, row_spans,
};
use diffgrid::wrap::{Grid, Hit, WrapOptions, fit_spans, wrap_row};
use diffgrid::{GapIdx, LineNo, RowIdx, SelIdx, SelRange, Side};
use rayon::prelude::*;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use tui_input::Input;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// Gutter / badge / box marker for a staged comment. An asterisk is the
/// conventional note/comment marker and stays crisp at one cell.
const COMMENT_MARKER: char = '*';
/// Marker for a drifted comment (its snippet could no longer be located).
const DRIFT_MARKER: char = '⚠';

/// Fallback viewport height for cursor-follow before the first render has
/// reported the real one. Every scroll decision takes the height as an argument
/// (`diffgrid` has no viewport constant), so this is only ever the value used for
/// keypresses that arrive before a frame has been drawn.
const DEFAULT_BODY_HEIGHT: usize = 20;

/// Fallback body width, used for the same window as [`DEFAULT_BODY_HEIGHT`].
const DEFAULT_BODY_WIDTH: usize = 120;

/// Columns a tab expands to in diff content. `diffgrid` expands tabs at render
/// (a tab's width depends on the column it starts at, so it cannot be resolved
/// earlier) and the review view has no reason to differ from the editor default.
const TAB_WIDTH: usize = 4;

/// Which column has keyboard focus inside the review view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewFocus {
    FileList,
    Body,
}

/// A clickable button in the review-view footer. Review keys are matched by raw
/// `KeyCode` in [`App::handle_review_key`] rather than routed through
/// `BindableAction`, so a click replays the key it labels: `key` is fed
/// straight into the same handler.
#[derive(Debug, Clone, Copy)]
pub struct ReviewButton {
    pub rect: Rect,
    pub key: KeyEvent,
}

/// One segment of the review footer: a non-actionable key legend (`Plain`), a
/// clickable `Button` that replays `key` on click, or a transient status message
/// (`Toast`) that momentarily claims the bar.
enum FooterItem {
    Plain(&'static str),
    Button { label: &'static str, key: KeyEvent },
    Toast(String),
}

/// The key of the footer button containing `(col, row)`, or `None`. First
/// match wins (buttons never overlap). Mirrors [`crate::hotkey::button_at`]
/// but yields the raw `KeyEvent` review buttons carry.
pub(super) fn review_button_at(buttons: &[ReviewButton], col: u16, row: u16) -> Option<KeyEvent> {
    buttons.iter().find_map(|b| {
        let r = b.rect;
        (col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height).then_some(b.key)
    })
}

impl FooterItem {
    fn button(label: &'static str, key: KeyEvent) -> Self {
        Self::Button { label, key }
    }

    /// Styled spans plus their total display width. Buttons bracket the key
    /// letter (or append the key when absent); plain legends render as-is.
    fn render(&self, base: Style, accent: Style) -> (Vec<Span<'static>>, u16) {
        match self {
            FooterItem::Plain(text) => {
                let span = Span::styled((*text).to_string(), base);
                let width = span.width() as u16;
                (vec![span], width)
            }
            FooterItem::Toast(text) => {
                let span = Span::styled(text.clone(), base.add_modifier(Modifier::BOLD));
                let width = span.width() as u16;
                (vec![span], width)
            }
            FooterItem::Button { label, key } => {
                let kb = claude_commander_core::config::keybindings::KeyBinding::new(
                    key.code,
                    key.modifiers,
                );
                let seg = crate::hotkey::segment_with_key(label, Some(&kb));
                let spans = crate::hotkey::hotkey_spans(&seg, base, accent);
                let width = spans.iter().map(|s| s.width() as u16).sum();
                (spans, width)
            }
        }
    }
}

/// How the diff body is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewLayout {
    /// GitHub-style unified inline diff (default).
    Inline,
    /// Old | new split columns.
    SideBySide,
}

/// Columns kept clear at the right edge when soft-wrapping, so wrapped text
/// doesn't butt directly against the body border (the background fill still
/// extends to the edge; only the wrap point is pulled in).
const WRAP_RIGHT_MARGIN: usize = 2;

/// Columns between the two halves of the side-by-side layout: `│` with a space
/// either side.
const SBS_SEPARATOR: usize = 3;

/// The inline gutter: an edge bar, the comment-marker slot, both four-column
/// line numbers, and the `+`/`-` sign with a space after it so code doesn't butt
/// against it — 14 columns, derived by [`GutterSpec::cols`] rather than written
/// down anywhere.
fn inline_gutter() -> GutterSpec {
    GutterSpec::default()
}

/// One side-by-side half's gutter: a four-column line number and a space. No
/// edge, marker or sign — the two halves already say which side they are.
fn sbs_gutter() -> GutterSpec {
    // Field assignment rather than a struct literal: `GutterSpec` is
    // `#[non_exhaustive]`, which is the point — a field added upstream keeps its
    // default here instead of failing to compile.
    let mut spec = GutterSpec::default().with_sides(GutterSides::One);
    spec.edge = false;
    spec.marker = false;
    spec.sign = false;
    spec.pad = 0;
    spec
}

/// The key an in-progress comment draft is attached to the layout under.
///
/// `u128::MAX` cannot collide with a comment's own key, which is a UUID: every
/// UUID has fixed version and variant bits, so the all-ones value is not one.
const DRAFT_ATTACH_KEY: AttachKey = AttachKey::new(u128::MAX);

/// The layout key for a saved comment's inline box.
fn comment_attach_key(id: uuid::Uuid) -> AttachKey {
    AttachKey::new(id.as_u128())
}

/// A review image's load state, cached on `App` keyed by (display path, side).
/// Lives on `App` (not `DiffReviewState`) because `StatefulProtocol` isn't
/// `Clone` and `DiffReviewState` derives `Clone`.
pub(crate) enum ImageEntry {
    /// Fetch + decode in flight; nothing to draw yet.
    Pending,
    /// Fetch or decode failed; the string is a short reason for display.
    Failed(String),
    /// Decoded and ready: a resize protocol bound to the detected terminal
    /// graphics capability. Boxed — it's far larger than the other variants.
    Ready(Box<ratatui_image::protocol::StatefulProtocol>),
}

/// The side of `file`'s image to display: forced for added/deleted files
/// (which have only one side), else the user's toggle preference.
pub(super) fn shown_image_side(file: &FileDiff, pref: DiffSide) -> DiffSide {
    match file.status {
        FileStatus::Added => DiffSide::New,
        FileStatus::Deleted => DiffSide::Old,
        _ => pref,
    }
}

/// The path of `file` on a given side (differs only for renames).
fn side_path(file: &FileDiff, side: DiffSide) -> &str {
    match side {
        DiffSide::Old => &file.old_path,
        DiffSide::New => &file.new_path,
    }
}

/// Working-tree file content, keyed by display path, as `diffgrid`'s source for
/// revealed context.
///
/// Populated lazily by [`App::ensure_review_file_lines`] once a fetch completes;
/// until then a file simply reports no content, so no context is revealed and no
/// expand controls are offered — exactly what
/// [`FileSource::has_content`] is for.
#[derive(Debug, Clone, Default)]
struct FileLines(HashMap<String, std::sync::Arc<Vec<String>>>);

impl FileLines {
    /// Whether `path`'s content has already been fetched.
    fn is_loaded(&self, path: &str) -> bool {
        self.0.contains_key(path)
    }
}

impl FileSource for FileLines {
    fn line_count(&self, path: &str) -> Option<usize> {
        self.0.get(path).map(|l| l.len())
    }

    fn line(&self, path: &str, lineno: LineNo) -> Option<Cow<'_, str>> {
        self.0
            .get(path)?
            .get(lineno.index())
            .map(|s| Cow::Borrowed(s.as_str()))
    }
}

/// One file's `diffgrid` state: the word-diffed model, its logical layout, and
/// the expansions the user has asked for.
///
/// Held per display path rather than rebuilt on demand because a [`FileLayout`]
/// is where revealed context lives, and that has to survive navigating away and
/// back — which is how the old `(path, gap)` reveal map behaved.
#[derive(Debug, Clone)]
struct FileView {
    /// The diff as `diffgrid` models it, with intra-line word diffs already
    /// applied. Owned (`'static`), so this struct is not self-referential.
    file: diffgrid::FileDiff<'static>,
    layout: FileLayout,
    /// The expand actions the user has applied, in order.
    ///
    /// Kept because a [`FileLayout`]'s revealed state is write-only from
    /// outside — there is no setter and no getter — so replaying the *requests*
    /// is the only way to reproduce it after a rebuild. Rebuilds happen when the
    /// layout mode changes (inline ⇄ side-by-side bake different row structures)
    /// and when the file's content finally arrives, and both must keep what the
    /// user revealed.
    expands: Vec<(GapIdx, ExpandAction)>,
    /// The line count the layout was last built against, so an arriving file
    /// triggers exactly one rebuild.
    source_len: Option<usize>,
    /// Digest of the attached blocks, so they are only re-spliced when the
    /// comment set or the draft anchor actually changes. `None` forces a
    /// re-splice after a rebuild dropped them.
    attach_sig: Option<u64>,
}

impl FileView {
    /// A view over `file`, laid out against whatever content `source` can
    /// currently supply.
    fn new(file: diffgrid::FileDiff<'static>, source: &FileLines, mode: LayoutMode) -> Self {
        let mut view = Self {
            layout: FileLayout::build(&file, source, &LayoutOptions::new(mode)),
            file,
            expands: Vec::new(),
            source_len: None,
            attach_sig: None,
        };
        view.source_len = view.source_line_count(source);
        view
    }

    /// The `FileSource` key for this file: the new-side path, per the trait's
    /// new-side-only contract.
    fn source_line_count(&self, source: &FileLines) -> Option<usize> {
        source.line_count(self.file.side_path(Side::New))
    }

    /// Whether the layout no longer matches the mode or the content it was
    /// built against.
    fn is_stale(&self, source: &FileLines, mode: LayoutMode) -> bool {
        self.layout.mode() != mode || self.source_len != self.source_line_count(source)
    }

    /// Rebuild the layout and replay every expansion onto it.
    fn rebuild(&mut self, source: &FileLines, mode: LayoutMode) {
        self.layout = FileLayout::build(&self.file, source, &LayoutOptions::new(mode));
        for (gap, action) in self.expands.clone() {
            self.layout.expand(&self.file, source, gap, action);
        }
        self.source_len = self.source_line_count(source);
        self.attach_sig = None;
    }

    /// Apply an expand action, recording it so a later rebuild keeps it.
    fn expand(&mut self, source: &FileLines, gap: GapIdx, action: ExpandAction) {
        self.expands.push((gap, action));
        self.layout.expand(&self.file, source, gap, action);
    }
}

/// An in-progress comment, captured against a selectable-line range.
#[derive(Debug, Clone)]
pub struct CommentDraft {
    /// Editable comment text plus cursor (insert/delete/navigate in place),
    /// backed by `tui-input`.
    pub input: Input,
    /// Inclusive selectable-line index range the comment applies to.
    pub range: (usize, usize),
}

/// State backing the full-screen review view.
#[derive(Debug, Clone)]
pub struct DiffReviewState {
    pub session_id: SessionId,
    pub title: String,
    /// Base the diff was computed against (branch/sha/HEAD), for the header.
    pub base: String,
    pub diff: ParsedDiff,
    pub comments: Vec<Comment>,
    /// xxh3 hash of the raw diff this view was last built from. Lets the
    /// background refresh skip a rebuild when the working tree is unchanged.
    pub content_hash: u64,
    /// Display paths of files marked reviewed (mirrors the persisted store;
    /// stale marks are pruned by the service before the view opens).
    pub reviewed: HashSet<String>,
    /// Changed whenever this view changes `reviewed` itself (a toggle, a diff
    /// refresh). A background poll of the store records the generation it was
    /// spawned at and is dropped if the view has moved on, so a read that
    /// raced a toggle can't revert it. Drawn from [`next_reviewed_gen`], so a
    /// reopened view never shares a value with the one a poll was spawned for.
    pub reviewed_gen: u64,
    /// Every file's hash in `diff`, computed once per diff so folding polled
    /// marks in each second is a lookup rather than a re-hash.
    hashes: DiffHashes,
    /// Index into `diff.files` of the file shown in the body.
    pub selected_file: usize,
    /// First visible body row (scroll offset).
    pub scroll: u16,
    pub focus: ReviewFocus,
    /// Cursor position as a selectable-line index within the current file.
    pub cursor: usize,
    /// `Some(anchor)` while in visual (range-select) mode; the active end is
    /// `cursor`.
    pub visual_anchor: Option<usize>,
    /// `Some` while the comment box is open.
    pub comment: Option<CommentDraft>,
    pub layout: ReviewLayout,
    /// Which side of a binary image to show. Clamped per file: added files
    /// always show New, deleted always show Old (see [`Self::shown_image_side`]).
    pub image_side: DiffSide,
    /// File tree built from the diff's paths (single-child directory chains
    /// compressed, lazygit-style).
    file_tree: Vec<TreeNode>,
    /// Paths of directory nodes the user has collapsed.
    collapsed: HashSet<String>,
    /// Cursor over the flattened, currently-visible tree rows.
    tree_cursor: usize,
    /// First visible tree row (scroll offset for the file pane).
    tree_scroll: u16,
    /// Comment ids whose inline box is collapsed (absent = expanded).
    collapsed_comments: HashSet<uuid::Uuid>,
    /// One [`FileView`] per display path, built on first use (or handed over
    /// wholesale by the open-review precompute, see [`Self::prime_views`]).
    ///
    /// Interior-mutable because the renderer holds `&self` and both the word
    /// diff and the layout are memoized here. Keyed by path rather than by
    /// index so a refresh that reorders files keeps each file's revealed
    /// context.
    views: RefCell<HashMap<String, FileView>>,
    /// Body pane inner `(width, height)` from the most recent render.
    ///
    /// The height is not decoration: every scroll decision takes the viewport
    /// as an argument, because `diffgrid` deliberately has no viewport constant
    /// — the fixed `BODY_VIEWPORT = 20` this replaces made cursor-follow wrong
    /// on every terminal that was not about 22 rows tall.
    body: Cell<(usize, usize)>,
    /// Lazily-fetched working-tree lines per display path, the source revealed
    /// context is read from. Cleared on `refresh_diff`.
    file_lines: FileLines,
}

/// A node in the file tree: either a directory (with children) or a file leaf
/// (`file_index` into `diff.files`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeNode {
    /// Segment label (compressed directories join with `/`).
    name: String,
    /// Full path of this node, used as the collapse-set key.
    path: String,
    file_index: Option<usize>,
    children: Vec<TreeNode>,
}

/// A flattened, visible tree row for rendering and navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeRow {
    Dir {
        depth: usize,
        path: String,
        name: String,
        collapsed: bool,
    },
    File {
        depth: usize,
        index: usize,
        name: String,
    },
}

impl DiffReviewState {
    pub fn new(
        session_id: SessionId,
        title: String,
        base: String,
        diff: ParsedDiff,
        comments: Vec<Comment>,
    ) -> Self {
        let file_tree = build_file_tree(&diff.files);
        let hashes = DiffHashes::of(&diff);
        // Body starts on the first file in tree order so it shows something.
        let selected_file = first_file_index(&file_tree).unwrap_or(0);
        Self {
            session_id,
            title,
            base,
            diff,
            comments,
            content_hash: 0,
            reviewed: HashSet::new(),
            reviewed_gen: next_reviewed_gen(),
            hashes,
            selected_file,
            scroll: 0,
            focus: ReviewFocus::FileList,
            cursor: 0,
            visual_anchor: None,
            comment: None,
            layout: ReviewLayout::Inline,
            image_side: DiffSide::New,
            file_tree,
            collapsed: HashSet::new(),
            tree_cursor: 0,
            tree_scroll: 0,
            collapsed_comments: HashSet::new(),
            views: RefCell::new(HashMap::new()),
            // Reasonable defaults until the first render reports the real pane
            // size (keypress math before any render then self-heals).
            body: Cell::new((DEFAULT_BODY_WIDTH, DEFAULT_BODY_HEIGHT)),
            file_lines: FileLines::default(),
        }
    }

    /// Install fully word-diffed `diffgrid` models (one per file in `diff.files`
    /// order). Called once after the open-review background task builds them
    /// off-thread, so the first navigation to each file is instant. Uses
    /// interior mutability so it can run on the freshly built (immutable) state
    /// before it's boxed into the modal.
    pub(super) fn prime_views(&self, models: Vec<diffgrid::FileDiff<'static>>) {
        let mode = self.layout_mode();
        let mut views = self.views.borrow_mut();
        for (i, file) in models.into_iter().enumerate() {
            let Some(path) = self.diff.files.get(i).map(FileDiff::display_path) else {
                continue;
            };
            views.insert(
                path.to_string(),
                FileView::new(file, &self.file_lines, mode),
            );
        }
    }

    /// The `diffgrid` layout mode for the view's current layout choice.
    fn layout_mode(&self) -> LayoutMode {
        match self.layout {
            ReviewLayout::Inline => LayoutMode::Inline,
            ReviewLayout::SideBySide => LayoutMode::SideBySide,
        }
    }

    /// Ensure the current file has a [`FileView`], rebuilt for the layout mode
    /// and re-read against the file source if either has changed since it was
    /// built, then run `f` over it.
    ///
    /// `None` only when there is no current file. `f` must not re-enter
    /// `views` — every accessor below goes through this one borrow.
    fn with_view<R>(&self, f: impl FnOnce(&FileView) -> R) -> Option<R> {
        let path = self.current_file()?.display_path().to_string();
        let mode = self.layout_mode();
        {
            let mut views = self.views.borrow_mut();
            match views.get_mut(&path) {
                Some(v) if v.is_stale(&self.file_lines, mode) => v.rebuild(&self.file_lines, mode),
                Some(_) => {}
                None => {
                    let model = word_diffed(self.current_file()?);
                    views.insert(path.clone(), FileView::new(model, &self.file_lines, mode));
                }
            }
            self.sync_attachments(views.get_mut(&path)?);
        }
        Some(f(self.views.borrow().get(&path)?))
    }

    /// Same as [`Self::with_view`], for the calls that mutate the layout.
    fn with_view_mut<R>(&mut self, f: impl FnOnce(&mut FileView, &FileLines) -> R) -> Option<R> {
        self.with_view(|_| ())?;
        let path = self.current_file()?.display_path().to_string();
        let mut views = self.views.borrow_mut();
        let view = views.get_mut(&path)?;
        Some(f(view, &self.file_lines))
    }

    /// Re-splice the layout's attached blocks when the comment set or the draft
    /// anchor has moved.
    ///
    /// Guarded by a digest because attaching is O(rows) per block: without it a
    /// file with a dozen comments would re-splice a dozen times on every frame.
    fn sync_attachments(&self, view: &mut FileView) {
        let wanted = self.wanted_attachments();
        let sig = attachment_signature(&wanted);
        if view.attach_sig == Some(sig) {
            return;
        }
        view.attach_sig = Some(sig);
        // No bulk setter, so clear and re-attach: several blocks sharing one
        // anchor stack in attachment order, which is the order they must be
        // drawn in (saved comments first, then the draft).
        let existing: Vec<AttachKey> = view.layout.attachments().map(|(_, k)| k).collect();
        for key in existing {
            view.layout.detach(key);
        }
        for (sel, key) in wanted {
            view.layout.attach(Attach::Below(sel), key);
        }
    }

    /// The blocks that should be attached to the current file, in draw order:
    /// each saved comment below the last line of its range, then the
    /// in-progress draft below the last line of its selection.
    fn wanted_attachments(&self) -> Vec<(SelIdx, AttachKey)> {
        let mut out: Vec<(SelIdx, AttachKey)> = Vec::new();
        let Some(file) = self.current_file() else {
            return out;
        };
        let display = file.display_path();
        let staged = || {
            self.comments
                .iter()
                .filter(|a| a.status != CommentStatus::Applied && a.file == display)
        };
        // The overwhelmingly common case, and this runs on every keypress:
        // anchoring needs the file's whole line list, which is not worth
        // building to conclude there is nothing to anchor.
        if self.comment.is_none() && staged().next().is_none() {
            return out;
        }
        let lines = self.selectable_lines();
        for ann in staged() {
            if let Some(idx) = self.comment_anchor_index(ann, &lines) {
                out.push((SelIdx::new(idx), comment_attach_key(ann.id)));
            }
        }
        if let Some(anchor) = self.draft_anchor() {
            out.push((SelIdx::new(anchor), DRAFT_ATTACH_KEY));
        }
        out
    }

    /// Height of the attached block `key` at `width` display columns — the
    /// callback [`Grid::build`] resolves attachments through. `0` hides a block
    /// whose comment has gone away without detaching it.
    fn attached_height(&self, key: AttachKey, width: usize) -> usize {
        if key == DRAFT_ATTACH_KEY {
            return self.comment.as_ref().map_or(0, |d| {
                comment_draft_box_height(&super::input_with_caret(&d.input), width)
            });
        }
        self.comment_by_key(key).map_or(0, |ann| {
            comment_box_height(ann, self.is_comment_collapsed(ann.id), width)
        })
    }

    /// The staged comment an attachment key refers to.
    fn comment_by_key(&self, key: AttachKey) -> Option<&Comment> {
        self.comments
            .iter()
            .find(|a| comment_attach_key(a.id) == key)
    }

    /// Body pane inner width from the last render.
    fn body_width(&self) -> usize {
        self.body.get().0
    }

    /// Body pane inner height from the last render — the viewport every scroll
    /// decision is made against.
    fn body_height(&self) -> usize {
        self.body.get().1
    }

    /// The wrap options the body is laid out with at the current pane size.
    fn wrap_options(&self) -> WrapOptions {
        WrapOptions::new(self.body_width())
            .with_right_margin(WRAP_RIGHT_MARGIN)
            .with_separator(SBS_SEPARATOR)
    }

    /// The span-emission context for the current file.
    ///
    /// `markers` is passed in rather than built here because [`RowContext`]
    /// borrows it, so it has to outlive the context — and only the caller has a
    /// stack frame long enough.
    fn row_context<'s>(
        &'s self,
        highlighter: Option<&'s dyn Highlighter>,
        language: Option<&'s str>,
        markers: &'s dyn Fn(SelIdx) -> Option<char>,
        focused: bool,
    ) -> RowContext<'s> {
        let gutter = match self.layout {
            ReviewLayout::Inline => inline_gutter(),
            ReviewLayout::SideBySide => sbs_gutter(),
        };
        let (lo, hi) = self.selection();
        let mut ctx = RowContext::new(&self.file_lines)
            .with_gutter(gutter)
            .with_tab_width(TAB_WIDTH)
            .with_width(self.body_width())
            .with_markers(markers)
            // Selection is only shown while the body has focus, so the file
            // list's own cursor is the only highlighted thing when it is active.
            .with_selection(focused.then(|| SelRange::new(SelIdx::new(lo), SelIdx::new(hi))));
        if let Some(h) = highlighter {
            ctx = ctx.with_highlighter(h, language);
        }
        ctx
    }

    /// Run `f` over the current file's model, layout and a freshly wrapped
    /// [`Grid`] at the current pane size.
    ///
    /// The grid is rebuilt per call rather than cached: it is a pure function of
    /// the layout, the width and the attached blocks' heights, and the draft box
    /// changes height on every keystroke without the layout's generation moving,
    /// so a cached grid would need invalidating on exactly the events that make
    /// caching worthless.
    fn with_grid<R>(&self, f: impl FnOnce(&FileView, &Grid) -> R) -> Option<R> {
        let markers = |_: SelIdx| None;
        let opts = self.wrap_options();
        self.with_view(|view| {
            // No highlighter: `Grid::build` measures widths off the model, and
            // highlighting changes colours, never widths.
            let ctx = self.row_context(None, None, &markers, false);
            let grid = Grid::build(&view.file, &view.layout, &ctx, &opts, |key, width| {
                self.attached_height(key, width)
            });
            f(view, &grid)
        })
    }

    /// Replace the displayed diff with a freshly composed one (the working
    /// tree changed while the view stayed open), preserving navigation state
    /// where it still makes sense: the body stays on the same file by path, the
    /// cursor and scroll clamp into the new content, collapsed directories and
    /// reviewed marks are kept, and any in-progress visual selection is dropped
    /// (line indices may have moved). Render caches are reset and re-primed from
    /// the precomputed `models`.
    pub(super) fn refresh_diff(
        &mut self,
        diff: ParsedDiff,
        comments: Vec<Comment>,
        reviewed: HashSet<String>,
        models: Vec<diffgrid::FileDiff<'static>>,
        content_hash: u64,
    ) {
        let prev_path = self.current_file().map(|f| f.display_path().to_string());
        let mut finished_before = Vec::new();
        collect_completed_dirs(&self.file_tree, None, &self.reviewed, &mut finished_before);
        self.hashes = DiffHashes::of(&diff);
        self.diff = diff;
        self.comments = comments;
        self.reviewed = reviewed;
        self.reviewed_gen = next_reviewed_gen();
        self.content_hash = content_hash;
        self.file_tree = build_file_tree(&self.diff.files);
        // Fold directories this refresh finished (marks set while it was
        // away), but leave alone one that was already finished: the user may
        // have expanded it to re-read, and an unrelated edit shouldn't shut it.
        let mut finished = Vec::new();
        collect_completed_dirs(&self.file_tree, None, &self.reviewed, &mut finished);
        self.collapsed.extend(
            finished
                .into_iter()
                .filter(|d| !finished_before.contains(d)),
        );
        self.clamp_tree_cursor();
        // Hunk boundaries (and gap indices) may have moved, so every file's
        // layout and revealed context is dropped and its content re-fetched
        // lazily on the next expand.
        self.views.borrow_mut().clear();
        self.file_lines.0.clear();
        self.prime_views(models);
        self.visual_anchor = None;
        // Re-locate the file that was on screen by its path; fall back to the
        // first file when it left the diff.
        self.selected_file = prev_path
            .and_then(|p| self.diff.files.iter().position(|f| f.display_path() == p))
            .or_else(|| first_file_index(&self.file_tree))
            .unwrap_or(0)
            .min(self.diff.files.len().saturating_sub(1));
        // Clamp the cursor into the (possibly shorter) current file, keep it in
        // view, and resync the tree cursor onto the shown file.
        let count = self.selectable_count();
        self.cursor = self.cursor.min(count.saturating_sub(1));
        self.follow_cursor();
        self.sync_tree_cursor_to_file();
    }

    /// The currently-visible tree rows (respecting collapsed directories).
    fn visible_rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        flatten_tree(&self.file_tree, 0, &self.collapsed, &mut rows);
        rows
    }

    /// Point the body at `idx`, resetting the body cursor/scroll/selection if
    /// it actually changed.
    fn set_body_file(&mut self, idx: usize) {
        if self.selected_file != idx {
            self.selected_file = idx;
            self.scroll = 0;
            self.cursor = 0;
            self.visual_anchor = None;
        }
    }

    /// Move the tree cursor over visible rows; landing on a file shows it.
    fn tree_move(&mut self, down: bool) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            return;
        }
        self.tree_cursor = if down {
            (self.tree_cursor + 1).min(rows.len() - 1)
        } else {
            self.tree_cursor.saturating_sub(1)
        };
        self.follow_tree_cursor(rows.len());
        if let Some(TreeRow::File { index, .. }) = rows.get(self.tree_cursor) {
            self.set_body_file(*index);
        }
    }

    /// Enter/Space on a directory row toggles its collapsed state.
    fn tree_activate(&mut self) {
        let rows = self.visible_rows();
        if let Some(TreeRow::Dir {
            path, collapsed, ..
        }) = rows.get(self.tree_cursor)
        {
            if *collapsed {
                self.collapsed.remove(path);
            } else {
                self.collapsed.insert(path.clone());
            }
            // The toggled dir keeps its index; clamp just in case.
            self.clamp_tree_cursor();
        }
    }

    /// Clamp the tree cursor into the current visible rows and re-anchor the
    /// scroll so the cursor stays on-screen. Call after anything that can shrink
    /// the visible-row count (collapsing directories) — otherwise a stale
    /// `tree_scroll` left past the new end renders the file pane blank.
    fn clamp_tree_cursor(&mut self) {
        let len = self.visible_rows().len();
        self.tree_cursor = self.tree_cursor.min(len.saturating_sub(1));
        self.follow_tree_cursor(len);
    }

    /// Keep the tree cursor within the file pane's viewport, which shares its
    /// height with the diff body beside it.
    fn follow_tree_cursor(&mut self, _len: usize) {
        let row = self.tree_cursor as u16;
        let height = self.body_height().max(1) as u16;
        if row < self.tree_scroll {
            self.tree_scroll = row;
        } else if row >= self.tree_scroll + height {
            self.tree_scroll = row.saturating_sub(height - 1);
        }
    }

    /// Sync the tree cursor onto the visible row for `selected_file`, if shown.
    fn sync_tree_cursor_to_file(&mut self) {
        let rows = self.visible_rows();
        if let Some(pos) = rows
            .iter()
            .position(|r| matches!(r, TreeRow::File { index, .. } if *index == self.selected_file))
        {
            self.tree_cursor = pos;
            self.follow_tree_cursor(rows.len());
        }
    }

    fn toggle_layout(&mut self) {
        self.layout = match self.layout {
            ReviewLayout::Inline => ReviewLayout::SideBySide,
            ReviewLayout::SideBySide => ReviewLayout::Inline,
        };
    }

    fn current_file(&self) -> Option<&FileDiff> {
        self.diff.files.get(self.selected_file)
    }

    /// Whether the current file could reveal more context (used to decide
    /// whether a background file-content fetch is worthwhile). True for a
    /// non-binary file with at least one hunk that exists on both sides.
    fn current_file_expandable(&self) -> bool {
        self.with_view(|v| v.file.is_expandable()).unwrap_or(false)
    }

    /// Apply an expand action to gap `gap` of the current file. The caller kicks
    /// the file-content fetch so the revealed lines have text to render.
    fn expand_gap(&mut self, gap: usize, action: ExpandAction) {
        self.with_view_mut(|view, source| view.expand(source, GapIdx::new(gap), action));
    }

    /// The hunk index containing selectable line `cursor`, or `None` when the
    /// file has no selectable lines. Used to pick the gap a keyboard expand
    /// acts on (the gap above/below the cursor's hunk).
    fn hunk_of_cursor(&self) -> Option<usize> {
        let file = self.current_file()?;
        let mut acc = 0;
        for (i, hunk) in file.hunks.iter().enumerate() {
            acc += hunk.lines.len();
            if self.cursor < acc {
                return Some(i);
            }
        }
        file.hunks.len().checked_sub(1)
    }

    /// Install loaded working-tree lines for `path` (test/handler entry point).
    pub(super) fn set_file_lines(&mut self, path: String, lines: std::sync::Arc<Vec<String>>) {
        self.file_lines.0.insert(path, lines);
    }

    /// Flip the preferred binary-image side (before ⇄ after). No-op visually on
    /// added/deleted files, which always show their only side.
    pub(super) fn toggle_image_side(&mut self) {
        self.image_side = match self.image_side {
            DiffSide::Old => DiffSide::New,
            DiffSide::New => DiffSide::Old,
        };
    }

    /// Whether flipping the image side actually does something: the current
    /// file is a *modified* binary image (added/deleted images show only their
    /// one side). Gates both the footer hint and the `o` telemetry, so no-op
    /// presses on text/added/deleted files aren't counted.
    pub(super) fn can_toggle_image_side(&self) -> bool {
        self.current_file().is_some_and(|f| {
            f.status == FileStatus::Modified
                && matches!(
                    f.binary.as_ref().map(|b| &b.kind),
                    Some(claude_commander_core::git::BinaryKind::Image { .. })
                )
        })
    }

    /// The current file's diff lines in render order (selection operates over
    /// these; hunk headers are not selectable).
    fn selectable_lines(&self) -> Vec<&DiffLine> {
        self.current_file()
            .map(|f| f.hunks.iter().flat_map(|h| h.lines.iter()).collect())
            .unwrap_or_default()
    }

    fn selectable_count(&self) -> usize {
        self.current_file()
            .map(|f| f.hunks.iter().map(|h| h.lines.len()).sum())
            .unwrap_or(0)
    }

    /// Number of not-yet-applied comments anchored to `file`.
    fn comment_count(&self, file: &str) -> usize {
        self.comments
            .iter()
            .filter(|a| a.status != CommentStatus::Applied && a.file == file)
            .count()
    }

    /// Pending comments across every file under directory `dir_path`, so a
    /// collapsed directory still surfaces that its subtree has comments.
    fn dir_comment_count(&self, dir_path: &str) -> usize {
        let prefix = format!("{dir_path}/");
        self.comments
            .iter()
            .filter(|a| a.status != CommentStatus::Applied && a.file.starts_with(&prefix))
            .count()
    }

    /// Inclusive selection range over selectable-line indices: the visual
    /// anchor..cursor when selecting, else the single cursor line.
    fn selection(&self) -> (usize, usize) {
        match self.visual_anchor {
            Some(anchor) => (anchor.min(self.cursor), anchor.max(self.cursor)),
            None => (self.cursor, self.cursor),
        }
    }

    /// Whether `path` (a display path) is marked reviewed.
    fn is_reviewed_path(&self, path: &str) -> bool {
        self.reviewed.contains(path)
    }

    /// Set or clear the reviewed mark for `path` in the view's local mirror
    /// (the persisted store is updated by the service).
    fn set_reviewed(&mut self, path: String, on: bool) {
        self.reviewed_gen = next_reviewed_gen();
        if on {
            self.reviewed.insert(path);
        } else {
            self.reviewed.remove(&path);
        }
    }

    /// Fold in the store's marks as polled at `generation`, so marks set
    /// elsewhere (the `review` CLI, another frontend) appear without a reopen
    /// (see [`review::fold_marks`] for the per-file rule).
    ///
    /// A poll from before this view last changed its own marks is ignored.
    pub(super) fn fold_reviewed_marks(&mut self, generation: u64, marks: &[ReviewedFileMark]) {
        if generation != self.reviewed_gen {
            return;
        }
        let newly_reviewed =
            review::fold_marks(&mut self.reviewed, &self.diff, &self.hashes, marks);
        // A mark arriving from elsewhere finishes a directory just as `m` does.
        for path in &newly_reviewed {
            self.collapse_completed_dirs(Some(path));
        }
        // Unlike `m`, nothing re-selects a file afterwards, so put the tree
        // cursor back on the shown file if a collapse moved the rows under it.
        if !newly_reviewed.is_empty() {
            self.sync_tree_cursor_to_file();
        }
    }

    /// Fold away directories whose entire file subtree is reviewed. With a
    /// `reviewed_path` (the file just marked), only its ancestors are
    /// considered, so unrelated directories elsewhere in the tree are never
    /// touched. With `None` (on open), every finished directory folds, so
    /// marks set while the view was closed — or by the `review` CLI — leave
    /// the tree as `m` would have. Only ever collapses.
    pub(super) fn collapse_completed_dirs(&mut self, reviewed_path: Option<&str>) {
        let mut completed = Vec::new();
        collect_completed_dirs(
            &self.file_tree,
            reviewed_path,
            &self.reviewed,
            &mut completed,
        );
        self.collapsed.extend(completed);
        // Collapsing shrinks the visible rows; keep the cursor (and scroll) in range.
        self.clamp_tree_cursor();
    }

    /// Select the first file not yet marked reviewed (in tree/display order),
    /// falling back to the first file when every file is reviewed. Called once
    /// after the reviewed set is populated so opening the view lands on the
    /// first file still needing attention rather than always the first file.
    pub(super) fn select_first_unreviewed(&mut self) {
        if let Some(idx) = first_unreviewed_file_index(&self.file_tree, &self.diff, &self.reviewed)
            .or_else(|| first_file_index(&self.file_tree))
        {
            self.set_body_file(idx);
            self.sync_tree_cursor_to_file();
        }
    }

    /// Jump the body to the next unreviewed file after the current one (in
    /// diff order), wrapping around; stays put when every file is reviewed.
    fn advance_to_next_unreviewed(&mut self) {
        let count = self.diff.files.len();
        let next = (1..count)
            .map(|step| (self.selected_file + step) % count)
            .find(|&idx| !self.is_reviewed_path(self.diff.files[idx].display_path()));
        if let Some(idx) = next {
            self.set_body_file(idx);
            self.sync_tree_cursor_to_file();
        }
    }

    /// Jump the body to the next file (in diff order), syncing the tree cursor.
    pub fn next_file(&mut self) {
        if self.diff.files.is_empty() {
            return;
        }
        self.set_body_file((self.selected_file + 1).min(self.diff.files.len() - 1));
        self.sync_tree_cursor_to_file();
    }

    /// Jump the body to the previous file (in diff order).
    pub fn prev_file(&mut self) {
        self.set_body_file(self.selected_file.saturating_sub(1));
        self.sync_tree_cursor_to_file();
    }

    /// Move the body cursor by one line, clamped, keeping it visible.
    fn move_cursor(&mut self, down: bool) {
        let count = self.selectable_count();
        if count == 0 {
            return;
        }
        if down {
            self.cursor = (self.cursor + 1).min(count - 1);
        } else {
            self.cursor = self.cursor.saturating_sub(1);
        }
        self.follow_cursor();
    }

    /// Adjust `scroll` so the cursor's row stays within the viewport.
    ///
    /// The viewport height is the pane's real height, reported by the last
    /// render. It used to be a `BODY_VIEWPORT = 20` constant, which put the
    /// cursor off-screen on every terminal that was not about 22 rows tall.
    fn follow_cursor(&mut self) {
        let cursor = SelIdx::new(self.cursor);
        let (current, height) = (self.scroll as usize, self.body_height());
        let scrolled = self
            .with_grid(|_, grid| {
                grid.row_of(cursor)
                    .map(|y| grid.scroll_to(y, current, height))
            })
            .flatten();
        if let Some(scroll) = scrolled {
            self.scroll = scroll as u16;
        }
    }

    /// The selectable-line index the in-progress comment edit box anchors to
    /// (the last line of its range), or `None` when no comment is being edited.
    /// The edit box renders just after this line, where the saved comment will.
    fn draft_anchor(&self) -> Option<usize> {
        self.comment.as_ref().map(|d| d.range.1)
    }

    /// Adjust `scroll` so the in-progress comment edit box stays visible (inline
    /// only). Keeps the box's last row in view as it grows with typed text, and
    /// pulls the top into view if the box starts above the viewport.
    fn follow_draft(&mut self) {
        if self.layout != ReviewLayout::Inline || self.comment.is_none() {
            return;
        }
        // The draft is an attached block, so the grid already knows where it
        // starts and how tall it currently is at this width.
        let span = self
            .with_grid(|view, grid| {
                let row = view
                    .layout
                    .rows()
                    .find(|r| r.attach_key() == Some(DRAFT_ATTACH_KEY))?;
                let first = grid.row_of_logical(row.index())?.get();
                Some((
                    first,
                    first + grid.height_of_logical(row.index()).max(1) - 1,
                ))
            })
            .flatten();
        let Some((first, last)) = span else {
            return;
        };
        let height = self.body_height();
        let bottom = self.scroll as usize + height;
        if last >= bottom {
            self.scroll = (last + 1).saturating_sub(height) as u16;
        }
        if (first as u16) < self.scroll {
            self.scroll = first as u16;
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            ReviewFocus::FileList => ReviewFocus::Body,
            ReviewFocus::Body => ReviewFocus::FileList,
        };
    }

    /// Open the comment box for the current selection (Enter / right-click).
    /// No-op (returns false) when the file has no diff lines or a comment is
    /// already open.
    pub fn begin_comment(&mut self) -> bool {
        if self.selectable_count() == 0 || self.comment.is_some() {
            return false;
        }
        self.focus = ReviewFocus::Body;
        self.comment = Some(CommentDraft {
            input: Input::default(),
            range: self.selection(),
        });
        self.follow_draft();
        true
    }

    /// Append pasted clipboard text to the open comment draft. Newlines are
    /// kept — bracketed paste delivers them as text rather than Enter key
    /// events, so a multi-line paste can't accidentally submit, and
    /// `compose_markdown` passes them to the agent verbatim. Carriage
    /// returns from CRLF clipboards are dropped. Returns `false` (no-op)
    /// when no comment is being edited.
    pub fn paste_into_draft(&mut self, text: &str) -> bool {
        let Some(draft) = self.comment.as_mut() else {
            return false;
        };
        // `tui-input` has no bulk insert, so feed chars one at a time at the
        // cursor. CRs from CRLF clipboards are dropped; newlines are kept.
        for c in text.chars().filter(|&c| c != '\r') {
            draft.input.handle(tui_input::InputRequest::InsertChar(c));
        }
        self.follow_draft();
        true
    }

    /// Total physical body rows for the current file: every logical row the
    /// layout produced, counting soft-wrap continuations and each row of an
    /// attached comment box.
    fn total_body_rows(&self) -> usize {
        self.with_grid(|_, grid| grid.row_count()).unwrap_or(0)
    }

    /// The largest useful `scroll`: the one that puts the file's last row on the
    /// pane's bottom row. Scrolling further only opens a void below the diff, so
    /// every free-scroll path (paging, the wheel) clamps here rather than at
    /// `rows - 1`, which let the end of the file climb to the top of an
    /// otherwise empty viewport.
    ///
    /// Falls back to `rows - 1` before the first render, when the pane's height
    /// is still unknown (zero) and there is no viewport to pin the last row to.
    fn max_body_scroll(&self) -> u16 {
        let rows = self.total_body_rows();
        rows.saturating_sub(self.body_height().max(1)) as u16
    }

    /// [`Self::max_body_scroll`] for the file pane, which shares the body's
    /// height.
    fn max_tree_scroll(&self) -> u16 {
        let rows = self.visible_rows().len();
        rows.saturating_sub(self.body_height().max(1)) as u16
    }

    /// Scroll the diff body by a page (lazygit-style PgUp/PgDn). Independent of
    /// focus, so paging the diff works while the file list is focused.
    fn page_body(&mut self, down: bool) {
        let max = self.max_body_scroll();
        let page = self.body_height() as u16;
        self.scroll = if down {
            (self.scroll + page).min(max)
        } else {
            self.scroll.saturating_sub(page)
        };
    }

    /// Selectable-line index at body row `body_row` (`None` for a header,
    /// comment-box, revealed-context or out-of-range row). A click on a
    /// soft-wrap continuation row maps to its diff line.
    ///
    /// Column-independent, so in side-by-side it reports whichever half the row
    /// carries; [`Self::hit_at`] is the column-aware form the mouse uses.
    fn selectable_at_body_row(&self, body_row: usize) -> Option<usize> {
        self.with_grid(|view, grid| {
            let row = grid.row(RowIdx::new(body_row))?;
            let logical = view.layout.row(row.logical())?;
            (logical.kind() == RowKind::Line)
                .then(|| logical.sel())
                .flatten()
                .map(SelIdx::get)
        })
        .flatten()
    }

    /// Scroll the body by one row (free of the cursor), for mouse wheel.
    pub fn wheel(&mut self, down: bool) {
        let max = self.max_body_scroll();
        self.scroll = if down {
            (self.scroll + 1).min(max)
        } else {
            self.scroll.saturating_sub(1)
        };
    }

    /// Scroll the file-list pane by one row (free of the tree cursor), for a
    /// mouse wheel over the file list.
    pub fn wheel_tree(&mut self, down: bool) {
        let max = self.max_tree_scroll();
        self.tree_scroll = if down {
            (self.tree_scroll + 1).min(max)
        } else {
            self.tree_scroll.saturating_sub(1)
        };
    }

    /// What sits under a screen position inside the body pane.
    ///
    /// The single mouse→content mapping: `diffgrid` resolves the column as well
    /// as the row, so an expand control's clickable zones, a comment box's
    /// interior and a diff line's gutter all come back distinguished, from the
    /// same layout the renderer drew.
    fn hit_at(&self, col: u16, row: u16, body: Rect) -> Option<Hit> {
        let body_row = self.body_row_at(col, row, body)?;
        let x = col.saturating_sub(body.x) as usize;
        self.with_grid(|_, grid| grid.hit(x, RowIdx::new(body_row)))
    }

    /// Left-click at a screen position. Clicking an expand control reveals more
    /// context; clicking a comment box folds or unfolds it; clicking a diff line
    /// focuses the body and moves the cursor there (clearing any selection).
    /// Returns `true` when the click triggered an expansion, so the caller can
    /// kick the file-content fetch that materialises the revealed lines.
    pub fn click_at(&mut self, col: u16, row: u16, body: Rect) -> bool {
        let Some(hit) = self.hit_at(col, row, body) else {
            return false;
        };
        match hit {
            Hit::ExpandControl { gap, action } => {
                if let Some(action) = action {
                    self.expand_gap(gap.get(), action);
                }
                true
            }
            // The draft box is not a fold target — it is being typed into.
            Hit::Attached { key, .. } if key != DRAFT_ATTACH_KEY => {
                if let Some(id) = self.comment_by_key(key).map(|a| a.id) {
                    self.toggle_comment_collapsed(id);
                }
                false
            }
            _ => {
                let body_row = self.body_row_at(col, row, body).unwrap_or_default();
                self.place_cursor_at_row(body_row);
                false
            }
        }
    }

    /// Focus the body and move the cursor to the diff line at `body_row`,
    /// clearing any active selection. Inline only: in side-by-side a row holds
    /// two lines and which one was meant depends on the column, so this just
    /// focuses the body there.
    fn place_cursor_at_row(&mut self, body_row: usize) {
        self.focus = ReviewFocus::Body;
        self.visual_anchor = None;
        if self.layout == ReviewLayout::Inline
            && let Some(idx) = self.selectable_at_body_row(body_row)
        {
            self.cursor = idx;
        }
    }

    /// Map a screen position in the file-list pane to a visible tree-row index
    /// (accounting for the tree's scroll offset).
    fn file_row_at(&self, col: u16, row: u16, rect: Rect) -> Option<usize> {
        let inside = col >= rect.x
            && col < rect.x + rect.width
            && row >= rect.y
            && row < rect.y + rect.height;
        inside.then(|| (row - rect.y) as usize + self.tree_scroll as usize)
    }

    /// Left-click in the file-list pane: focus it and move the tree cursor to
    /// the clicked row. A file row is shown in the body; a directory row is
    /// expanded/collapsed (the mouse equivalent of Enter).
    pub fn click_file_list_at(&mut self, col: u16, row: u16, rect: Rect) {
        let Some(idx) = self.file_row_at(col, row, rect) else {
            return;
        };
        let rows = self.visible_rows();
        let file_index = match rows.get(idx) {
            Some(TreeRow::File { index, .. }) => Some(*index),
            Some(TreeRow::Dir { .. }) => None,
            None => return,
        };
        self.focus = ReviewFocus::FileList;
        self.tree_cursor = idx;
        match file_index {
            Some(index) => self.set_body_file(index),
            None => self.tree_activate(),
        }
    }

    /// Right-click at a screen position: open the comment box. With no active
    /// selection, first move the cursor to the clicked line so a bare
    /// right-click comments on the line under the pointer; an in-progress
    /// drag-selection is preserved and commented on as-is.
    pub fn right_click_comment(&mut self, col: u16, row: u16, body: Rect) -> bool {
        // Position the cursor on the clicked line (not via `click_at`, which
        // would toggle a comment box rather than comment on it).
        if self.visual_anchor.is_none()
            && let Some(body_row) = self.body_row_at(col, row, body)
        {
            self.place_cursor_at_row(body_row);
        }
        self.begin_comment()
    }

    /// Double-click at a screen position: select the single diff line under the
    /// pointer and open its comment box (the mouse equivalent of right-click on
    /// a fresh line). Returns `false` — leaving the caller to fall back to a
    /// plain click — when the row is a header or comment box rather than a
    /// selectable diff line.
    pub fn double_click_comment(&mut self, col: u16, row: u16, body: Rect) -> bool {
        let Some(body_row) = self.body_row_at(col, row, body) else {
            return false;
        };
        if self.selectable_at_body_row(body_row).is_none() {
            return false;
        }
        self.place_cursor_at_row(body_row);
        self.begin_comment()
    }

    /// Left-drag to a screen position: begin a selection at the press point (if
    /// not already selecting) and extend it to the dragged line.
    pub fn drag_at(&mut self, col: u16, row: u16, body: Rect) {
        if self.layout != ReviewLayout::Inline {
            return;
        }
        let Some(body_row) = self.body_row_at(col, row, body) else {
            return;
        };
        if let Some(idx) = self.selectable_at_body_row(body_row) {
            if self.visual_anchor.is_none() {
                self.visual_anchor = Some(self.cursor);
            }
            self.cursor = idx;
        }
    }

    /// Map a screen position inside `body` to a body-row index (accounting for
    /// scroll), or `None` if outside.
    fn body_row_at(&self, col: u16, row: u16, body: Rect) -> Option<usize> {
        let inside = col >= body.x
            && col < body.x + body.width
            && row >= body.y
            && row < body.y + body.height;
        if !inside {
            return None;
        }
        Some((row - body.y) as usize + self.scroll as usize)
    }

    /// Enter visual mode at the cursor, or cancel it if already active.
    fn toggle_visual(&mut self) {
        self.visual_anchor = match self.visual_anchor {
            Some(_) => None,
            None => Some(self.cursor),
        };
    }

    /// Selectable lines for `range`, clamped to the available lines.
    fn selected_lines(&self, range: (usize, usize)) -> Vec<&DiffLine> {
        let lines = self.selectable_lines();
        let (lo, hi) = range;
        lines
            .get(lo..=hi.min(lines.len().saturating_sub(1)))
            .map(<[_]>::to_vec)
            .unwrap_or_default()
    }

    /// Pick the comment side for a selected line slice (New unless the
    /// selection is purely deletions) and collect each contributing line's
    /// gutter line number + content on that side. Shared by `build_draft`
    /// (snippet/anchor) and the comment-box title so they can't drift apart.
    fn side_and_lines(sel: &[&DiffLine]) -> (CommentSide, Vec<(usize, String)>) {
        let any_new = sel.iter().any(|l| l.new_lineno.is_some());
        let side = if any_new {
            CommentSide::New
        } else {
            CommentSide::Old
        };
        let collected = sel
            .iter()
            .filter_map(|l| {
                let n = match side {
                    CommentSide::New => l.new_lineno,
                    CommentSide::Old => l.old_lineno,
                }?;
                Some((n, l.content.clone()))
            })
            .collect();
        (side, collected)
    }

    /// Resolve a selectable-line range to the gutter line numbers it covers on
    /// its comment side — what the user sees, not the raw selectable index.
    /// Returns `None` if the range contributes no numbered lines.
    fn resolved_line_range(&self, range: (usize, usize)) -> Option<(usize, usize)> {
        let sel = self.selected_lines(range);
        let (_, collected) = Self::side_and_lines(&sel);
        let nums = collected.iter().map(|(n, _)| *n);
        Some((nums.clone().min()?, nums.max()?))
    }

    /// Build an comment draft from a selectable-line range plus comment
    /// text. Picks the New side unless the selection is purely deletions, and
    /// captures the snippet/line range from that side's lines only (so it
    /// re-anchors cleanly).
    fn build_draft(&self, range: (usize, usize), comment: String) -> Option<NewComment> {
        let file = self.current_file()?;
        let sel = self.selected_lines(range);
        if sel.is_empty() {
            return None;
        }
        let (side, collected) = Self::side_and_lines(&sel);
        if collected.is_empty() {
            return None;
        }

        let nums = collected.iter().map(|(n, _)| *n);
        Some(NewComment {
            file: file.display_path().to_string(),
            side,
            line_range: (nums.clone().min().unwrap(), nums.max().unwrap()),
            snippet: collected
                .iter()
                .map(|(_, c)| c.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            comment,
        })
    }

    /// Selectable-line index a non-applied comment anchors to in the current
    /// file: the line whose gutter number equals the end of the comment's
    /// range, or — when that line is no longer present in the diff (a drifted
    /// comment whose range fell outside the current hunks) — the file's last
    /// selectable line. Pinning orphans to the last line keeps them visible
    /// and deletable instead of silently dropping them. `None` only when the
    /// file has no selectable lines at all.
    fn comment_anchor_index(&self, ann: &Comment, lines: &[&DiffLine]) -> Option<usize> {
        if lines.is_empty() {
            return None;
        }
        let end = ann.line_range.1;
        let matched = lines.iter().position(|line| {
            let lineno = match ann.side {
                CommentSide::New => line.new_lineno,
                CommentSide::Old => line.old_lineno,
            };
            lineno == Some(end)
        });
        Some(matched.unwrap_or(lines.len() - 1))
    }

    /// Whether non-applied comment `ann` is reachable at selectable index
    /// `idx`: either `idx`'s gutter line falls within the comment's range, or
    /// `idx` is the comment's drift-fallback anchor line. The fallback only
    /// fires for the file's last selectable line, so a drifted comment whose
    /// range left the diff stays selectable (and thus deletable).
    fn comment_touches_index(&self, ann: &Comment, idx: usize, lines: &[&DiffLine]) -> bool {
        if let Some(line) = lines.get(idx) {
            let lineno = match ann.side {
                CommentSide::New => line.new_lineno,
                CommentSide::Old => line.old_lineno,
            };
            if lineno.is_some_and(|n| ann.line_range.0 <= n && n <= ann.line_range.1) {
                return true;
            }
        }
        idx + 1 == lines.len() && self.comment_anchor_index(ann, lines) == Some(idx)
    }

    /// Id of a not-yet-applied comment covering the cursor line, if any.
    fn comment_at_cursor(&self) -> Option<uuid::Uuid> {
        let file = self.current_file()?;
        let display = file.display_path();
        let lines = self.selectable_lines();
        self.comments
            .iter()
            .filter(|a| a.status != CommentStatus::Applied && a.file == display)
            .find(|a| self.comment_touches_index(a, self.cursor, &lines))
            .map(|a| a.id)
    }

    /// Status message for an Apply blocked by drifted comments.
    ///
    /// Names the files (deduped, at most two) rather than only counting, so a
    /// blocker is findable — including the residual case the orphan drop can't
    /// resolve, where the diff's absences aren't authoritative and a drifted
    /// comment sits on a file the tree isn't showing.
    fn drift_block_message(&self, drifted: &[uuid::Uuid]) -> String {
        let mut files: Vec<&str> = Vec::new();
        for id in drifted {
            if let Some(a) = self.comments.iter().find(|a| a.id == *id) {
                let name = a.file.rsplit('/').next().unwrap_or(&a.file);
                if !files.contains(&name) {
                    files.push(name);
                }
            }
        }
        // Match on the deduped *file* count, never the comment count: three
        // drifted comments in two files is still "in a, b" — an ellipsis there
        // would point at files that don't exist.
        let where_ = match files.as_slice() {
            [] => String::new(),
            [one] => format!(" in {one}"),
            [a, b] => format!(" in {a}, {b}"),
            [a, b, ..] => format!(" in {a}, {b}, …"),
        };
        format!(
            "{} drifted comment(s){where_} block apply — review or delete them",
            drifted.len()
        )
    }

    /// Selectable-line indices covered by a non-applied comment, mapped to
    /// whether any such comment has drifted.
    ///
    /// Built once per frame rather than probed per row: the gutter asks for
    /// every line, and the underlying test is O(comments) with an O(lines)
    /// setup, so per-row lookups made rendering quadratic in the file.
    fn comment_markers(&self) -> HashMap<usize, bool> {
        let mut out = HashMap::new();
        let Some(file) = self.current_file() else {
            return out;
        };
        let display = file.display_path();
        let lines = self.selectable_lines();
        for a in self
            .comments
            .iter()
            .filter(|a| a.status != CommentStatus::Applied && a.file == display)
        {
            let drifted = a.status == CommentStatus::Drifted;
            for idx in 0..lines.len() {
                if self.comment_touches_index(a, idx, &lines) {
                    *out.entry(idx).or_insert(false) |= drifted;
                }
            }
        }
        out
    }

    /// Whether comment `id`'s inline box is collapsed.
    fn is_comment_collapsed(&self, id: uuid::Uuid) -> bool {
        self.collapsed_comments.contains(&id)
    }

    /// Fold an expanded comment box, or unfold a collapsed one.
    fn toggle_comment_collapsed(&mut self, id: uuid::Uuid) {
        if !self.collapsed_comments.remove(&id) {
            self.collapsed_comments.insert(id);
        }
    }

    /// Toggle the inline box (expanded/collapsed) of the comment covering
    /// the cursor line, if any.
    fn toggle_comment_fold(&mut self) {
        if let Some(id) = self.comment_at_cursor()
            && !self.collapsed_comments.remove(&id)
        {
            self.collapsed_comments.insert(id);
        }
    }
}

impl App {
    /// The review view currently on screen: the one owning the modal, or the
    /// one suspended underneath the session switcher (see
    /// [`Modal::QuickSwitch`]'s `review`).
    ///
    /// Background folds go through this rather than matching `Modal::ReviewDiff`
    /// directly, because the palette can be open over a review when they land
    /// and dropping one is not always recoverable: the automatic refresh is
    /// *edge*-triggered on the agent going Working→Idle, so a refresh discarded
    /// because the switcher happened to be open never fires again and the diff
    /// stays stale until the user presses `r`.
    pub(super) fn open_review(&self) -> Option<&DiffReviewState> {
        match &self.ui_state.modal {
            Modal::ReviewDiff(state) => Some(state),
            Modal::QuickSwitch {
                review: Some(state),
                ..
            } => Some(state),
            _ => None,
        }
    }

    /// [`Self::open_review`], mutably — for the folds that update it in place.
    pub(super) fn open_review_mut(&mut self) -> Option<&mut DiffReviewState> {
        match &mut self.ui_state.modal {
            Modal::ReviewDiff(state) => Some(state),
            Modal::QuickSwitch {
                review: Some(state),
                ..
            } => Some(state),
            _ => None,
        }
    }

    /// Open the review view for the selected session.
    pub(super) async fn handle_open_review(&mut self) {
        let Some(sref) = self.ui_state.selected_session_id else {
            self.set_review_status("Select a session first");
            return;
        };
        let session_id = sref.id;

        let title = self
            .session(sref)
            .map(|s| s.title.clone())
            .unwrap_or_default();

        // Put the loading spinner up first, then fetch the review OFF the event
        // loop. `open_review` composes the base→working-tree diff — for a remote
        // session that's an HTTP GET with a 30s ceiling on a hung server, which
        // must never block the render loop. The precompute (when enabled) runs
        // in the same task, so there is a single modal covering fetch +
        // highlighting rather than one modal for each. On completion the task
        // posts `ReviewPrepared` (ready view), or `ReviewOpenFailed` for a
        // no-changes / errored fetch.
        self.ui_state.modal = Modal::Loading {
            title: "Preparing review".to_string(),
            message: "Loading changes…".to_string(),
            hint: Some(
                "Disable \"Precompute Review Caches\" in settings to skip highlighting".to_string(),
            ),
        };

        let precompute = self.config.precompute_review_caches;
        let highlight = self.theme.mode == ColorMode::TrueColor;
        let backend = self.backend_for(sref);
        let tx = self.event_loop.sender();
        tokio::spawn(async move {
            let snapshot = match backend.open_review(session_id).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx
                        .send(AppEvent::StateUpdate(StateUpdate::ReviewOpenFailed {
                            error: Some(e.to_string()),
                        }))
                        .await;
                    return;
                }
            };
            if snapshot.diff.is_empty() {
                let _ = tx
                    .send(AppEvent::StateUpdate(StateUpdate::ReviewOpenFailed {
                        error: None,
                    }))
                    .await;
                return;
            }
            let base = snapshot.base;
            let comments = snapshot.comments;
            let reviewed = snapshot.reviewed;
            let content_hash = snapshot.content_hash;
            let dropped_comments = snapshot.dropped_comments;
            // Default: precompute every file's render caches (the word diff plus
            // syntax highlighting) up front so file switching is instant. The
            // precompute is CPU-bound and synchronous, so keep it off the async
            // worker pool; hand the diff back out with its models rather than
            // cloning it. Opt-out (`precompute` off): send no models so each
            // file's caches build lazily on first navigation.
            let (diff, models) = if precompute {
                tokio::task::spawn_blocking(move || {
                    let models = precompute_review_caches(&snapshot.diff, highlight);
                    (snapshot.diff, models)
                })
                .await
                .expect("review precompute task panicked")
            } else {
                (snapshot.diff, Vec::new())
            };
            let _ = tx
                .send(AppEvent::StateUpdate(StateUpdate::ReviewPrepared {
                    prepared: Box::new(ReviewPrepared {
                        session_id,
                        title,
                        base,
                        diff,
                        comments,
                        reviewed,
                        models,
                        content_hash,
                        dropped_comments,
                    }),
                }))
                .await;
        });
    }

    pub(super) fn set_review_status(&mut self, msg: &str) {
        self.ui_state.status_message =
            Some((msg.to_string(), Instant::now() + Duration::from_secs(3)));
    }

    /// Reload the session's comments and re-anchor them against the current
    /// diff (used after create/delete/apply, which don't change the diff).
    pub(super) async fn reload_review_comments(&mut self, state: &mut DiffReviewState) {
        let backend = self.backend_arc(self.backend_of_session(state.session_id));
        let mut anns = backend
            .list_comments(state.session_id)
            .await
            .unwrap_or_default();
        // No orphan prune here on purpose. The store this just loaded was already
        // pruned by the open/refresh that composed `state.diff`, so a prune would
        // be a no-op — except on a diff whose absences aren't authoritative,
        // where the service deliberately skipped it and this would delete
        // still-live comments from the view while the store (and Apply gating)
        // kept them. `state.diff` alone can't tell the two cases apart.
        claude_commander_core::comment::reanchor_comments(&mut anns, &state.diff);
        // Keep the session-list pending-comment marker in sync without a disk
        // scan: we already have this session's full comment set in hand.
        let pending = anns.iter().any(|a| a.status != CommentStatus::Applied);
        if pending {
            self.ui_state
                .sessions_with_comments
                .insert(state.session_id);
        } else {
            self.ui_state
                .sessions_with_comments
                .remove(&state.session_id);
        }
        state.comments = anns;
    }

    /// Refresh the set of sessions with pending comments (drives the
    /// session-list `*` marker). Run at startup to surface comments left over
    /// from a previous run. Unions the pending-comment ids each backend already
    /// carries in its cached snapshot, so a remote session's marker shows
    /// without a network call — no per-backend query, no local-only bias.
    pub(super) fn refresh_comment_indicators(&mut self) {
        self.ui_state.sessions_with_comments = self
            .backends
            .iter()
            .flat_map(|h| h.view.snapshot.pending_comment_sessions.iter().copied())
            .collect();
    }

    /// Handle a key while the review view is open. `state` has been moved out
    /// of `self.ui_state.modal`; it is put back unless the view is closed.
    pub(super) async fn handle_review_key(
        &mut self,
        key: KeyEvent,
        mut state: Box<DiffReviewState>,
    ) {
        use crossterm::event::{KeyCode, KeyModifiers};

        // Comment box captures all input while open.
        if state.comment.is_some() {
            self.handle_review_comment_key(key, &mut state).await;
            self.ui_state.modal = Modal::ReviewDiff(state);
            return;
        }

        // Ctrl+Q closes the view (consistency with the tmux-session shortcut),
        // alongside Esc. The modal was already replaced with None on extraction.
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }

        // Re-attaching to the session mirrors the (rebindable) OpenReviewDiff
        // key that switches an attached session to its review diff (`Alt-r` by
        // default), so the pair toggles back and forth. The modal is already
        // None; `handle_select` queues the attach and quits the TUI loop,
        // which `run()` then picks up.
        if claude_commander_core::config::keybindings::matches_review_toggle(
            &self.config.keybindings,
            &key,
        ) {
            let backend = self.backend_of_session(state.session_id);
            self.ui_state.selected_session_id = Some(SessionRef::new(backend, state.session_id));
            self.handle_select().await;
            return;
        }

        // Open the review's session worktree in the editor, honouring the
        // configurable OpenInEditor binding (`.` by default) so the shortcut
        // works here as it does in the session list. Skipped in visual mode so
        // it doesn't tear the TUI down mid-selection (matching the footer, which
        // only offers "edit" outside comment/visual sub-modes). Failures surface
        // as an inline status so the review view stays open rather than being
        // replaced by an error modal.
        if state.visual_anchor.is_none()
            && self
                .config
                .keybindings
                .keys_for(BindableAction::OpenInEditor)
                .iter()
                .any(|kb| kb.matches(&key))
        {
            self.record_feature("editor.open");
            let backend = self.backend_of_session(state.session_id);
            if let Some(path) = self
                .session(SessionRef::new(backend, state.session_id))
                .map(|s| std::path::PathBuf::from(&s.worktree_path))
            {
                match self.open_path_in_editor(backend, path) {
                    super::actions::EditorLaunch::Launched => {}
                    super::actions::EditorLaunch::Unavailable(msg)
                    | super::actions::EditorLaunch::Failed(msg) => self.set_review_status(&msg),
                }
            }
            self.ui_state.modal = Modal::ReviewDiff(state);
            return;
        }

        // Ctrl-n / Ctrl-p mirror the arrow keys (and j/k) for navigation,
        // matching the convention used by the other list modals.
        let nav_code = review_nav_keycode(key);
        // Ctrl+Space (or the leader key) opens the session switcher over the
        // diff. Checked in the fallthrough arm below rather than here, so it
        // only applies to keys this view has no meaning for in the current
        // focus/sub-mode: with a leader rebound onto, say, `r`, `r` still
        // refreshes the diff. Guarded arms make that per-mode by construction —
        // a leader of `v` toggles visual select in the body and opens the
        // switcher from the file list, where `v` is unbound.
        let (leader_code, leader_mods) = self.config.parse_leader_key();
        let opens_switcher = super::input::opens_palette(&key, leader_code, leader_mods);
        // Record UI-only review features (layout/fold/image/visual/refresh) that
        // don't flow through an instrumented service method. Comment create /
        // delete / apply and reviewed-toggle are recorded at the service layer.
        if let Some(feature) = review_key_feature(nav_code, state.focus) {
            self.record_feature(feature);
        }
        match nav_code {
            // Esc cancels an in-progress selection first; otherwise closes.
            KeyCode::Esc if state.visual_anchor.is_none() => return,
            KeyCode::Esc => state.visual_anchor = None,
            // Binary focus toggle: Shift+Tab is its own reverse, same as Tab.
            KeyCode::Tab | KeyCode::BackTab => state.toggle_focus(),
            KeyCode::Char(']') => state.next_file(),
            KeyCode::Char('[') => state.prev_file(),
            KeyCode::Down | KeyCode::Char('j') => match state.focus {
                ReviewFocus::FileList => state.tree_move(true),
                ReviewFocus::Body => state.move_cursor(true),
            },
            KeyCode::Up | KeyCode::Char('k') => match state.focus {
                ReviewFocus::FileList => state.tree_move(false),
                ReviewFocus::Body => state.move_cursor(false),
            },
            // Page the diff regardless of focus (lazygit: scroll the diff while
            // the file list is focused).
            KeyCode::PageDown => state.page_body(true),
            KeyCode::PageUp => state.page_body(false),
            KeyCode::Char('t') => state.toggle_layout(),
            // Flip the before/after side of a binary image (no-op for non-image
            // files and for added/deleted files, which have only one side).
            // Record only when it does something, so no-op presses on text
            // files don't inflate the metric.
            KeyCode::Char('o') => {
                if state.can_toggle_image_side() {
                    self.record_feature("review.toggle_image_side");
                }
                state.toggle_image_side();
            }
            KeyCode::Char('z') => state.toggle_comment_fold(),
            KeyCode::Char('v') if state.focus == ReviewFocus::Body => state.toggle_visual(),
            // Enter: toggle a directory in the tree, or open the comment box in
            // the body.
            KeyCode::Enter if state.focus == ReviewFocus::FileList => state.tree_activate(),
            KeyCode::Enter if state.focus == ReviewFocus::Body => {
                state.begin_comment();
            }
            KeyCode::Char('d') if state.focus == ReviewFocus::Body => {
                if let Some(id) = state.comment_at_cursor() {
                    let backend = self.backend_arc(self.backend_of_session(state.session_id));
                    if let Err(e) = backend.delete_comment(state.session_id, id).await {
                        self.set_review_status(&format!("Delete failed: {e}"));
                    } else {
                        self.reload_review_comments(&mut state).await;
                    }
                } else {
                    self.set_review_status("No comment on this line");
                }
            }
            // Toggle the reviewed mark on the file shown in the body (either
            // focus); marking advances to the next unreviewed file. The
            // direction comes from what this view shows, not the store: a mark
            // set elsewhere for newer content reads as unreviewed here, and a
            // store-side toggle would clear it when the user meant to mark.
            KeyCode::Char('m') => {
                if let Some(file) = state.current_file().cloned() {
                    let want = !state.is_reviewed_path(file.display_path());
                    let backend = self.backend_arc(self.backend_of_session(state.session_id));
                    match backend
                        .set_file_reviewed(state.session_id, file.display_path().to_string(), want)
                        .await
                    {
                        Ok(now_reviewed) => {
                            let path = file.display_path().to_string();
                            state.set_reviewed(path.clone(), now_reviewed);
                            if now_reviewed {
                                state.collapse_completed_dirs(Some(&path));
                                state.advance_to_next_unreviewed();
                            }
                        }
                        Err(e) => self.set_review_status(&format!("Mark failed: {e}")),
                    }
                }
            }
            KeyCode::Char('a') => self.apply_review(&mut state).await,
            // Manually re-compose the diff against the working tree, folding in
            // any edits made since the view opened (e.g. by the agent acting on
            // applied comments). Idle agents trigger this automatically too.
            KeyCode::Char('r') => {
                let (sid, title, prev_hash) =
                    (state.session_id, state.title.clone(), state.content_hash);
                self.spawn_review_refresh(sid, title, prev_hash, true);
            }
            // Expand more context above / below the hunk under the cursor
            // (GitHub-style). The gap above the cursor's hunk grows upward
            // toward it; the gap below grows downward from it.
            KeyCode::Char('{') => {
                if let Some(hunk) = state.hunk_of_cursor() {
                    self.record_feature("review.expand_context");
                    state.expand_gap(hunk, ExpandAction::Up);
                    // Revealed rows push the cursor's hunk down; keep it visible.
                    state.follow_cursor();
                }
            }
            KeyCode::Char('}') => {
                if let Some(hunk) = state.hunk_of_cursor() {
                    self.record_feature("review.expand_context");
                    state.expand_gap(hunk + 1, ExpandAction::Down);
                    state.follow_cursor();
                }
            }
            // A key this view doesn't bind: the switcher, or nothing at all.
            // The palette takes the review state with it, so return rather than
            // falling through to the restore below.
            _ if opens_switcher => {
                self.open_review_switcher(state).await;
                return;
            }
            _ => {}
        }
        // A navigation key or side toggle may have changed the visible file;
        // kick off its lazy image / working-tree-content fetches (so expand
        // controls have concrete lines to reveal), if not already loaded.
        self.ensure_review_image(&state).await;
        self.ensure_review_file_lines(&state).await;
        self.ui_state.modal = Modal::ReviewDiff(state);
    }

    /// Clear the decoded-image cache and bump the review generation. Called when
    /// a review opens so in-flight fetches from the previous review (which
    /// captured the old generation) are dropped on arrival rather than poisoning
    /// the new review's cache.
    pub(super) fn reset_review_images(&self) {
        self.review_images.borrow_mut().clear();
        self.review_image_gen
            .set(self.review_image_gen.get().wrapping_add(1));
        self.invalidate_review_file_lines();
    }

    /// Drop the context-expansion file-content caches: clear the in-flight set
    /// and bump the fetch generation so any pre-existing fetch is discarded on
    /// arrival. Called both when a review opens and when its diff is refreshed,
    /// since a refresh replaces the diff the cached lines were indexed against.
    pub(super) fn invalidate_review_file_lines(&self) {
        self.review_file_loads.borrow_mut().clear();
        self.review_file_gen
            .set(self.review_file_gen.get().wrapping_add(1));
    }

    /// Ensure the binary image for the currently-shown file+side is being (or
    /// has been) loaded. Inserts a `Pending` marker and spawns an off-thread
    /// fetch+decode that reports back via [`StateUpdate::ReviewImageLoaded`].
    /// Cheap no-op when the current file isn't an image or is already cached.
    pub(super) async fn ensure_review_image(&self, state: &DiffReviewState) {
        let Some(file) = state.current_file() else {
            return;
        };
        let Some(info) = file.binary.as_ref() else {
            return;
        };
        if !matches!(
            info.kind,
            claude_commander_core::git::BinaryKind::Image { .. }
        ) {
            return;
        }
        let side = shown_image_side(file, state.image_side);
        let path = side_path(file, side).to_string();
        let key = (path.clone(), side);

        if self.review_images.borrow().contains_key(&key) {
            return;
        }
        // Mark in-flight before the first await so repeated renders/keypresses
        // don't spawn duplicate fetches.
        self.review_images
            .borrow_mut()
            .insert(key, ImageEntry::Pending);

        let backend = self.backend_arc(self.backend_of_session(state.session_id));
        let sid = state.session_id;
        let tx = self.event_loop.sender();
        let generation = self.review_image_gen.get();
        tokio::spawn(async move {
            // Fetch the blob bytes through the backend that owns the session — a
            // local git read, or a remote fetch over the wire — then decode off
            // the async runtime, since decoding is CPU-bound. A fetch failure is
            // reported as `Failed` via the same event path as a decode failure.
            let bytes = backend.fetch_diff_blob(sid, side, path.clone()).await;
            let image = match bytes {
                Err(e) => Err(format!("read failed: {e}")),
                Ok(b) => tokio::task::spawn_blocking(move || {
                    image::load_from_memory(&b)
                        .map(std::sync::Arc::new)
                        .map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(format!("decode task failed: {e}"))),
            };
            let _ = tx
                .send(AppEvent::StateUpdate(StateUpdate::ReviewImageLoaded {
                    generation,
                    path,
                    side,
                    image,
                }))
                .await;
        });
    }

    /// Ensure the current file's working-tree content is being (or has been)
    /// fetched, so context expansion has concrete lines to reveal. Fetched
    /// through the backend that owns the session (local read or remote GET) and
    /// reported back via [`StateUpdate::ReviewFileLines`]. Cheap no-op when the
    /// file can't be expanded, is already loaded, or a fetch is in flight.
    pub(super) async fn ensure_review_file_lines(&self, state: &DiffReviewState) {
        if !state.current_file_expandable() {
            return;
        }
        let Some(file) = state.current_file() else {
            return;
        };
        let path = file.display_path().to_string();
        // Already loaded into the view, or a fetch is already in flight.
        if state.file_lines.is_loaded(&path)
            || !self.review_file_loads.borrow_mut().insert(path.clone())
        {
            return;
        }

        let backend = self.backend_arc(self.backend_of_session(state.session_id));
        let sid = state.session_id;
        let tx = self.event_loop.sender();
        let generation = self.review_file_gen.get();
        tokio::spawn(async move {
            let lines = backend
                .fetch_diff_blob(sid, DiffSide::New, path.clone())
                .await
                .map(|bytes| split_file_lines(&bytes))
                .map_err(|e| e.to_string());
            let _ = tx
                .send(AppEvent::StateUpdate(StateUpdate::ReviewFileLines {
                    generation,
                    session_id: sid,
                    path,
                    lines,
                }))
                .await;
        });
    }

    async fn handle_review_comment_key(&mut self, key: KeyEvent, state: &mut DiffReviewState) {
        use crossterm::event::{Event, KeyCode};
        let Some(draft) = state.comment.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                state.comment = None;
            }
            KeyCode::Enter => {
                let draft = state.comment.take().expect("comment present");
                if draft.input.value().trim().is_empty() {
                    return;
                }
                let range = draft.range;
                if let Some(ann) = state.build_draft(range, draft.input.value().to_string()) {
                    let backend = self.backend_arc(self.backend_of_session(state.session_id));
                    match backend.create_comment(state.session_id, ann).await {
                        Ok(_) => {
                            state.visual_anchor = None;
                            self.reload_review_comments(state).await;
                        }
                        Err(e) => self.set_review_status(&format!("Comment failed: {e}")),
                    }
                }
            }
            // Everything else (chars, backspace, delete, arrows, Home/End,
            // word/line shortcuts) is `tui-input`'s standard edit keymap.
            _ => {
                if let Some(req) = tui_input::backend::crossterm::to_input_request(&Event::Key(key))
                {
                    draft.input.handle(req);
                    state.follow_draft();
                }
            }
        }
    }

    /// Apply staged comments and report the outcome.
    async fn apply_review(&mut self, state: &mut DiffReviewState) {
        use claude_commander_core::comment::ApplyOutcome;
        let backend = self.backend_arc(self.backend_of_session(state.session_id));
        match backend.apply_comments(state.session_id).await {
            Ok(ApplyOutcome::Nothing) => {
                self.set_review_status("No staged comments to apply");
                // "Nothing" is exactly what an orphan-only staged set produces
                // (the filter emptied it), so this needs the refresh most: it is
                // what turns the silent nothing into "Dropped 1 comment — …".
                self.refresh_after_apply(state);
            }
            Ok(ApplyOutcome::Blocked { drifted }) => {
                self.set_review_status(&state.drift_block_message(&drifted));
            }
            Ok(ApplyOutcome::Applied { count, .. }) => {
                self.reload_review_comments(state).await;
                self.set_review_status(&format!("Sent {count} comment(s) to the agent"));
                self.refresh_after_apply(state);
            }
            Ok(ApplyOutcome::Deferred { count, .. }) => {
                self.reload_review_comments(state).await;
                self.set_review_status(&format!(
                    "{count} comment(s) queued — agent busy or stopped"
                ));
                self.refresh_after_apply(state);
            }
            Err(e) => self.set_review_status(&format!("Apply failed: {e}")),
        }
    }

    /// Re-compose the review after an apply attempt.
    ///
    /// Apply deliberately leaves comments whose file left the diff in the store
    /// (it can't announce a deletion — see `apply_comments`), so this is what
    /// reaches the one path that both drops them and says so. A file can only
    /// have left the diff if the diff moved, so the hash differs and the refresh
    /// really does produce a snapshot; when nothing moved it short-circuits to
    /// `None` and, being non-manual, stays silent.
    ///
    /// Best-effort, not a guarantee: `spawn_review_refresh` coalesces against an
    /// in-flight refresh, so a transition-driven one racing this drops it. The
    /// consequence is only delay — the comment stays on disk and the next
    /// compose (`r`, an agent turn, or reopening the review) drops it with the
    /// notice. Nothing is ever deleted without being reported.
    fn refresh_after_apply(&mut self, state: &DiffReviewState) {
        self.spawn_review_refresh(
            state.session_id,
            state.title.clone(),
            state.content_hash,
            false,
        );
    }

    /// Render the full-screen review view. Returns the clickable footer-button
    /// regions drawn (recorded for mouse hit-testing).
    pub(super) fn render_review_modal(
        &self,
        frame: &mut Frame,
        area: Rect,
        state: &DiffReviewState,
    ) -> Vec<ReviewButton> {
        frame.render_widget(Clear, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(20), Constraint::Min(0)])
            .split(rows[0]);

        self.render_review_file_list(frame, cols[0], state);
        self.render_review_body(frame, cols[1], state);

        self.render_review_footer(frame, rows[1], state)
    }

    /// Render the review view's footer — this view's status bar — as bracketed,
    /// clickable buttons plus plain non-actionable key hints, varying by focus /
    /// mode (comment editing, visual select, file-list, diff body). The `close`
    /// button is pinned to the right edge so it survives when the row is narrow.
    fn render_review_footer(
        &self,
        frame: &mut Frame,
        area: Rect,
        state: &DiffReviewState,
    ) -> Vec<ReviewButton> {
        use crossterm::event::{KeyCode, KeyModifiers};

        let base = self.theme.status_bar();
        let accent = base.fg(self.theme.text_accent);

        // The session-toggle key (default Alt-r) re-attaches; shown only when a
        // binding exists. Cloned so it outlives the borrow.
        let toggle_key = claude_commander_core::config::keybindings::review_toggle_binding(
            &self.config.keybindings,
        );

        let key = |code, mods| KeyEvent::new(code, mods);
        let none = KeyModifiers::NONE;

        // Ordered footer items per sub-mode. `Plain` items are non-actionable
        // key legends; `Button`s replay the key they label on click.
        let mut live_toast = self
            .ui_state
            .status_message
            .as_ref()
            .filter(|(_, expires)| Instant::now() < *expires)
            .map(|(msg, _)| msg.clone());
        let mut items: Vec<FooterItem> = Vec::new();
        if state.comment.is_some() {
            // A live status message (e.g. "● Dictating…" while the comment is
            // being spoken) takes the legends' place, so it is visible exactly
            // where it applies. It goes *after* the editor's buttons: the row
            // truncates a toast that overflows and drops everything after it,
            // and a long error must not cost save/cancel.
            let toast = live_toast.take();
            if toast.is_none() {
                items.push(FooterItem::Plain("type comment"));
                items.push(FooterItem::Plain("←→/Home/End move"));
            }
            items.push(FooterItem::button("save", key(KeyCode::Enter, none)));
            items.push(FooterItem::button("cancel", key(KeyCode::Esc, none)));
            items.extend(toast.map(FooterItem::Toast));
        } else if state.visual_anchor.is_some() {
            items.push(FooterItem::Plain("↑↓ extend"));
            items.push(FooterItem::button("comment", key(KeyCode::Enter, none)));
            items.push(FooterItem::button(
                "cancel selection",
                key(KeyCode::Char('v'), none),
            ));
        } else if state.focus == ReviewFocus::FileList {
            items.push(FooterItem::Plain("↑↓/jk move"));
            items.push(FooterItem::button("expand", key(KeyCode::Enter, none)));
            items.push(FooterItem::Plain("[ ] file"));
            items.push(FooterItem::button(
                "reviewed",
                key(KeyCode::Char('m'), none),
            ));
            items.push(FooterItem::button("diff", key(KeyCode::Tab, none)));
        } else {
            items.push(FooterItem::Plain("↑↓/jk move"));
            items.push(FooterItem::button("select", key(KeyCode::Char('v'), none)));
            items.push(FooterItem::button("comment", key(KeyCode::Enter, none)));
            items.push(FooterItem::button("fold", key(KeyCode::Char('z'), none)));
            items.push(FooterItem::button("delete", key(KeyCode::Char('d'), none)));
            items.push(FooterItem::button(
                "reviewed",
                key(KeyCode::Char('m'), none),
            ));
            items.push(FooterItem::button("apply", key(KeyCode::Char('a'), none)));
            items.push(FooterItem::button("refresh", key(KeyCode::Char('r'), none)));
            items.push(FooterItem::button("layout", key(KeyCode::Char('t'), none)));
            items.push(FooterItem::Plain("{ } context"));
            // Offer the image side-toggle only when it does something: a binary
            // image with two sides (a modification).
            if state.can_toggle_image_side() {
                items.push(FooterItem::button(
                    "before/after",
                    key(KeyCode::Char('o'), none),
                ));
            }
        }
        // Open the session's worktree in the editor (default `.`), shown only
        // outside comment/visual sub-modes and when the owning backend can drive
        // the operator's local editor (remote sessions can't).
        if state.comment.is_none()
            && state.visual_anchor.is_none()
            && let Some(kb) = self
                .config
                .keybindings
                .keys_for(BindableAction::OpenInEditor)
                .first()
            && self
                .backend_arc(self.backend_of_session(state.session_id))
                .capabilities()
                .open_editor
        {
            items.push(FooterItem::Button {
                label: "edit",
                key: KeyEvent::new(kb.code, kb.modifiers),
            });
        }

        // The session re-attach toggle is available in every non-comment mode.
        if state.comment.is_none()
            && let Some(kb) = &toggle_key
        {
            items.push(FooterItem::Button {
                label: "session",
                key: KeyEvent::new(kb.code, kb.modifiers),
            });
        }

        // The session switcher, last so it is the first thing dropped when the
        // row is too narrow for every item. Labelled with Ctrl+Space rather
        // than the leader key because it is the binding that holds in every
        // mode this footer is drawn for (the leader only opens the switcher
        // where the view has no meaning of its own for that key). Not offered
        // while editing a comment, where the box owns every key — the same
        // reason `close` isn't.
        if state.comment.is_none() {
            items.push(FooterItem::button(
                "switch",
                key(KeyCode::Char(' '), KeyModifiers::CONTROL),
            ));
        }

        // A live status message momentarily claims the footer (this view's
        // status bar), mirroring the main status bar where a toast replaces the
        // action buttons. Without this the review view — a full-screen takeover
        // that never draws the normal status bar — would silently swallow every
        // `set_review_status` (apply/refresh/mark results, editor errors, …).
        // While editing a comment it shares the row with the editor's buttons
        // instead (above), so save/cancel stay clickable.
        if state.comment.is_none()
            && let Some(msg) = live_toast
        {
            items = vec![FooterItem::Toast(msg)];
        }

        // Close is pinned to the right edge (Ctrl-Q always closes the view) —
        // but not while editing a comment, where keys route to the comment box
        // (a synthesized Ctrl-Q would type into it); "cancel" (Esc) exits there.
        let close = (state.comment.is_none())
            .then(|| FooterItem::button("close", key(KeyCode::Char('q'), KeyModifiers::CONTROL)));

        self.render_footer_items(frame, area, &items, close.as_ref(), base, accent)
    }

    /// Lay out footer `items` left-to-right (dropping any that overflow) with
    /// `close` pinned to the right edge, recording each button's clickable rect.
    fn render_footer_items(
        &self,
        frame: &mut Frame,
        area: Rect,
        items: &[FooterItem],
        close: Option<&FooterItem>,
        base: Style,
        accent: Style,
    ) -> Vec<ReviewButton> {
        const SEP: &str = " · ";
        const SEP_WIDTH: u16 = 3;

        let mut buttons: Vec<ReviewButton> = Vec::new();

        // Reserve the right edge for the close button (when shown) so it always
        // survives overflow; without one, items use the full width.
        let close_rendered = close.map(|c| c.render(base, accent));
        let close_width = close_rendered.as_ref().map_or(0, |(_, w)| *w);
        let close_x = area.right().saturating_sub(close_width);
        let items_right = if close.is_some() {
            close_x.saturating_sub(SEP_WIDTH)
        } else {
            area.right()
        };

        // Left-aligned items, dropping whole ones that would collide with the
        // reserved close zone.
        let mut spans: Vec<Span> = vec![Span::styled(" ", base)];
        let mut x = area.x + 1;
        for (i, item) in items.iter().enumerate() {
            let (item_spans, width) = item.render(base, accent);
            let lead = if i == 0 { 0 } else { SEP_WIDTH };
            if x.saturating_add(lead).saturating_add(width) > items_right {
                // A `Toast` is the review view's only status channel, so on a
                // narrow terminal truncate it to fit rather than dropping it,
                // which would blank the footer for the toast's lifetime — the
                // exact invisibility this override exists to prevent. Optional
                // key-hint items still just drop on overflow.
                if let FooterItem::Toast(text) = item {
                    if i != 0 {
                        spans.push(Span::styled(SEP, base));
                        x += SEP_WIDTH;
                    }
                    let avail = items_right.saturating_sub(x) as usize;
                    let truncated = super::settings::truncate_str(text, avail);
                    spans.push(Span::styled(truncated, base.add_modifier(Modifier::BOLD)));
                }
                break;
            }
            if i != 0 {
                spans.push(Span::styled(SEP, base));
                x += SEP_WIDTH;
            }
            if let FooterItem::Button { key, .. } = item {
                buttons.push(ReviewButton {
                    rect: Rect {
                        x,
                        y: area.y,
                        width,
                        height: 1,
                    },
                    key: *key,
                });
            }
            spans.extend(item_spans);
            x += width;
        }

        // The footer doubles as this view's status bar — styled like the app
        // status bar so it reads as a replacement, not a second bar.
        frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);

        // Render the pinned close button in its reserved right slot.
        if let (Some((close_spans, _)), Some(FooterItem::Button { key, .. })) =
            (close_rendered, close)
        {
            buttons.push(ReviewButton {
                rect: Rect {
                    x: close_x,
                    y: area.y,
                    width: close_width,
                    height: 1,
                },
                key: *key,
            });
            let close_area = Rect {
                x: close_x,
                y: area.y,
                width: close_width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(close_spans)).style(base),
                close_area,
            );
        }

        buttons
    }

    fn render_review_file_list(&self, frame: &mut Frame, area: Rect, state: &DiffReviewState) {
        let focused = state.focus == ReviewFocus::FileList;
        let pal = self.theme.review_palette();
        let border = if focused {
            pal.border_focused
        } else {
            pal.border_unfocused
        };

        let rows = state.visible_rows();
        // Inner content width (pane minus the two border columns), so a reviewed
        // row's background band can be padded to fill the full pane.
        let inner_width = area.width.saturating_sub(2) as usize;
        let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            // The cursor row is highlighted whether or not this pane has focus —
            // unfocused it wears a muted one (see `selection_bg_unfocused`), so
            // the row you navigated to stays identifiable while you read the
            // body. (The cursor can rest on a directory, in which case it marks
            // that row and not the file the body is showing — same as when the
            // list is focused; moving onto a directory never changes the body.)
            let on_cursor = i == state.tree_cursor;
            let line = match row {
                TreeRow::Dir {
                    depth,
                    name,
                    collapsed,
                    path,
                } => {
                    let indent = "  ".repeat(*depth);
                    let chevron = if *collapsed { '▶' } else { '▼' };
                    let mut spans = vec![Span::styled(
                        format!("{indent}{chevron} {name}"),
                        Style::default().fg(pal.dir_fg).add_modifier(Modifier::BOLD),
                    )];
                    spans.extend(comment_badge_span(state.dir_comment_count(path), &pal));
                    if on_cursor {
                        spans = select_spans(spans, &pal, focused);
                    }
                    Line::from(spans)
                }
                TreeRow::File { depth, index, name } => {
                    let file = &state.diff.files[*index];
                    let reviewed = state.is_reviewed_path(file.display_path());
                    // Reviewed rows are dimmed so the remaining work stands out.
                    let dim = if reviewed {
                        Modifier::DIM
                    } else {
                        Modifier::empty()
                    };
                    let indent = "  ".repeat(*depth);
                    let marker = file.status.marker();
                    // Only the status letter is coloured; the file name stays
                    // the default foreground.
                    let mut spans = vec![
                        Span::raw(format!("{indent}  ")),
                        Span::styled(
                            marker.to_string(),
                            Style::default()
                                .fg(file_status_color(file.status, &pal))
                                .add_modifier(dim),
                        ),
                        Span::styled(format!(" {name}"), Style::default().add_modifier(dim)),
                    ];
                    spans.extend(reviewed_check_span(reviewed, &pal));
                    spans.extend(comment_badge_span(
                        state.comment_count(file.display_path()),
                        &pal,
                    ));
                    // Band reviewed rows so "read" files stand out; the cursor
                    // selection still wins on the focused row.
                    spans = apply_reviewed_bg(spans, reviewed, inner_width, &pal);
                    if on_cursor {
                        spans = select_spans(spans, &pal, focused);
                    }
                    Line::from(spans)
                }
            };
            lines.push(line);
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(Style::default().fg(border))
            .title(match state.reviewed.len() {
                0 => format!(" Files ({}) ", state.diff.files.len()),
                n => format!(" Files ({n}/{} reviewed) ", state.diff.files.len()),
            });
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .scroll((state.tree_scroll, 0)),
            area,
        );
    }

    fn render_review_body(&self, frame: &mut Frame, area: Rect, state: &DiffReviewState) {
        let focused = state.focus == ReviewFocus::Body;
        let pal = self.theme.review_palette();
        let border = if focused {
            pal.border_focused
        } else {
            pal.border_unfocused
        };

        let title = match state.current_file() {
            Some(f) if state.is_reviewed_path(f.display_path()) => {
                format!(" {} — vs {} ✓ reviewed ", f.display_path(), state.base)
            }
            Some(f) => format!(" {} — vs {} ", f.display_path(), state.base),
            None => format!(" review — vs {} ", state.base),
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(Style::default().fg(border))
            .title(title);

        // Binary files (images, blobs) carry no textual hunks — render the image
        // (or a placeholder) instead of the line-based diff body.
        if let Some(file) = state.current_file()
            && let Some(info) = file.binary.as_ref()
        {
            self.render_review_binary(frame, area, block, state, file, info);
            return;
        }

        // Syntax highlighting emits RGB foregrounds, so only apply it on
        // true-color terminals; below that the palette's text colour carries the
        // whole line, which is what an absent highlighter already means.
        let highlighter = (self.theme.mode == ColorMode::TrueColor).then_some(SyntectHighlighter);
        let ext = state
            .current_file()
            .map(|f| file_extension(f.display_path()).to_string())
            .unwrap_or_default();
        let inner = block.inner(area);
        // Record the pane's inner size so keypress-time cursor/scroll math wraps
        // and scrolls exactly as we render here.
        state
            .body
            .set((inner.width as usize, inner.height as usize));
        let lines = review_body_lines(
            state,
            focused,
            &pal,
            &ext,
            highlighter.as_ref().map(|h| h as &dyn Highlighter),
            self.config.rounded_borders,
        );

        frame.render_widget(
            Paragraph::new(lines).block(block).scroll((state.scroll, 0)),
            area,
        );
    }

    /// Render a binary file's review body: the decoded image via `ratatui-image`
    /// (graphics protocol or half-block fallback), or a placeholder for
    /// non-image blobs and not-yet-loaded images. Bytes are fetched lazily — see
    /// `ensure_review_image`.
    fn render_review_binary(
        &self,
        frame: &mut Frame,
        area: Rect,
        block: Block<'static>,
        state: &DiffReviewState,
        file: &FileDiff,
        info: &claude_commander_core::git::BinaryInfo,
    ) {
        let pal = self.theme.review_palette();
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let side = shown_image_side(file, state.image_side);
        let size = match side {
            DiffSide::Old => info.old_size,
            DiffSide::New => info.new_size,
        };
        let side_label = match side {
            DiffSide::Old => "before",
            DiffSide::New => "after",
        };

        if !matches!(
            info.kind,
            claude_commander_core::git::BinaryKind::Image { .. }
        ) {
            let note = format!(
                "Binary file · {} · not a previewable image",
                human_size(size)
            );
            frame.render_widget(
                Paragraph::new(note)
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(pal.text)),
                inner,
            );
            return;
        }

        // Caption (one row) above the image area.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        let caption = image_caption(file.status, side_label, size);
        frame.render_widget(
            Paragraph::new(caption).style(Style::default().fg(pal.gutter_fg)),
            rows[0],
        );

        let path = side_path(file, side).to_string();
        let mut images = self.review_images.borrow_mut();
        match images.get_mut(&(path, side)) {
            Some(ImageEntry::Ready(proto)) => {
                frame.render_stateful_widget(
                    ratatui_image::StatefulImage::default(),
                    rows[1],
                    proto.as_mut(),
                );
            }
            Some(ImageEntry::Failed(e)) => {
                frame.render_widget(
                    Paragraph::new(format!("Failed to load image: {e}"))
                        .alignment(Alignment::Center)
                        .style(Style::default().fg(pal.del_fg)),
                    rows[1],
                );
            }
            // Pending, or not yet requested (the request is kicked off by the
            // navigation handlers, which insert `Pending` before this renders).
            _ => {
                frame.render_widget(
                    Paragraph::new("Loading image…")
                        .alignment(Alignment::Center)
                        .style(Style::default().fg(pal.gutter_fg)),
                    rows[1],
                );
            }
        }
    }
}

/// Caption shown above a review image: side label + size, plus a `press o to
/// toggle` hint only when the file is a modification (the sole case where `o`
/// has two sides to flip between — added/deleted files are single-sided).
fn image_caption(status: FileStatus, side_label: &str, size: Option<u64>) -> String {
    if status == FileStatus::Modified {
        format!(" {side_label} · {} · press o to toggle ", human_size(size))
    } else {
        format!(" {side_label} · {} ", human_size(size))
    }
}

/// Human-readable byte size for an optional count (`None` → `"? bytes"`).
fn human_size(size: Option<u64>) -> String {
    match size {
        None => "? bytes".to_string(),
        Some(n) if n < 1024 => format!("{n} bytes"),
        Some(n) if n < 1024 * 1024 => format!("{:.1} KiB", n as f64 / 1024.0),
        Some(n) => format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0)),
    }
}

/// Normalise a review-view key into the keycode the dispatch matches on.
/// `Ctrl-n`/`Ctrl-p` are folded onto `Down`/`Up` so they act as navigation
/// aliases for the arrow keys (and `j`/`k`), mirroring the other list modals;
/// every other key passes through unchanged.
/// Telemetry feature name for a review-view key, or `None` for keys with no
/// tracked feature (navigation, scroll, file movement — pure noise).
///
/// These are UI-only actions handled directly in [`Self::handle_review_key`];
/// unlike the comment create/delete/apply mutations they never reach an
/// instrumented [`CommanderService`] method, so they are recorded inline. The
/// `code` is the navigation-normalised keycode (post [`review_nav_keycode`]),
/// and `focus` gates `v`, which only enters visual mode in the body. Kept pure
/// and free-standing so it is unit-testable without driving the async handler.
///
/// `o` (image-side toggle) is recorded separately in the handler because
/// whether it does anything depends on the current file (see
/// [`DiffReviewState::can_toggle_image_side`]), which this key-only mapping
/// can't see — counting every `o` would inflate the metric with no-op presses.
fn review_key_feature(code: crossterm::event::KeyCode, focus: ReviewFocus) -> Option<&'static str> {
    use crossterm::event::KeyCode;
    match code {
        KeyCode::Char('t') => Some("review.toggle_layout"),
        KeyCode::Char('z') => Some("review.toggle_fold"),
        KeyCode::Char('r') => Some("review.refresh"),
        KeyCode::Char('v') if focus == ReviewFocus::Body => Some("review.visual_select"),
        _ => None,
    }
}

fn review_nav_keycode(key: crossterm::event::KeyEvent) -> crossterm::event::KeyCode {
    use crossterm::event::{KeyCode, KeyModifiers};
    match key.code {
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyCode::Down,
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyCode::Up,
        other => other,
    }
}

/// Inner rects of the (file list, diff body) panes for a given modal `area` —
/// the regions a mouse position maps into. Must mirror the layout in
/// `render_review_modal`.
fn review_inner_rects(area: Rect) -> (Rect, Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(20), Constraint::Min(0)])
        .split(rows[0]);
    // Inset by each pane block's border.
    let inset = Margin {
        vertical: 1,
        horizontal: 1,
    };
    (cols[0].inner(inset), cols[1].inner(inset))
}

/// Inner rect of the diff body pane (see [`review_inner_rects`]).
pub(super) fn review_body_inner_rect(area: Rect) -> Rect {
    review_inner_rects(area).1
}

/// Inner rect of the file-list pane (see [`review_inner_rects`]).
pub(super) fn review_file_list_inner_rect(area: Rect) -> Rect {
    review_inner_rects(area).0
}

/// Build a file tree from the diff's files (keyed on each file's display path),
/// then compress single-child directory chains (lazygit-style).
fn build_file_tree(files: &[FileDiff]) -> Vec<TreeNode> {
    let mut roots: Vec<TreeNode> = Vec::new();
    for (idx, file) in files.iter().enumerate() {
        let segments: Vec<&str> = file.display_path().split('/').collect();
        insert_path(&mut roots, &segments, idx, "");
    }
    for node in &mut roots {
        compress(node);
    }
    roots
}

/// Insert a file's path segments into the tree, creating directory nodes.
fn insert_path(children: &mut Vec<TreeNode>, segments: &[&str], file_index: usize, prefix: &str) {
    let Some((head, rest)) = segments.split_first() else {
        return;
    };
    let path = if prefix.is_empty() {
        head.to_string()
    } else {
        format!("{prefix}/{head}")
    };
    if rest.is_empty() {
        children.push(TreeNode {
            name: head.to_string(),
            path,
            file_index: Some(file_index),
            children: Vec::new(),
        });
        return;
    }
    let pos = children
        .iter()
        .position(|n| n.file_index.is_none() && n.name == *head);
    let idx = match pos {
        Some(i) => i,
        None => {
            children.push(TreeNode {
                name: head.to_string(),
                path: path.clone(),
                file_index: None,
                children: Vec::new(),
            });
            children.len() - 1
        }
    };
    insert_path(&mut children[idx].children, rest, file_index, &path);
}

/// Merge a directory with its sole child when that child is also a directory,
/// repeatedly, then recurse. `a` → `b` → files becomes `a/b`.
fn compress(node: &mut TreeNode) {
    while node.file_index.is_none()
        && node.children.len() == 1
        && node.children[0].file_index.is_none()
    {
        let child = node.children.remove(0);
        node.name = format!("{}/{}", node.name, child.name);
        node.path = child.path;
        node.children = child.children;
    }
    for child in &mut node.children {
        compress(child);
    }
}

/// Flatten the tree into visible rows, skipping collapsed directories' subtrees.
fn flatten_tree(
    nodes: &[TreeNode],
    depth: usize,
    collapsed: &HashSet<String>,
    out: &mut Vec<TreeRow>,
) {
    for node in nodes {
        match node.file_index {
            Some(index) => out.push(TreeRow::File {
                depth,
                index,
                name: node.name.clone(),
            }),
            None => {
                let is_collapsed = collapsed.contains(&node.path);
                out.push(TreeRow::Dir {
                    depth,
                    path: node.path.clone(),
                    name: node.name.clone(),
                    collapsed: is_collapsed,
                });
                if !is_collapsed {
                    flatten_tree(&node.children, depth + 1, collapsed, out);
                }
            }
        }
    }
}

/// Whether every file in `node`'s subtree is marked reviewed. A leaf node's
/// `path` is its `display_path` (built from the same segments), which is the key
/// `reviewed` stores, so it can be checked directly without the file list.
fn subtree_all_reviewed(node: &TreeNode, reviewed: &HashSet<String>) -> bool {
    match node.file_index {
        Some(_) => reviewed.contains(&node.path),
        None => node
            .children
            .iter()
            .all(|c| subtree_all_reviewed(c, reviewed)),
    }
}

/// A fresh [`DiffReviewState::reviewed_gen`], unique across every view this
/// process opens.
fn next_reviewed_gen() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Collect the paths of directory nodes whose entire file subtree is reviewed
/// — the directories that should auto-collapse. With a `target`, only those
/// containing it (the file just marked read); with `None`, every one.
fn collect_completed_dirs(
    nodes: &[TreeNode],
    target: Option<&str>,
    reviewed: &HashSet<String>,
    out: &mut Vec<String>,
) {
    for node in nodes {
        if node.file_index.is_some() {
            continue;
        }
        // `target` is under this dir when stripping the dir path leaves a
        // `/`-rooted remainder; the leading slash stops "dir" matching "dir2/…".
        if target.is_some_and(|t| {
            !t.strip_prefix(node.path.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        }) {
            continue;
        }
        if subtree_all_reviewed(node, reviewed) {
            out.push(node.path.clone());
        }
        collect_completed_dirs(&node.children, target, reviewed, out);
    }
}

/// First file index in tree order (depth-first) whose index satisfies `pred`,
/// if any. `first_file_index` and `first_unreviewed_file_index` are thin
/// predicate wrappers over this walker.
fn find_file_index(nodes: &[TreeNode], pred: &impl Fn(usize) -> bool) -> Option<usize> {
    for node in nodes {
        if let Some(i) = node.file_index.filter(|&i| pred(i)) {
            return Some(i);
        }
        if let Some(i) = find_file_index(&node.children, pred) {
            return Some(i);
        }
    }
    None
}

/// Index of the first file in tree order (depth-first), if any.
fn first_file_index(nodes: &[TreeNode]) -> Option<usize> {
    find_file_index(nodes, &|_| true)
}

/// First file index (in tree display order) whose file is not marked reviewed,
/// or `None` when every file is reviewed.
fn first_unreviewed_file_index(
    nodes: &[TreeNode],
    diff: &ParsedDiff,
    reviewed: &HashSet<String>,
) -> Option<usize> {
    find_file_index(nodes, &|i| !reviewed.contains(diff.files[i].display_path()))
}

/// Colour used for a file row by its change status, from the theme palette.
fn file_status_color(status: FileStatus, pal: &ReviewPalette) -> Color {
    match status {
        FileStatus::Added => pal.add_fg,
        FileStatus::Deleted => pal.del_fg,
        FileStatus::Modified => pal.modified_fg,
        FileStatus::Renamed => pal.renamed_fg,
    }
}

/// The ` *N` pending-comment badge for a file-tree row, comment-coloured so it
/// stands out from the file name, or `None` when there are no pending comments.
fn comment_badge_span(count: usize, pal: &ReviewPalette) -> Option<Span<'static>> {
    (count > 0).then(|| {
        Span::styled(
            format!(" {COMMENT_MARKER}{count}"),
            Style::default().fg(pal.comment_border),
        )
    })
}

/// The ` ✓` reviewed check for a file-tree row, add-coloured so it reads as
/// "done", or `None` when the file is not marked reviewed.
fn reviewed_check_span(reviewed: bool, pal: &ReviewPalette) -> Option<Span<'static>> {
    reviewed.then(|| Span::styled(" ✓", Style::default().fg(pal.add_fg)))
}

/// Lay the subtle "read" background band across a reviewed file-tree row,
/// padding the line out to `width` so the band fills the whole pane rather than
/// only sitting behind the text. Returns the spans untouched when the row isn't
/// reviewed, so unread files (where the work is) keep the default background.
fn apply_reviewed_bg(
    mut spans: Vec<Span<'static>>,
    reviewed: bool,
    width: usize,
    pal: &ReviewPalette,
) -> Vec<Span<'static>> {
    if !reviewed {
        return spans;
    }
    for span in &mut spans {
        span.style = span.style.bg(pal.reviewed_bg);
    }
    // Display columns, not chars: a path with a CJK or emoji component is wider
    // than its character count, and padding by that count overruns the pane.
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    if width > used {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(pal.reviewed_bg),
        ));
    }
    spans
}

/// Build the rendered body for the current file, in whichever layout is active.
///
/// The row structure is entirely `diffgrid`'s: it decides what rows exist (hunk
/// headers, diff lines, revealed context, expand controls, attached comment
/// boxes), how they wrap, and what each run of text *is*. This function does the
/// two things only the host can: it draws the comment boxes it attached, and it
/// turns semantic roles into `ratatui` styles through the theme's palette.
fn review_body_lines(
    state: &DiffReviewState,
    focused: bool,
    pal: &ReviewPalette,
    ext: &str,
    highlighter: Option<&dyn Highlighter>,
    rounded: bool,
) -> Vec<Line<'static>> {
    let width = state.body_width();
    // One pass over the comments per frame rather than one per rendered row:
    // the marker lookup is called for every line's gutter.
    let markers = state.comment_markers();
    let marker_at = |sel: SelIdx| match markers.get(&sel.get()) {
        Some(true) => Some(DRIFT_MARKER),
        Some(false) => Some(COMMENT_MARKER),
        None => None,
    };
    let language = (!ext.is_empty()).then_some(ext);
    let ctx = state.row_context(highlighter, language, &marker_at, focused);
    let (sel_lo, sel_hi) = state.selection();
    let selected = |sel: Option<SelIdx>| {
        focused && sel.is_some_and(|s| s.get() >= sel_lo && s.get() <= sel_hi)
    };

    state
        .with_view(|view| {
            let mut out: Vec<Line<'static>> = Vec::new();
            for row in view.layout.rows() {
                let kind = row.kind();
                // An attached block is the host's own: `diffgrid` only ever asked
                // how tall it was.
                if kind == RowKind::Attached {
                    out.extend(attached_block_lines(
                        state,
                        row.attach_key(),
                        width,
                        pal,
                        rounded,
                    ));
                    continue;
                }
                let role = row_role(kind, row.origin());
                if state.layout == ReviewLayout::SideBySide && !is_full_width(kind) {
                    out.push(sbs_row_line(view, &ctx, pal, row.index(), state, focused));
                    continue;
                }
                let spans = row_spans(&view.file, &view.layout, row.index(), &ctx);
                let pad = SpanStyle::new(role).with_selected(selected(row.sel()));
                // Only the kinds `Grid::build` wraps may wrap here. A hunk
                // header or an expand control is one physical row however long
                // its text is, and emitting two would put every mapping below it
                // one row out.
                if is_full_width(kind) {
                    out.push(to_line(fit_spans(spans, width, pad), pal));
                    continue;
                }
                let gutter = gutter_cols(kind, &ctx.gutter);
                let content_width = state.wrap_options().content_width(gutter);
                for physical in wrap_row(spans, gutter, content_width) {
                    out.push(to_line(fit_spans(physical, width, pad), pal));
                }
            }
            out
        })
        .unwrap_or_default()
}

/// One side-by-side row: the two halves, separated by a `│` rule.
///
/// Neither half soft-wraps — two columns wrapping independently would stop
/// lining up, and the pairing is the point of the layout — so each is fitted to
/// its half's width.
fn sbs_row_line(
    view: &FileView,
    ctx: &RowContext<'_>,
    pal: &ReviewPalette,
    row: diffgrid::LogicalIdx,
    state: &DiffReviewState,
    focused: bool,
) -> Line<'static> {
    let half = state.wrap_options().half_width();
    let (sel_lo, sel_hi) = state.selection();
    let logical = view.layout.row(row);
    let kind = logical.map(|r| r.kind()).unwrap_or(RowKind::Line);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for side in [Side::Old, Side::New] {
        if side == Side::New {
            // The rule takes the row's own band so a revealed-context row reads
            // as one continuous stripe across both halves.
            let ink = pal.ink(&SpanStyle::new(row_role(kind, None)));
            let bg = if kind == RowKind::ExpandedContext {
                ink.bg
            } else {
                Color::Reset
            };
            spans.push(Span::styled(
                " │ ",
                Style::default().fg(pal.gutter_fg).bg(bg),
            ));
        }
        let cell = logical.and_then(|r| r.cell(side));
        let Some(cell) = cell else {
            // The blank half of an unbalanced change block: a diagonal hatch, so
            // the eye reads "nothing here" rather than "not drawn yet".
            spans.push(Span::styled(
                "╱".repeat(half),
                Style::default().fg(pal.gap_fg),
            ));
            continue;
        };
        let role = row_role(kind, cell.origin);
        let selected = focused
            && cell
                .sel
                .is_some_and(|s| s.get() >= sel_lo && s.get() <= sel_hi);
        let cell = cell_spans(&view.file, &view.layout, row, side, ctx);
        let pad = SpanStyle::new(role).with_selected(selected);
        spans.extend(to_spans(fit_spans(cell, half, pad), pal));
    }
    Line::from(spans)
}

/// The body role a row of `kind` wears, so its padding extends the row's own
/// tint to the edge of the pane rather than punching a hole in it.
fn row_role(kind: RowKind, origin: Option<diffgrid::LineOrigin>) -> Role {
    match kind {
        RowKind::HunkHeader => Role::HunkHeader,
        RowKind::ExpandedContext => Role::ExpandedContext,
        RowKind::ExpandControl => Role::ExpandControl,
        RowKind::AlignmentGap => Role::AlignmentGap,
        _ => match origin {
            Some(diffgrid::LineOrigin::Addition) => Role::Addition,
            Some(diffgrid::LineOrigin::Deletion) => Role::Deletion,
            _ => Role::Context,
        },
    }
}

/// The host-drawn rows of an attached block: a saved comment's inline box, or
/// the in-progress draft's edit box.
fn attached_block_lines(
    state: &DiffReviewState,
    key: Option<AttachKey>,
    width: usize,
    pal: &ReviewPalette,
    rounded: bool,
) -> Vec<Line<'static>> {
    let Some(key) = key else {
        return Vec::new();
    };
    if key == DRAFT_ATTACH_KEY {
        let Some(draft) = state.comment.as_ref() else {
            return Vec::new();
        };
        return comment_draft_box_lines(
            &super::input_with_caret(&draft.input),
            &draft_loc_label(state, draft.range),
            width,
            pal,
            rounded,
        );
    }
    state
        .comment_by_key(key)
        .map(|ann| comment_box_lines(ann, state.is_comment_collapsed(ann.id), width, pal, rounded))
        .unwrap_or_default()
}

/// Resolve `diffgrid` spans into `ratatui` ones through the theme palette.
fn to_spans(spans: Vec<diffgrid::style::Span<'_>>, pal: &ReviewPalette) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|s| {
            let ink = pal.ink(&s.style);
            let mut style = Style::default().fg(ink.fg).bg(ink.bg);
            if ink.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            Span::styled(s.text.into_owned(), style)
        })
        .collect()
}

/// [`to_spans`], as one rendered line.
fn to_line(spans: Vec<diffgrid::style::Span<'_>>, pal: &ReviewPalette) -> Line<'static> {
    Line::from(to_spans(spans, pal))
}

/// Split raw file bytes into lines (1-based when indexed as `lines[n - 1]`),
/// dropping a single trailing newline's empty tail so the line count matches
/// the file's line numbering. Lossy UTF-8, matching how the diff is decoded.
fn split_file_lines(bytes: &[u8]) -> std::sync::Arc<Vec<String>> {
    if bytes.is_empty() {
        // `"".split('\n')` yields one empty element; an empty file has no lines.
        return std::sync::Arc::new(Vec::new());
    }
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    std::sync::Arc::new(lines)
}

/// File extension (no dot) of a path's final component, or `""` if none.
fn file_extension(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext)
        .unwrap_or("")
}

/// Render an comment as an inline box, visually distinct from the diff.
/// Collapsed → a single rounded header bar with a preview; expanded → the bar
/// plus the wrapped comment and a closing border.
/// Number of rendered rows [`comment_box_lines`] produces for the same inputs,
/// without building the styled lines. Used by the click hit-test to walk the
/// body's row layout. Must track `comment_box_lines`'s structure (guarded by a
/// test).
fn comment_box_height(ann: &Comment, collapsed: bool, width: usize) -> usize {
    const INDENT_LEN: usize = 2; // "  "
    let avail = width.saturating_sub(INDENT_LEN);
    if avail < 8 {
        return 0;
    }
    if collapsed {
        return 1;
    }
    let inner = avail - 2;
    let text_width = inner.saturating_sub(2);
    // Top border + wrapped body lines + bottom border.
    let body: usize = ann
        .comment
        .split('\n')
        .map(|paragraph| wrap_text(paragraph, text_width).len())
        .sum();
    body + 2
}

fn comment_box_lines(
    ann: &Comment,
    collapsed: bool,
    width: usize,
    pal: &ReviewPalette,
    rounded: bool,
) -> Vec<Line<'static>> {
    const INDENT: &str = "  ";
    let avail = width.saturating_sub(INDENT.len());
    if avail < 8 {
        return Vec::new();
    }
    let inner = avail - 2; // text columns between the │ borders
    let drifted = ann.status == CommentStatus::Drifted;
    let border = Style::default().fg(if drifted {
        pal.drift_border
    } else {
        pal.comment_border
    });
    // A plain comment needs no marker inside its own box — the border already
    // reads as a comment (the gutter still carries the `*`). Drifted comments
    // keep the ⚠ so the drift stays obvious.
    let marker = if drifted {
        format!("{DRIFT_MARKER} ")
    } else {
        String::new()
    };
    let chevron = if collapsed { '▸' } else { '▾' };

    if collapsed {
        // A single capped horizontal rule (not box corners) so a folded comment
        // reads as one deliberate line rather than the top half of a box.
        let preview = ann.comment.lines().next().unwrap_or("");
        let header = hrule(&format!("{chevron} {marker}{preview} "), inner);
        return vec![Line::from(Span::styled(
            format!("{INDENT}╶{header}╴"),
            border,
        ))];
    }

    // Corners follow the user's rounded-borders setting, matching the panes.
    let (tl, tr, bl, br) = if rounded {
        ('╭', '╮', '╰', '╯')
    } else {
        ('┌', '┐', '└', '┘')
    };

    let mut out = Vec::new();
    let header = hrule(&format!("{chevron} {marker}comment "), inner);
    out.push(Line::from(Span::styled(
        format!("{INDENT}{tl}{header}{tr}"),
        border,
    )));
    let text_width = inner.saturating_sub(2);
    for paragraph in ann.comment.split('\n') {
        for chunk in wrap_text(paragraph, text_width) {
            out.push(Line::from(vec![
                Span::styled(format!("{INDENT}│"), border),
                Span::raw(format!(" {} ", clip_pad(&chunk, text_width))),
                Span::styled("│".to_string(), border),
            ]));
        }
    }
    out.push(Line::from(Span::styled(
        format!("{INDENT}{bl}{}{br}", "─".repeat(inner)),
        border,
    )));
    out
}

/// A short label for the line(s) the in-progress comment covers, using the
/// displayed gutter numbers (e.g. `line 5` / `lines 5–8`), for the edit box
/// title. Mirrors the label the old bottom overlay showed.
fn draft_loc_label(state: &DiffReviewState, range: (usize, usize)) -> String {
    match state.resolved_line_range(range) {
        Some((lo, hi)) if lo == hi => format!("line {lo}"),
        Some((lo, hi)) => format!("lines {lo}–{hi}"),
        None => "line ?".to_string(),
    }
}

/// Number of rendered rows [`comment_draft_box_lines`] produces for the same
/// `display`/`width`. Drives the inline layout model so cursor/scroll/click stay
/// in step with the rendered edit box (guarded by a test). `display` is the
/// caret-embedded text from [`super::input_with_caret`].
fn comment_draft_box_height(display: &str, width: usize) -> usize {
    const INDENT_LEN: usize = 2; // "  "
    let avail = width.saturating_sub(INDENT_LEN);
    if avail < 8 {
        return 0;
    }
    let inner = avail - 2;
    let text_width = inner.saturating_sub(2);
    let body = wrap_text(display, text_width).len();
    // Top border + wrapped body lines + bottom border.
    body + 2
}

/// Render the in-progress comment as an inline edit box, anchored where the
/// saved comment will appear. Same geometry as [`comment_box_lines`]'s expanded
/// form (so the layout model can share width math) but with the draft border
/// colour, a `*`-marked title carrying the line range, and a caret at the
/// cursor. `display` is the caret-embedded text from [`super::input_with_caret`].
fn comment_draft_box_lines(
    display: &str,
    loc: &str,
    width: usize,
    pal: &ReviewPalette,
    rounded: bool,
) -> Vec<Line<'static>> {
    const INDENT: &str = "  ";
    let avail = width.saturating_sub(INDENT.len());
    if avail < 8 {
        return Vec::new();
    }
    let inner = avail - 2;
    let border = Style::default().fg(pal.draft_border);
    let (tl, tr, bl, br) = if rounded {
        ('╭', '╮', '╰', '╯')
    } else {
        ('┌', '┐', '└', '┘')
    };

    let mut out = Vec::new();
    let header = hrule(&format!("{COMMENT_MARKER} comment · {loc} "), inner);
    out.push(Line::from(Span::styled(
        format!("{INDENT}{tl}{header}{tr}"),
        border,
    )));
    let text_width = inner.saturating_sub(2);
    for chunk in wrap_text(display, text_width) {
        out.push(Line::from(vec![
            Span::styled(format!("{INDENT}│"), border),
            Span::raw(format!(" {} ", clip_pad(&chunk, text_width))),
            Span::styled("│".to_string(), border),
        ]));
    }
    out.push(Line::from(Span::styled(
        format!("{INDENT}{bl}{}{br}", "─".repeat(inner)),
        border,
    )));
    out
}

/// Build a horizontal-rule string: `head` followed by `─` padding to exactly
/// `width` display columns (truncated if `head` is already too wide).
fn hrule(head: &str, width: usize) -> String {
    let head = format!("─ {head}");
    let len = head.width();
    if len >= width {
        clip(&head, width)
    } else {
        format!("{head}{}", "─".repeat(width - len))
    }
}

/// Truncate `s` to at most `width` display columns, never inside a grapheme.
///
/// `chars().take(n)` is wrong twice over: it counts characters rather than
/// columns, so a CJK line overruns by half its length, and it can cut a
/// combining sequence in two.
fn clip(s: &str, width: usize) -> String {
    let mut used = 0usize;
    let mut out = String::new();
    for g in s.graphemes(true) {
        let w = g.width();
        if used + w > width {
            break;
        }
        used += w;
        out.push_str(g);
    }
    out
}

/// `s` clipped to `width` display columns and right-padded with spaces to
/// exactly that width.
fn clip_pad(s: &str, width: usize) -> String {
    let mut out = clip(s, width);
    let used = out.width();
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    out
}

/// Word-wrap `s` to `width` display columns (falls back to hard cuts via the
/// caller's truncation for over-long words). Always returns at least one line.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if cur.is_empty() {
            cur = word.to_string();
        } else if cur.width() + 1 + word.width() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Apply the theme's selection highlight to every span (background, and
/// foreground when the theme sets one), matching the session list.
///
/// The file-list pane only: inside the diff body, selection is a flag on a
/// [`SpanStyle`] that the palette resolves, so the two never disagree about
/// precedence.
fn select_spans(
    spans: Vec<Span<'static>>,
    pal: &ReviewPalette,
    focused: bool,
) -> Vec<Span<'static>> {
    // Unfocused, the row wears the same selection at 70% — both halves, since a
    // theme may carry its selection mostly in the foreground.
    let (bg, sel_fg) = if focused {
        (pal.selection_bg, pal.selection_fg)
    } else {
        (pal.selection_bg_unfocused, pal.selection_fg_unfocused)
    };
    spans
        .into_iter()
        .map(|s| {
            let mut style = s.style.bg(bg);
            if let Some(fg) = sel_fg {
                style = style.fg(fg);
            }
            Span::styled(s.content, style)
        })
        .collect()
}

/// Precompute every file's render caches: the intra-line word diff, and (on
/// true-color terminals) a warm syntax-highlight cache for every content line.
///
/// This is the heavy per-file work the review body would otherwise do lazily on
/// first navigation — `diffgrid`'s word diff per file plus a fresh `syntect`
/// highlighter per line. It is pure (reads the parsed diff, writes only the
/// process-global highlight cache), so the open-review flow runs it on a
/// blocking worker thread while a spinner shows, making the first view of each
/// file instant. Returns the word-diffed models in `diff.files` order, ready for
/// [`DiffReviewState::prime_views`].
pub(crate) fn precompute_review_caches(
    diff: &ParsedDiff,
    highlight: bool,
) -> Vec<diffgrid::FileDiff<'static>> {
    if highlight {
        // Warm every content line across all files in one parallel pass. The
        // flattened line list spreads a single large file across cores too, not
        // just many files. `ext` borrows from each file's `display_path` (both
        // `&str`), which lives as long as `diff`.
        let lines: Vec<(&str, &str)> = diff
            .files
            .iter()
            .flat_map(|file| {
                let ext = file_extension(file.display_path());
                file.hunks
                    .iter()
                    .flat_map(move |hunk| hunk.lines.iter().map(move |l| (ext, l.content.as_str())))
            })
            .collect();
        warm_highlight_cache(&lines);
    }
    // `par_iter().collect()` preserves order, so the result still lines up with
    // `diff.files` for `prime_views`.
    diff.files.par_iter().map(word_diffed).collect()
}

/// Options for the intra-line word diff.
///
/// `join_gap` is off: absorbing a lone unchanged character between two changed
/// runs is a `diffgrid` refinement this view has never shown, and turning it on
/// here would be a rendering change smuggled in with a refactor. The threshold
/// is the same `0.5` that used to be a constant in this file.
fn intraline_options() -> diffgrid::enrich::IntralineOptions {
    diffgrid::enrich::IntralineOptions::default().with_join_gap(0)
}

/// Convert one wire [`FileDiff`] into `diffgrid`'s model and word-diff it.
///
/// Owned (`'static`) throughout: the wire model is owned `String`s, and a
/// borrowing model would make [`FileView`] self-referential.
fn word_diffed(file: &FileDiff) -> diffgrid::FileDiff<'static> {
    let mut out = to_diffgrid(file);
    diffgrid::enrich::word_diff(&mut out, &intraline_options());
    out
}

/// Map a wire [`FileDiff`] onto `diffgrid`'s model.
///
/// A binary file is marked binary, which drops its hunks: an LFS-tracked image
/// keeps pointer-text hunks on the wire (so the reviewed-mark hash stays
/// content-sensitive) but the body renders the image, never a diff, so the
/// layout for it is deliberately empty.
fn to_diffgrid(file: &FileDiff) -> diffgrid::FileDiff<'static> {
    let status = match file.status {
        FileStatus::Added => diffgrid::FileStatus::Added,
        FileStatus::Deleted => diffgrid::FileStatus::Deleted,
        FileStatus::Modified => diffgrid::FileStatus::Modified,
        FileStatus::Renamed => diffgrid::FileStatus::Renamed,
    };
    let hunks = file
        .hunks
        .iter()
        .map(|h| {
            diffgrid::Hunk::new(
                h.old_start,
                h.old_lines,
                h.new_start,
                h.new_lines,
                h.lines
                    .iter()
                    .map(|l| {
                        diffgrid::DiffLine::new(
                            match l.origin {
                                LineOrigin::Context => diffgrid::LineOrigin::Context,
                                LineOrigin::Addition => diffgrid::LineOrigin::Addition,
                                LineOrigin::Deletion => diffgrid::LineOrigin::Deletion,
                            },
                            l.old_lineno.and_then(LineNo::new),
                            l.new_lineno.and_then(LineNo::new),
                            l.content.clone(),
                        )
                    })
                    .collect(),
            )
            .with_section(h.header.clone())
        })
        .collect();
    let out = diffgrid::FileDiff::new(file.old_path.clone(), file.new_path.clone(), status, hunks);
    match file.binary.is_some() {
        true => out.with_binary(diffgrid::BinaryInfo::absent()),
        false => out,
    }
}

/// A stable digest of the blocks that should be attached to a layout, so a
/// re-splice only happens when the comment set or the draft anchor moves.
fn attachment_signature(attachments: &[(SelIdx, AttachKey)]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    for (sel, key) in attachments {
        h.update(&(sel.get() as u64).to_le_bytes());
        h.update(&key.get().to_le_bytes());
    }
    h.digest()
}

/// Review payload prepared off the render thread: the parsed diff plus its warmed
/// render caches, ready to construct a [`DiffReviewState`]. Built by the
/// open-review background task (see `handle_open_review`) and consumed by
/// `handle_state_update` once the loading spinner can be replaced with the view.
#[derive(Debug, Clone)]
pub struct ReviewPrepared {
    pub(super) session_id: SessionId,
    pub(super) title: String,
    pub(super) base: String,
    pub(super) diff: ParsedDiff,
    pub(super) comments: Vec<Comment>,
    pub(super) reviewed: Vec<String>,
    /// Every file's word-diffed `diffgrid` model, in `diff.files` order. Empty
    /// when the precompute is turned off, in which case each file's model is
    /// built on first navigation to it.
    pub(super) models: Vec<diffgrid::FileDiff<'static>>,
    pub(super) content_hash: u64,
    /// Comments the refresh discarded because their file left the diff, so the
    /// view can say so rather than having them vanish silently.
    pub(super) dropped_comments: Vec<Comment>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_commander_core::git::parse_unified_diff;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn review_btn(x: u16, w: u16, code: KeyCode) -> ReviewButton {
        ReviewButton {
            rect: Rect {
                x,
                y: 3,
                width: w,
                height: 1,
            },
            key: KeyEvent::new(code, KeyModifiers::NONE),
        }
    }

    #[test]
    fn review_button_at_maps_click_to_key() {
        let buttons = vec![
            review_btn(0, 6, KeyCode::Char('v')),
            review_btn(9, 8, KeyCode::Char('a')),
        ];
        assert_eq!(
            review_button_at(&buttons, 2, 3).map(|k| k.code),
            Some(KeyCode::Char('v'))
        );
        assert_eq!(
            review_button_at(&buttons, 10, 3).map(|k| k.code),
            Some(KeyCode::Char('a'))
        );
    }

    #[test]
    fn review_button_at_misses_are_none() {
        let buttons = vec![review_btn(0, 6, KeyCode::Char('v'))];
        assert!(review_button_at(&buttons, 6, 3).is_none()); // just past width
        assert!(review_button_at(&buttons, 2, 4).is_none()); // wrong row
        assert!(review_button_at(&[], 0, 3).is_none());
    }

    fn state_with_two_files() -> DiffReviewState {
        let diff = parse_unified_diff(
            "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,2 +1,3 @@
 fn main() {
+    let y = 3;
 }
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1 +1 @@
-b
+B
",
        );
        DiffReviewState::new(
            SessionId::new(),
            "test".to_string(),
            "main".to_string(),
            diff,
            Vec::new(),
        )
    }

    /// A single file with two hunks separated by a gap, and starting below
    /// line 1, so there is a leading gap, a middle gap, and (once the file's
    /// length is known) a trailing gap to expand.
    fn state_with_gap_diff() -> DiffReviewState {
        let diff = parse_unified_diff(
            "\
diff --git a/f.rs b/f.rs
--- a/f.rs
+++ b/f.rs
@@ -5,3 +5,3 @@
 a
-b
+B
 c
@@ -20,3 +20,3 @@
 d
-e
+E
 f
",
        );
        DiffReviewState::new(
            SessionId::new(),
            "test".to_string(),
            "main".to_string(),
            diff,
            Vec::new(),
        )
    }

    /// Load `n` synthetic working-tree lines ("L1".."Ln") for `f.rs`.
    fn load_gap_file_lines(s: &mut DiffReviewState, n: usize) {
        let lines: Vec<String> = (1..=n).map(|i| format!("L{i}")).collect();
        s.set_file_lines("f.rs".to_string(), std::sync::Arc::new(lines));
    }

    /// Render the body at a given pane size, as the renderer does.
    fn render_body(s: &DiffReviewState, width: usize, height: usize) -> Vec<Line<'static>> {
        s.body.set((width, height));
        let pal = Theme::truecolor().review_palette();
        review_body_lines(s, false, &pal, "rs", None, true)
    }

    /// The first body row showing gap `gap`'s expand control, if it has one.
    fn control_row(s: &DiffReviewState, gap: usize) -> Option<usize> {
        s.with_grid(|_, grid| {
            (0..grid.row_count()).find(|y| {
                matches!(grid.hit(0, RowIdx::new(*y)),
                    Hit::ExpandControl { gap: g, .. } if g.get() == gap)
            })
        })
        .flatten()
    }

    /// Expand controls are `diffgrid`'s, but *whether it has the file* is this
    /// view's: content arrives asynchronously, and until it does the layout must
    /// offer no affordance it cannot honour.
    #[test]
    fn expand_controls_appear_only_once_content_arrives() {
        let mut s = state_with_gap_diff();
        // The diff shape says a fetch is worth making...
        assert!(s.current_file_expandable());
        // ...but until it lands there is nothing to reveal, so no affordance is
        // drawn that could not be honoured.
        assert_eq!(control_row(&s, 1), None);

        load_gap_file_lines(&mut s, 30);
        assert!(
            control_row(&s, 1).is_some(),
            "the middle gap should offer a control once the file is loaded"
        );
    }

    /// Revealed context is display-only: it must never shift the selectable-line
    /// indices the cursor and every comment anchor are expressed in.
    #[test]
    fn expansion_does_not_change_selectable_count() {
        let mut s = state_with_gap_diff();
        let before = s.selectable_count();
        load_gap_file_lines(&mut s, 30);
        s.expand_gap(0, ExpandAction::All);
        s.expand_gap(1, ExpandAction::All);
        s.expand_gap(2, ExpandAction::All);
        assert_eq!(s.selectable_count(), before);
        // ...and the body really did grow, or the assertion above proves nothing.
        assert!(s.total_body_rows() > before);
    }

    /// A layout mode is baked into a `FileLayout`, so switching rebuilds it. The
    /// view replays the expansions it recorded, because otherwise pressing `t`
    /// would silently collapse everything the user had revealed.
    #[test]
    fn expansion_survives_a_layout_toggle_and_a_file_round_trip() {
        let mut s = state_with_gap_diff();
        load_gap_file_lines(&mut s, 30);
        s.expand_gap(1, ExpandAction::All);
        let revealed = s.total_body_rows();

        s.toggle_layout();
        s.toggle_layout();
        assert_eq!(
            s.total_body_rows(),
            revealed,
            "toggling layout lost context"
        );
        assert_eq!(
            control_row(&s, 1),
            None,
            "the gap should still be fully open"
        );
    }

    /// Content arrives after the layout was first built, and the expansion asked
    /// for before it landed has to take effect when it does — otherwise the
    /// first click on a cold file looks broken.
    #[test]
    fn an_expand_requested_before_load_lands_when_content_arrives() {
        let mut s = state_with_gap_diff();
        let collapsed = s.total_body_rows();
        s.expand_gap(1, ExpandAction::All);
        assert_eq!(s.total_body_rows(), collapsed, "nothing to reveal yet");

        load_gap_file_lines(&mut s, 30);
        assert!(s.total_body_rows() > collapsed);
        assert_eq!(control_row(&s, 1), None);
    }

    #[test]
    fn split_file_lines_handles_trailing_newline_and_empty() {
        assert_eq!(&**split_file_lines(b"a\nb\n"), &["a", "b"]);
        assert_eq!(&**split_file_lines(b"a\nb"), &["a", "b"]);
        assert!(split_file_lines(b"").is_empty());
        // A lone newline is one empty line, not two.
        assert_eq!(&**split_file_lines(b"\n"), &[""]);
        // A blank line in the middle is preserved.
        assert_eq!(&**split_file_lines(b"a\n\nc\n"), &["a", "", "c"]);
    }

    /// A click on an expand control reveals context, and reports that it did so
    /// (the caller uses that to kick the content fetch).
    #[test]
    fn clicking_an_expand_control_reveals_context() {
        let mut s = state_with_gap_diff();
        load_gap_file_lines(&mut s, 30);
        s.body.set((80, 20));
        let before = s.total_body_rows();
        let row = control_row(&s, 1).expect("middle gap has a control");
        let body = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 40,
        };
        assert!(s.click_at(0, row as u16, body), "click should expand");
        assert!(s.total_body_rows() > before);
    }

    /// The one invariant the whole view rests on: the number of lines the
    /// renderer emits equals the number of rows every cursor, scroll and click
    /// mapping is computed against. Checked in both layouts, with a comment box
    /// and revealed context interleaved, at several widths.
    #[test]
    fn rendered_rows_match_the_grid_in_both_layouts() {
        for layout in [ReviewLayout::Inline, ReviewLayout::SideBySide] {
            let mut s = state_with_gap_diff();
            load_gap_file_lines(&mut s, 30);
            s.layout = layout;
            s.expand_gap(1, ExpandAction::Down);
            s.comments
                .push(Comment::new("f.rs", CommentSide::New, (6, 6), "B", "note"));
            for width in [40, 80, 120] {
                let lines = render_body(&s, width, 20);
                assert_eq!(
                    lines.len(),
                    s.total_body_rows(),
                    "{layout:?} at width {width}"
                );
                // Every rendered line fills the pane, or a row's tint stops
                // short of the edge. Side by side may leave the odd column
                // over on a data row, since two equal halves and the rule
                // between them need not sum to the pane's width.
                let floor = match layout {
                    ReviewLayout::Inline => width,
                    ReviewLayout::SideBySide => s.wrap_options().half_width() * 2 + SBS_SEPARATOR,
                };
                for (i, line) in lines.iter().enumerate() {
                    let w = line.width();
                    assert!(
                        (floor..=width).contains(&w),
                        "{layout:?} row {i} at width {width} is {w} wide"
                    );
                }
            }
        }
    }

    /// A hunk header is one physical row however long its section heading is —
    /// `Grid` counts it as one — so the renderer must truncate it rather than
    /// soft-wrap it, or every mapping below it lands one row out.
    #[test]
    fn a_long_hunk_header_stays_one_row() {
        let diff = parse_unified_diff(
            "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1 +1 @@ fn a_very_long_enclosing_function_name_that_will_not_fit_in_a_narrow_pane()
-a
+b
",
        );
        let s = DiffReviewState::new(
            SessionId::new(),
            "test".to_string(),
            "main".to_string(),
            diff,
            Vec::new(),
        );
        // header + deletion + addition, at a width the header cannot fit in.
        assert_eq!(s.total_body_rows(), 3);
        assert_eq!(render_body(&s, 30, 20).len(), 3);
    }

    /// Cursor-follow uses the pane's real height. With a fixed viewport constant
    /// this was wrong on every terminal that was not about 22 rows tall — the
    /// cursor would scroll off the bottom of a tall pane, or the view would jump
    /// on a short one.
    #[test]
    fn cursor_follow_uses_the_real_viewport_height() {
        let mut s = state_with_gap_diff();
        load_gap_file_lines(&mut s, 30);
        s.expand_gap(1, ExpandAction::All);
        s.focus = ReviewFocus::Body;
        let last = s.selectable_count() - 1;

        // A tall pane holds the whole file, so following the cursor to the last
        // line need not scroll at all.
        s.body.set((80, 60));
        s.cursor = last;
        s.follow_cursor();
        assert_eq!(s.scroll, 0, "a pane taller than the file should not scroll");

        // A short one must scroll until the cursor's row is the bottom row.
        s.body.set((80, 5));
        s.scroll = 0;
        s.follow_cursor();
        let row = s
            .with_grid(|_, grid| grid.row_of(SelIdx::new(last)).map(|r| r.get()))
            .flatten()
            .expect("cursor line has a row");
        assert_eq!(s.scroll as usize, row + 1 - 5);
    }

    #[test]
    fn review_key_feature_maps_tracked_actions() {
        use crossterm::event::KeyCode;
        // The UI-only toggles each record a feature regardless of focus.
        // `o` is intentionally absent: it's gated on file type in the handler
        // (see can_toggle_image_side) so no-op presses aren't counted.
        for (code, feature) in [
            (KeyCode::Char('t'), "review.toggle_layout"),
            (KeyCode::Char('z'), "review.toggle_fold"),
            (KeyCode::Char('r'), "review.refresh"),
        ] {
            assert_eq!(
                review_key_feature(code, ReviewFocus::FileList),
                Some(feature)
            );
            assert_eq!(review_key_feature(code, ReviewFocus::Body), Some(feature));
        }
        // `v` only enters visual selection (and records) in the body.
        assert_eq!(
            review_key_feature(KeyCode::Char('v'), ReviewFocus::Body),
            Some("review.visual_select")
        );
        assert_eq!(
            review_key_feature(KeyCode::Char('v'), ReviewFocus::FileList),
            None
        );
        // Navigation / scroll / file-movement keys are noise — never recorded.
        // `o` is here too: it's recorded inline (gated), not via this mapping.
        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('['),
            KeyCode::Char(']'),
            KeyCode::Tab,
            KeyCode::Enter,
            KeyCode::Char('m'),
            KeyCode::Char('a'),
            KeyCode::Char('d'),
            KeyCode::Char('o'),
        ] {
            assert_eq!(
                review_key_feature(code, ReviewFocus::Body),
                None,
                "{code:?}"
            );
        }
    }

    #[test]
    fn can_toggle_image_side_gates_on_modified_image() {
        use claude_commander_core::git::{BinaryInfo, BinaryKind};

        let mut state = state_with_two_files();
        // a.rs is a modified *text* file — the image toggle does nothing.
        assert!(!state.can_toggle_image_side());

        // Make the current file a binary image.
        let file = &mut state.diff.files[state.selected_file];
        file.binary = Some(BinaryInfo {
            kind: BinaryKind::Image {
                mime: "image/png".to_string(),
            },
            old_oid: None,
            new_oid: None,
            old_size: None,
            new_size: None,
        });

        // A modified binary image has two sides: the toggle is meaningful.
        file.status = FileStatus::Modified;
        assert!(state.can_toggle_image_side());

        // An added image shows only its one side — no-op, not counted.
        state.diff.files[state.selected_file].status = FileStatus::Added;
        assert!(!state.can_toggle_image_side());
    }

    #[test]
    fn ctrl_n_p_alias_arrow_keys_for_navigation() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        // Ctrl-n / Ctrl-p fold onto Down / Up so they navigate like the
        // arrow keys (and j/k) regardless of focus.
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(review_nav_keycode(ctrl('n')), KeyCode::Down);
        assert_eq!(review_nav_keycode(ctrl('p')), KeyCode::Up);
        // Without Ctrl, and for unrelated keys, the keycode passes through.
        let plain = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert_eq!(review_nav_keycode(plain('n')), KeyCode::Char('n'));
        assert_eq!(review_nav_keycode(plain('j')), KeyCode::Char('j'));
        assert_eq!(
            review_nav_keycode(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            KeyCode::Esc
        );
    }

    #[test]
    fn next_prev_file_clamps_and_resets() {
        let mut s = state_with_two_files();
        s.cursor = 1;
        s.prev_file();
        assert_eq!(s.selected_file, 0);
        s.next_file();
        assert_eq!(s.selected_file, 1);
        assert_eq!(s.cursor, 0, "cursor resets on file change");
        s.next_file();
        assert_eq!(s.selected_file, 1, "clamps at last file");
    }

    #[test]
    fn move_cursor_clamps() {
        let mut s = state_with_two_files();
        s.focus = ReviewFocus::Body;
        // a.rs has 3 selectable lines (context, addition, context).
        assert_eq!(s.selectable_count(), 3);
        for _ in 0..10 {
            s.move_cursor(true);
        }
        assert_eq!(s.cursor, 2);
        s.move_cursor(false);
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn refresh_diff_swaps_content_and_keeps_file_by_path() {
        let mut s = state_with_two_files();
        // Viewing b.rs (the second file).
        s.selected_file = 1;
        s.focus = ReviewFocus::Body;
        assert_eq!(s.current_file().unwrap().display_path(), "b.rs");

        // Simulate the agent editing files after comments were applied: a fresh
        // diff where b.rs gained a line and the file order changed.
        let new_diff = parse_unified_diff(
            "\
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1 +1,2 @@
-b
+B
+extra
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,2 +1,3 @@
 fn main() {
+    let y = 3;
 }
",
        );
        s.refresh_diff(new_diff, Vec::new(), HashSet::new(), Vec::new(), 42);

        assert_eq!(s.content_hash, 42);
        // The body still shows b.rs even though it moved to index 0...
        assert_eq!(s.selected_file, 0);
        assert_eq!(s.current_file().unwrap().display_path(), "b.rs");
        // ...and it reflects the *new* content (the stale snapshot lacked it).
        let body: Vec<_> = s
            .selectable_lines()
            .iter()
            .map(|l| l.content.clone())
            .collect();
        assert!(
            body.iter().any(|c| c == "extra"),
            "fresh diff shown: {body:?}"
        );
    }

    #[test]
    fn refresh_diff_clamps_cursor_into_shorter_file() {
        let mut s = state_with_two_files();
        s.selected_file = 0; // a.rs has 3 selectable lines
        s.focus = ReviewFocus::Body;
        s.cursor = 2;
        // a.rs shrinks to a single changed line in the refreshed diff.
        let new_diff = parse_unified_diff(
            "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-fn main() {}
+fn main() { }
",
        );
        s.refresh_diff(new_diff, Vec::new(), HashSet::new(), Vec::new(), 7);
        assert_eq!(s.current_file().unwrap().display_path(), "a.rs");
        assert_eq!(s.selectable_count(), 2);
        assert!(
            s.cursor < s.selectable_count(),
            "cursor clamped: {}",
            s.cursor
        );
    }

    #[test]
    fn refresh_diff_falls_back_when_file_removed() {
        let mut s = state_with_two_files();
        s.selected_file = 1; // b.rs
        // Refreshed diff no longer contains b.rs.
        let new_diff = parse_unified_diff(
            "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,2 +1,3 @@
 fn main() {
+    let y = 3;
 }
",
        );
        s.refresh_diff(new_diff, Vec::new(), HashSet::new(), Vec::new(), 1);
        assert_eq!(s.diff.files.len(), 1);
        assert_eq!(s.selected_file, 0);
        assert_eq!(s.current_file().unwrap().display_path(), "a.rs");
    }

    #[test]
    fn visual_selection_range_normalises() {
        let mut s = state_with_two_files();
        s.cursor = 2;
        s.toggle_visual(); // anchor at 2
        s.cursor = 0; // active end above anchor
        assert_eq!(s.selection(), (0, 2));
        s.toggle_visual(); // cancel
        assert_eq!(s.selection(), (0, 0));
    }

    #[test]
    fn build_draft_picks_new_side_and_captures_snippet() {
        let mut s = state_with_two_files();
        // Select the inserted line (selectable index 1: "+    let y = 3;").
        s.cursor = 1;
        let draft = s.build_draft((1, 1), "extract helper".to_string()).unwrap();
        assert_eq!(draft.file, "a.rs");
        assert_eq!(draft.side, CommentSide::New);
        assert_eq!(draft.line_range, (2, 2));
        assert_eq!(draft.snippet, "    let y = 3;");
        assert_eq!(draft.comment, "extract helper");
    }

    #[test]
    fn resolved_line_range_uses_gutter_number_not_selectable_index() {
        // Two deletions precede the addition, so the addition's selectable
        // index (2) is well ahead of its new-side gutter number (1). The
        // comment-box title must show the gutter number.
        let diff = parse_unified_diff(
            "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1,3 +1,2 @@
-old a
-old b
+new
 tail
",
        );
        let s = DiffReviewState::new(
            SessionId::new(),
            "t".to_string(),
            "main".to_string(),
            diff,
            Vec::new(),
        );
        // selectable index 2 is the addition; its new-side line number is 1.
        assert_eq!(s.resolved_line_range((2, 2)), Some((1, 1)));
        // And build_draft anchors to the same number, so title and storage agree.
        assert_eq!(
            s.build_draft((2, 2), "x".into()).unwrap().line_range,
            (1, 1)
        );
    }

    #[test]
    fn build_draft_pure_deletion_uses_old_side() {
        let mut s = state_with_two_files();
        s.selected_file = 1; // b.rs: -b / +B
        // selectable index 0 is the deletion "-b".
        let draft = s.build_draft((0, 0), "why?".to_string()).unwrap();
        assert_eq!(draft.side, CommentSide::Old);
        assert_eq!(draft.snippet, "b");
    }

    #[test]
    fn comment_at_cursor_matches_covering_range() {
        let mut s = state_with_two_files();
        s.comments.push(Comment::new(
            "a.rs",
            CommentSide::New,
            (2, 2),
            "    let y = 3;",
            "note",
        ));
        s.cursor = 1; // the inserted line (new lineno 2)
        assert!(s.comment_at_cursor().is_some());
        s.cursor = 0; // context line "fn main() {" (new lineno 1) — not covered
        assert!(s.comment_at_cursor().is_none());
    }

    #[test]
    fn toggle_focus_flips() {
        let mut s = state_with_two_files();
        assert_eq!(s.focus, ReviewFocus::FileList);
        s.toggle_focus();
        assert_eq!(s.focus, ReviewFocus::Body);
    }

    // --- reviewed marks ---

    fn state_with_three_files() -> DiffReviewState {
        let diff = parse_unified_diff(
            "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-a
+A
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1 +1 @@
-b
+B
diff --git a/c.rs b/c.rs
--- a/c.rs
+++ b/c.rs
@@ -1 +1 @@
-c
+C
",
        );
        DiffReviewState::new(
            SessionId::new(),
            "test".to_string(),
            "main".to_string(),
            diff,
            Vec::new(),
        )
    }

    #[test]
    fn opens_on_first_unreviewed_file() {
        let mut s = state_with_three_files();
        s.reviewed = ["a.rs".to_string()].into_iter().collect();
        s.select_first_unreviewed();
        assert_eq!(s.selected_file, 1, "skips reviewed a.rs, lands on b.rs");
    }

    #[test]
    fn opens_on_first_file_when_all_reviewed() {
        let mut s = state_with_three_files();
        s.reviewed = ["a.rs", "b.rs", "c.rs"]
            .into_iter()
            .map(String::from)
            .collect();
        s.select_first_unreviewed();
        assert_eq!(s.selected_file, 0, "all reviewed falls back to first file");
    }

    /// The store's mark for `file`, as another frontend would have written it.
    fn stored_mark(file: &FileDiff) -> ReviewedFileMark {
        ReviewedFileMark {
            file: file.display_path().to_string(),
            hash: review::hash_hex(file),
        }
    }

    #[test]
    fn polled_marks_fold_in_only_for_the_content_on_screen() {
        let mut s = state_with_three_files();
        let mut stale_b = stored_mark(&s.diff.files[1]);
        stale_b.hash = "0".into();
        let marks = vec![stored_mark(&s.diff.files[0]), stale_b];

        s.fold_reviewed_marks(s.reviewed_gen, &marks);
        assert!(s.is_reviewed_path("a.rs"));
        assert!(
            !s.is_reviewed_path("b.rs"),
            "a mark pinned to other content than the view shows stays hidden"
        );

        s.fold_reviewed_marks(s.reviewed_gen, &[]);
        assert!(s.reviewed.is_empty(), "an external unmark folds in too");
    }

    /// `m` pins its mark to the working tree, which can be newer than a stale
    /// view's diff. The poll then sees a mark for other content than shown —
    /// and must not undo the keypress.
    #[test]
    fn a_mark_for_newer_content_does_not_clear_what_the_view_shows() {
        let mut s = state_with_three_files();
        s.set_reviewed("a.rs".to_string(), true);
        let mut newer_a = stored_mark(&s.diff.files[0]);
        newer_a.hash = "0".into();

        s.fold_reviewed_marks(s.reviewed_gen, &[newer_a]);
        assert!(s.is_reviewed_path("a.rs"));
    }

    #[test]
    fn a_poll_that_raced_a_local_toggle_is_dropped() {
        let mut s = state_with_three_files();
        let spawned_at = s.reviewed_gen;
        // The user marks c.rs while a poll (which read the store before the
        // write) is in flight.
        s.set_reviewed("c.rs".to_string(), true);
        s.fold_reviewed_marks(spawned_at, &[]);
        assert!(s.is_reviewed_path("c.rs"), "the local toggle survives");
    }

    #[test]
    fn mark_advances_to_next_unreviewed_file() {
        let mut s = state_with_two_files();
        s.set_reviewed("a.rs".to_string(), true);
        s.advance_to_next_unreviewed();
        assert!(s.is_reviewed_path("a.rs"));
        assert_eq!(s.selected_file, 1);
    }

    #[test]
    fn advance_wraps_past_end_to_first_unreviewed() {
        let mut s = state_with_three_files();
        s.set_reviewed("b.rs".to_string(), true);
        s.set_reviewed("c.rs".to_string(), true);
        s.selected_file = 1;
        s.advance_to_next_unreviewed();
        assert_eq!(s.selected_file, 0, "wraps past reviewed c.rs to a.rs");
    }

    #[test]
    fn advance_stays_put_when_all_reviewed() {
        let mut s = state_with_two_files();
        s.set_reviewed("a.rs".to_string(), true);
        s.set_reviewed("b.rs".to_string(), true);
        s.advance_to_next_unreviewed();
        assert_eq!(s.selected_file, 0);
    }

    #[test]
    fn unmark_does_not_advance() {
        let mut s = state_with_two_files();
        s.set_reviewed("a.rs".to_string(), true);
        s.set_reviewed("a.rs".to_string(), false);
        assert!(!s.is_reviewed_path("a.rs"));
        assert_eq!(s.selected_file, 0, "unmarking never moves the selection");
    }

    // --- file tree ---

    fn file(path: &str) -> FileDiff {
        FileDiff {
            old_path: path.to_string(),
            new_path: path.to_string(),
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
            hunks: Vec::new(),
            binary: None,
        }
    }

    #[test]
    fn tree_compresses_single_child_dir_chains() {
        let files = vec![
            file("common/src/redux/middleware/a.ts"),
            file("common/src/redux/middleware/b.ts"),
            file("notes/src/app/Runner.ts"),
        ];
        let tree = build_file_tree(&files);
        assert_eq!(tree.len(), 2);
        // Single-child dir chain with two file leaves collapses to one node.
        assert_eq!(tree[0].name, "common/src/redux/middleware");
        assert_eq!(tree[0].children.len(), 2);
        // Chain stops at the directory holding the single file leaf.
        assert_eq!(tree[1].name, "notes/src/app");
        assert_eq!(tree[1].children.len(), 1);
        assert_eq!(tree[1].children[0].name, "Runner.ts");
        assert_eq!(tree[1].children[0].file_index, Some(2));
    }

    #[test]
    fn flatten_respects_collapse() {
        let tree = build_file_tree(&[file("dir/a.ts"), file("dir/b.ts")]);
        let mut collapsed = HashSet::new();
        let mut rows = Vec::new();
        flatten_tree(&tree, 0, &collapsed, &mut rows);
        assert_eq!(rows.len(), 3); // dir + two files

        collapsed.insert("dir".to_string());
        rows.clear();
        flatten_tree(&tree, 0, &collapsed, &mut rows);
        assert_eq!(rows.len(), 1); // collapsed dir hides its files
    }

    #[test]
    fn tree_move_updates_body_and_activate_collapses() {
        let diff = ParsedDiff {
            files: vec![file("dir/a.ts"), file("dir/b.ts")],
        };
        let mut s = DiffReviewState::new(
            SessionId::new(),
            "t".to_string(),
            "main".to_string(),
            diff,
            Vec::new(),
        );
        // rows: Dir(dir) @0, File a @1, File b @2. Body starts on first file.
        assert_eq!(s.selected_file, 0);
        s.tree_move(true); // onto file a
        s.tree_move(true); // onto file b
        assert_eq!(s.selected_file, 1);
        // Back to the dir row and collapse it.
        s.tree_move(false);
        s.tree_move(false);
        assert_eq!(s.tree_cursor, 0);
        s.tree_activate();
        assert_eq!(s.visible_rows().len(), 1);
        // Expanding restores the files.
        s.tree_activate();
        assert_eq!(s.visible_rows().len(), 3);
    }

    fn dir_state() -> DiffReviewState {
        DiffReviewState::new(
            SessionId::new(),
            "t".to_string(),
            "main".to_string(),
            ParsedDiff {
                files: vec![file("dir/a.ts"), file("dir/b.ts"), file("other.ts")],
            },
            Vec::new(),
        )
    }

    fn dir_collapsed(s: &DiffReviewState) -> bool {
        s.visible_rows().iter().any(
            |r| matches!(r, TreeRow::Dir { path, collapsed, .. } if path == "dir" && *collapsed),
        )
    }

    /// Marks set by the `review` CLI finish a directory just as `m` does.
    #[test]
    fn polled_marks_that_finish_a_directory_collapse_it() {
        let mut s = dir_state();
        s.set_reviewed("dir/a.ts".to_string(), true);
        let marks = [stored_mark(&s.diff.files[0]), stored_mark(&s.diff.files[1])];

        s.fold_reviewed_marks(s.reviewed_gen, &marks);
        assert!(dir_collapsed(&s));
    }

    /// Marks set while the view was closed (or between refreshes) leave
    /// finished directories folded when it opens or refreshes.
    #[test]
    fn refresh_collapses_directories_reviewed_while_away() {
        let mut s = dir_state();
        assert!(!dir_collapsed(&s));
        let diff = s.diff.clone();
        let reviewed = ["dir/a.ts", "dir/b.ts"].map(String::from).into();

        s.refresh_diff(diff, Vec::new(), reviewed, Vec::new(), 1);
        assert!(dir_collapsed(&s));
    }

    /// A directory that was already finished stays as the user left it: an
    /// unrelated edit refreshing the view mustn't shut one opened to re-read.
    #[test]
    fn refresh_leaves_an_already_finished_directory_the_user_expanded() {
        let mut s = dir_state();
        s.reviewed = ["dir/a.ts", "dir/b.ts"].map(String::from).into();
        assert!(!dir_collapsed(&s), "precondition: the user has it expanded");
        let diff = s.diff.clone();
        let reviewed = s.reviewed.clone();

        s.refresh_diff(diff, Vec::new(), reviewed, Vec::new(), 2);
        assert!(!dir_collapsed(&s));
    }

    /// A poll collapsing a directory above the cursor must leave the tree
    /// highlight on the file the body shows, not on whatever row slid under it.
    #[test]
    fn a_polled_collapse_keeps_the_tree_cursor_on_the_shown_file() {
        let mut s = dir_state();
        let other = s
            .diff
            .files
            .iter()
            .position(|f| f.display_path() == "other.ts")
            .unwrap();
        s.set_body_file(other);
        s.sync_tree_cursor_to_file();
        let marks = [stored_mark(&s.diff.files[0]), stored_mark(&s.diff.files[1])];

        s.fold_reviewed_marks(s.reviewed_gen, &marks);

        assert!(dir_collapsed(&s));
        assert!(matches!(
            s.visible_rows()[s.tree_cursor],
            TreeRow::File { index, .. } if index == other
        ));
    }

    /// Generations are unique across views, so a poll spawned for a closed
    /// view can't pass as current for the one reopened in its place.
    #[test]
    fn a_reopened_view_does_not_accept_a_poll_from_the_old_one() {
        let old = dir_state();
        let mut reopened = dir_state();
        reopened.reviewed.insert("other.ts".to_string());

        reopened.fold_reviewed_marks(old.reviewed_gen, &[]);
        assert!(reopened.is_reviewed_path("other.ts"));
    }

    #[test]
    fn marking_last_file_in_dir_auto_collapses_it() {
        let diff = ParsedDiff {
            files: vec![file("dir/a.ts"), file("dir/b.ts"), file("other.ts")],
        };
        let mut s = DiffReviewState::new(
            SessionId::new(),
            "t".to_string(),
            "main".to_string(),
            diff,
            Vec::new(),
        );
        let file_rows = |s: &DiffReviewState| {
            s.visible_rows()
                .iter()
                .filter(|r| matches!(r, TreeRow::File { .. }))
                .count()
        };

        // One of two files in `dir` reviewed: incomplete, stays expanded.
        s.set_reviewed("dir/a.ts".to_string(), true);
        s.collapse_completed_dirs(Some("dir/a.ts"));
        assert_eq!(
            file_rows(&s),
            3,
            "dir stays open while it has unreviewed files"
        );

        // Last file in `dir` reviewed: the directory folds away automatically.
        s.set_reviewed("dir/b.ts".to_string(), true);
        s.collapse_completed_dirs(Some("dir/b.ts"));
        let rows = s.visible_rows();
        assert!(
            rows.iter().any(
                |r| matches!(r, TreeRow::Dir { path, collapsed, .. } if path == "dir" && *collapsed)
            ),
            "fully-reviewed dir is collapsed"
        );
        // Its two files are hidden; the unrelated root-level file remains.
        assert_eq!(file_rows(&s), 1, "only the root-level file row is left");
    }

    #[test]
    fn auto_collapse_reanchors_scroll_when_rows_vanish() {
        // Enough files under one dir to push the file pane past its viewport.
        let files: Vec<FileDiff> = (0..30).map(|i| file(&format!("dir/f{i:02}.ts"))).collect();
        let mut s = DiffReviewState::new(
            SessionId::new(),
            "t".to_string(),
            "main".to_string(),
            ParsedDiff { files },
            Vec::new(),
        );
        // All files reviewed, cursor/scroll parked deep in the list.
        for i in 0..30 {
            s.set_reviewed(format!("dir/f{i:02}.ts"), true);
        }
        s.tree_cursor = 30;
        s.tree_scroll = 20;

        // Marking the last file folds the whole dir down to a single row.
        s.collapse_completed_dirs(Some("dir/f29.ts"));
        assert_eq!(
            s.visible_rows().len(),
            1,
            "the collapsed dir leaves one row"
        );
        // Scroll must follow, or the pane renders blank past the new end.
        assert_eq!(s.tree_scroll, 0, "scroll re-anchors onto the surviving row");
        assert_eq!(s.tree_cursor, 0, "cursor clamps into range");
    }

    #[test]
    fn auto_collapse_folds_nested_completed_dirs() {
        let diff = ParsedDiff {
            files: vec![file("a/b/x.ts")],
        };
        let mut s = DiffReviewState::new(
            SessionId::new(),
            "t".to_string(),
            "main".to_string(),
            diff,
            Vec::new(),
        );
        // The single-child chain compresses to one "a/b" node holding x.ts.
        s.set_reviewed("a/b/x.ts".to_string(), true);
        s.collapse_completed_dirs(Some("a/b/x.ts"));
        let rows = s.visible_rows();
        assert!(
            rows.iter().any(
                |r| matches!(r, TreeRow::Dir { path, collapsed, .. } if path == "a/b" && *collapsed)
            ),
            "the compressed ancestor dir collapses once its only file is reviewed"
        );
        assert!(
            !rows.iter().any(|r| matches!(r, TreeRow::File { .. })),
            "the reviewed file is hidden under the collapsed dir"
        );
    }

    #[test]
    fn selectable_at_body_row_skips_header() {
        let s = state_with_two_files();
        // a.rs body rows: 0 header, 1..=3 the three diff lines.
        assert_eq!(s.selectable_at_body_row(0), None);
        assert_eq!(s.selectable_at_body_row(1), Some(0));
        assert_eq!(s.selectable_at_body_row(3), Some(2));
        assert_eq!(s.selectable_at_body_row(4), None);
    }

    #[test]
    fn click_and_drag_select_a_range() {
        let mut s = state_with_two_files();
        let body = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        // Click the first diff line (body row 1).
        s.click_at(5, 1, body);
        assert_eq!(s.focus, ReviewFocus::Body);
        assert_eq!(s.cursor, 0);
        assert!(s.visual_anchor.is_none());
        // Drag down to body row 3 (third diff line) → selection 0..=2.
        s.drag_at(5, 3, body);
        assert_eq!(s.selection(), (0, 2));
        // Clicking outside the body rect leaves the cursor untouched.
        s.click_at(5, 50, body);
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn click_file_list_selects_file() {
        let mut s = state_with_two_files();
        // Start focused on the body looking at the first file.
        s.focus = ReviewFocus::Body;
        assert_eq!(s.selected_file, 0);
        let rect = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 20,
        };
        // visible tree rows: [a.rs (row 0), b.rs (row 1)]. Click b.rs.
        s.click_file_list_at(5, 1, rect);
        assert_eq!(s.focus, ReviewFocus::FileList);
        assert_eq!(s.tree_cursor, 1);
        assert_eq!(s.selected_file, 1);
        // A click below the last row is a no-op (no panic, file unchanged).
        s.click_file_list_at(5, 10, rect);
        assert_eq!(s.selected_file, 1);
    }

    #[test]
    fn right_click_selects_line_then_opens_comment() {
        let mut s = state_with_two_files();
        let body = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        // No selection yet; cursor at 0. Right-click body row 2 (selectable
        // index 1, the inserted line) should move the cursor there first.
        assert!(s.right_click_comment(5, 2, body));
        assert_eq!(s.cursor, 1);
        let draft = s.comment.as_ref().unwrap();
        assert_eq!(draft.range, (1, 1));
    }

    #[test]
    fn double_click_selects_line_then_opens_comment() {
        let mut s = state_with_two_files();
        let body = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        // Double-click body row 2 (selectable index 1): selects just that line
        // and opens its comment box, like a right-click on a fresh line.
        assert!(s.double_click_comment(5, 2, body));
        assert_eq!(s.cursor, 1);
        assert!(s.visual_anchor.is_none());
        let draft = s.comment.as_ref().unwrap();
        assert_eq!(draft.range, (1, 1));
    }

    #[test]
    fn double_click_on_header_row_is_no_op() {
        let mut s = state_with_two_files();
        let body = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        // Body row 0 is the hunk header — not a selectable diff line.
        assert!(!s.double_click_comment(5, 0, body));
        assert!(s.comment.is_none());
    }

    #[test]
    fn right_click_keeps_active_selection() {
        let mut s = state_with_two_files();
        let body = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        // Drag-select rows 1..=3 (selectable 0..=2), then right-click row 2.
        s.click_at(5, 1, body);
        s.drag_at(5, 3, body);
        assert_eq!(s.selection(), (0, 2));
        assert!(s.right_click_comment(5, 2, body));
        // The multi-line selection is preserved, not collapsed to the click.
        let draft = s.comment.as_ref().unwrap();
        assert_eq!(draft.range, (0, 2));
    }

    #[test]
    fn comment_badge_span_is_comment_coloured_and_hidden_when_zero() {
        let pal = Theme::truecolor().review_palette();
        assert!(comment_badge_span(0, &pal).is_none());
        let span = comment_badge_span(3, &pal).expect("badge for non-zero count");
        assert_eq!(span.content.as_ref(), format!(" {COMMENT_MARKER}3"));
        assert_eq!(span.style.fg, Some(pal.comment_border));
    }

    #[test]
    fn reviewed_check_span_hidden_when_unreviewed() {
        let pal = Theme::truecolor().review_palette();
        assert!(reviewed_check_span(false, &pal).is_none());
        let span = reviewed_check_span(true, &pal).expect("check for reviewed file");
        assert_eq!(span.content.as_ref(), " ✓");
        assert_eq!(span.style.fg, Some(pal.add_fg));
    }

    #[test]
    fn reviewed_bg_unreviewed_row_unchanged() {
        let pal = Theme::truecolor().review_palette();
        let spans = vec![Span::raw("a.rs")];
        let out = apply_reviewed_bg(spans, false, 20, &pal);
        // Unreviewed rows keep the default background and no padding span.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].style.bg, None);
    }

    #[test]
    fn reviewed_bg_bands_and_pads_to_width() {
        let pal = Theme::truecolor().review_palette();
        let spans = vec![Span::raw("a.rs")]; // 4 columns
        let out = apply_reviewed_bg(spans, true, 20, &pal);
        // Every span carries the reviewed background...
        assert!(out.iter().all(|s| s.style.bg == Some(pal.reviewed_bg)));
        // ...and the band is padded out to fill the full pane width.
        let total: usize = out.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(total, 20);
    }

    #[test]
    fn reviewed_bg_no_pad_when_content_exceeds_width() {
        let pal = Theme::truecolor().review_palette();
        let spans = vec![Span::raw("a-really-long-file-name.rs")]; // 26 columns
        let out = apply_reviewed_bg(spans, true, 10, &pal);
        // No padding span is added when the content already overflows the width.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].style.bg, Some(pal.reviewed_bg));
    }

    #[test]
    fn dir_comment_count_aggregates_subtree_pending_comments() {
        let mut s = state_with_two_files();
        s.comments.push(Comment::new(
            "src/git/diff.rs",
            CommentSide::New,
            (1, 1),
            "x",
            "note",
        ));
        s.comments.push(Comment::new(
            "src/git/backend.rs",
            CommentSide::New,
            (1, 1),
            "y",
            "note",
        ));
        // Both nested comments roll up to their ancestor directories.
        assert_eq!(s.dir_comment_count("src"), 2);
        assert_eq!(s.dir_comment_count("src/git"), 2);
        // A sibling directory with no comments stays clean, and the prefix
        // match respects the `/` boundary (no "src" → "srcfoo" leakage).
        assert_eq!(s.dir_comment_count("other"), 0);
        assert_eq!(s.dir_comment_count("src/gi"), 0);
        // Applied comments don't count.
        s.comments[0].status = CommentStatus::Applied;
        assert_eq!(s.dir_comment_count("src/git"), 1);
    }

    /// The Apply-blocked message names the offending files so a blocker is
    /// findable — including the residual case the orphan drop can't resolve,
    /// where the file has no row in the tree at all.
    #[test]
    fn drift_block_message_names_files_by_file_count_not_comment_count() {
        let mut s = state_with_two_files();
        let mut push = |file: &str| {
            let c = Comment::new(file, CommentSide::New, (1, 1), "x", "note");
            let id = c.id;
            s.comments.push(c);
            id
        };
        let a1 = push("src/git/diff.rs");
        let a2 = push("src/git/diff.rs");
        let b = push("src/git/backend.rs");
        let c = push("other/third.rs");

        assert_eq!(
            s.drift_block_message(&[a1]),
            "1 drifted comment(s) in diff.rs block apply — review or delete them"
        );
        // Two comments in ONE file names one file, not two.
        assert_eq!(
            s.drift_block_message(&[a1, a2]),
            "2 drifted comment(s) in diff.rs block apply — review or delete them"
        );
        // Three comments across exactly two files must NOT gain an ellipsis:
        // it would point at a file that doesn't exist.
        assert_eq!(
            s.drift_block_message(&[a1, a2, b]),
            "3 drifted comment(s) in diff.rs, backend.rs block apply — review or delete them"
        );
        // Three distinct files do elide.
        assert_eq!(
            s.drift_block_message(&[a1, b, c]),
            "3 drifted comment(s) in diff.rs, backend.rs, … block apply — review or delete them"
        );
        // An id with no matching comment degrades to the bare count.
        assert_eq!(
            s.drift_block_message(&[uuid::Uuid::new_v4()]),
            "1 drifted comment(s) block apply — review or delete them"
        );
    }

    #[test]
    fn comment_anchors_map_to_end_line_selidx() {
        let mut s = state_with_two_files();
        // a.rs: ctx(new1,sel0), +let y=3(new2,sel1), ctx(new3,sel2).
        s.comments.push(Comment::new(
            "a.rs",
            CommentSide::New,
            (2, 2),
            "let y = 3;",
            "note",
        ));
        // The box attaches below selectable line 1, not line 0.
        assert_eq!(
            s.wanted_attachments(),
            vec![(SelIdx::new(1), comment_attach_key(s.comments[0].id))]
        );
    }

    #[test]
    fn orphaned_drifted_comment_pins_to_last_line_and_stays_reachable() {
        // Regression: a drifted comment whose anchor line no longer exists in
        // the diff (the code drifted away) used to render nowhere — invisible
        // and impossible to delete, yet still counted as pending and blocking
        // Apply. It must pin to the file's last selectable line so it stays
        // visible, gutter-marked, and selectable for deletion.
        let mut s = state_with_two_files();
        // a.rs has 3 selectable lines (indices 0..=2); new line 99 is gone.
        let mut ann = Comment::new("a.rs", CommentSide::New, (99, 99), "vanished", "note");
        ann.status = CommentStatus::Drifted;
        s.comments.push(ann);
        s.focus = ReviewFocus::Body;

        // The box anchors to the last selectable line rather than being dropped.
        assert_eq!(
            s.wanted_attachments(),
            vec![(SelIdx::new(2), comment_attach_key(s.comments[0].id))]
        );

        // The last line carries a drift-flagged gutter marker...
        assert_eq!(s.comment_markers().get(&2), Some(&true));

        // ...and the cursor there resolves to the comment so `d` can delete it.
        s.cursor = 2;
        assert!(s.comment_at_cursor().is_some());
    }

    #[test]
    fn comment_box_collapsed_single_line_expanded_boxed() {
        let ann = Comment::new(
            "a.rs",
            CommentSide::New,
            (2, 2),
            "let y = 3;",
            "extract helper\nand rename",
        );
        let pal = Theme::truecolor().review_palette();
        assert_eq!(comment_box_lines(&ann, true, 60, &pal, true).len(), 1);
        // top border + two comment paragraphs + bottom border.
        assert_eq!(comment_box_lines(&ann, false, 60, &pal, true).len(), 4);
    }

    #[test]
    fn comment_box_corners_follow_rounded_setting() {
        let pal = Theme::truecolor().review_palette();
        let ann = Comment::new("a.rs", CommentSide::New, (2, 2), "let y = 3;", "note");
        let corners = |rounded: bool| -> String {
            let lines = comment_box_lines(&ann, false, 60, &pal, rounded);
            let top: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
            let bot: String = lines
                .last()
                .unwrap()
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            format!("{top}{bot}")
        };
        let round = corners(true);
        assert!(round.contains('╭') && round.contains('╮'));
        assert!(round.contains('╰') && round.contains('╯'));
        let square = corners(false);
        assert!(square.contains('┌') && square.contains('┐'));
        assert!(square.contains('└') && square.contains('┘'));
        assert!(!square.contains('╭') && !square.contains('╯'));
    }

    #[test]
    fn comment_box_header_drops_asterisk_keeps_drift_marker() {
        let pal = Theme::truecolor().review_palette();
        let text_of = |lines: &[Line]| -> String {
            lines[0].spans.iter().map(|s| s.content.as_ref()).collect()
        };

        // A staged comment's box header has no asterisk (the gutter keeps it).
        let mut ann = Comment::new("a.rs", CommentSide::New, (2, 2), "let y = 3;", "note");
        let expanded = text_of(&comment_box_lines(&ann, false, 60, &pal, true));
        let collapsed = text_of(&comment_box_lines(&ann, true, 60, &pal, true));
        assert!(expanded.contains("comment"));
        assert!(!expanded.contains(COMMENT_MARKER));
        assert!(!collapsed.contains(COMMENT_MARKER));

        // A drifted comment still surfaces the ⚠ in its box header.
        ann.status = CommentStatus::Drifted;
        let drifted = text_of(&comment_box_lines(&ann, false, 60, &pal, true));
        assert!(drifted.contains(DRIFT_MARKER));
    }

    #[test]
    fn toggle_comment_fold_flips_state() {
        let mut s = state_with_two_files();
        let ann = Comment::new("a.rs", CommentSide::New, (2, 2), "let y = 3;", "note");
        let id = ann.id;
        s.comments.push(ann);
        s.focus = ReviewFocus::Body;
        s.cursor = 1; // the inserted line, covered by the comment
        assert!(!s.is_comment_collapsed(id));
        s.toggle_comment_fold();
        assert!(s.is_comment_collapsed(id));
        s.toggle_comment_fold();
        assert!(!s.is_comment_collapsed(id));
    }

    #[test]
    fn comment_box_height_matches_rendered() {
        let pal = Theme::truecolor().review_palette();
        for comment in ["short", "one\ntwo\nthree", &"word ".repeat(40)] {
            let ann = Comment::new("a.rs", CommentSide::New, (2, 2), "snip", comment);
            for width in [12usize, 40, 80, 6 /* below the 8-col floor */] {
                for collapsed in [true, false] {
                    assert_eq!(
                        comment_box_height(&ann, collapsed, width),
                        comment_box_lines(&ann, collapsed, width, &pal, true).len(),
                        "height mismatch (collapsed={collapsed}, width={width})"
                    );
                }
            }
        }
    }

    /// A comment box is drawn with its own borders, so its interior has to be
    /// measured in display columns. A CJK or emoji comment counts fewer
    /// characters than it occupies, and padding by that count pushed the closing
    /// `│` past the right edge of the box.
    #[test]
    fn comment_box_borders_line_up_for_wide_characters() {
        let pal = Theme::truecolor().review_palette();
        for text in ["日本語のコメントです", "emoji 🎉 comment", "plain ascii"] {
            let ann = Comment::new("a.rs", CommentSide::New, (2, 2), "snip", text);
            for width in [24usize, 40, 80] {
                let lines = comment_box_lines(&ann, false, width, &pal, true);
                let widths: Vec<usize> = lines.iter().map(Line::width).collect();
                assert!(
                    widths.iter().all(|w| *w == widths[0]),
                    "box edges ragged for {text:?} at width {width}: {widths:?}"
                );
            }
        }
    }

    #[test]
    fn comment_draft_box_height_matches_rendered() {
        let pal = Theme::truecolor().review_palette();
        // Cover end-of-text and mid-text caret placement (the caret is spliced
        // into `display` before either function wraps it).
        for display in [
            "▏",
            "short▏",
            "mid▏dle",
            &format!("{}▏", "word ".repeat(40)),
        ] {
            for width in [12usize, 40, 80, 6 /* below the 8-col floor */] {
                assert_eq!(
                    comment_draft_box_height(display, width),
                    comment_draft_box_lines(display, "line 1", width, &pal, true).len(),
                    "height mismatch (width={width}, display={display:?})"
                );
            }
        }
    }

    #[test]
    fn draft_box_renders_inline_at_its_anchor() {
        // Opening a comment box must add rows to the body, attached below the
        // selected line — and the renderer and the grid must still agree.
        let mut s = state_with_two_files();
        s.body.set((80, 20));
        s.focus = ReviewFocus::Body;
        s.cursor = 1; // the inserted line
        let before = s.total_body_rows();
        assert!(s.begin_comment());
        assert!(s.total_body_rows() > before, "draft box should occupy rows");

        // The draft's rows sit immediately after selectable line 1's row.
        let (line_row, draft_row) = s
            .with_grid(|view, grid| {
                let line = grid.row_of(SelIdx::new(1))?.get();
                let block = view
                    .layout
                    .rows()
                    .find(|r| r.attach_key() == Some(DRAFT_ATTACH_KEY))?;
                Some((line, grid.row_of_logical(block.index())?.get()))
            })
            .flatten()
            .expect("both the line and its draft box have rows");
        assert_eq!(draft_row, line_row + 1);

        // Renderer row count matches the grid (cursor/scroll/click).
        assert_eq!(render_body(&s, 80, 20).len(), s.total_body_rows());
    }

    #[test]
    fn click_on_comment_box_toggles_fold() {
        let mut s = state_with_two_files();
        let ann = Comment::new("a.rs", CommentSide::New, (2, 2), "let y = 3;", "note");
        let id = ann.id;
        s.comments.push(ann);
        let body = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        // Rows: 0 header, 1 ctx, 2 +addition, 3.. comment box. The box is
        // anchored to selectable index 1 (the addition), so it renders after
        // body row 2.
        assert!(!s.is_comment_collapsed(id));
        s.click_at(5, 3, body);
        assert!(
            s.is_comment_collapsed(id),
            "clicking the box should fold it"
        );
        // Collapsed, the box is a single row still at body row 3 — click again
        // to unfold.
        s.click_at(5, 3, body);
        assert!(
            !s.is_comment_collapsed(id),
            "clicking again should unfold it"
        );
    }

    #[test]
    fn click_on_diff_line_does_not_toggle_comment() {
        let mut s = state_with_two_files();
        let ann = Comment::new("a.rs", CommentSide::New, (2, 2), "let y = 3;", "note");
        let id = ann.id;
        s.comments.push(ann);
        let body = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        // Clicking the addition line (body row 2) moves the cursor and leaves
        // the comment expanded.
        s.click_at(5, 2, body);
        assert_eq!(s.cursor, 1);
        assert!(!s.is_comment_collapsed(id));
    }

    #[test]
    fn click_toggles_comment_box_in_side_by_side() {
        let mut s = state_with_two_files();
        s.layout = ReviewLayout::SideBySide;
        let ann = Comment::new("a.rs", CommentSide::New, (2, 2), "let y = 3;", "note");
        let id = ann.id;
        s.comments.push(ann);
        let body = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        // side-by-side rows: 0 header, 1 ctx(1,1), 2 (gap|+addition), then the
        // box anchored to the addition (selectable index 1) at body row 3.
        s.click_at(5, 3, body);
        assert!(s.is_comment_collapsed(id));
    }

    #[test]
    fn file_extension_handles_paths_and_dotfiles() {
        assert_eq!(file_extension("src/git/diff.rs"), "rs");
        assert_eq!(file_extension("Cargo.toml"), "toml");
        assert_eq!(file_extension("README.md"), "md");
        assert_eq!(file_extension("Makefile"), "");
        assert_eq!(file_extension("dir.with.dot/Justfile"), "");
    }

    /// The word diff itself is `diffgrid`'s and tested there. What is this
    /// view's is the *options* it runs with: gap joining stays off, so a lone
    /// surviving character between two rewritten runs is not swept into the
    /// emphasis, which is what this view has always rendered.
    #[test]
    fn word_diff_options_keep_gap_joining_off() {
        let opts = intraline_options();
        assert_eq!(opts.join_gap, 0);
        let (old, _) = diffgrid::enrich::word_diff_pair("a, b", "x, y", &opts)
            .expect("a similar-enough pair is marked up");
        // The `,` between the two changed runs stays unemphasised.
        assert!(
            old.iter()
                .any(|seg| !seg.emphasis && seg.text.contains(',')),
            "gap joining must not absorb the separator: {old:?}"
        );
    }

    /// The wire model and `diffgrid`'s must agree about what is on each line, or
    /// a comment anchored through one lands on the wrong line in the other.
    #[test]
    fn diffgrid_model_mirrors_the_wire_model() {
        let s = state_with_two_files();
        let file = &s.diff.files[0];
        let model = word_diffed(file);
        assert_eq!(model.display_path(), file.display_path());
        assert_eq!(model.selectable_count(), file.hunks[0].lines.len());
        for (i, line) in file.hunks[0].lines.iter().enumerate() {
            let got = model.line(SelIdx::new(i)).expect("line present");
            assert_eq!(got.content, line.content, "line {i}");
            assert_eq!(got.old_lineno.map(LineNo::get), line.old_lineno);
            assert_eq!(got.new_lineno.map(LineNo::get), line.new_lineno);
        }
    }

    /// A binary file's model carries no hunks, so the layout for it is empty —
    /// the body renders the image instead — while the wire model keeps the
    /// git-LFS pointer text its reviewed-mark hash is computed over.
    #[test]
    fn a_binary_file_lays_out_as_nothing() {
        let diff = parse_unified_diff(
            "\
diff --git a/logo.png b/logo.png
index 1111111..2222222 100644
Binary files a/logo.png and b/logo.png differ
",
        );
        let model = word_diffed(&diff.files[0]);
        assert!(model.is_binary());
        assert!(!model.is_expandable());
        assert_eq!(model.selectable_count(), 0);
    }

    #[test]
    fn precompute_matches_the_lazy_path() {
        let s = state_with_two_files();
        // Highlighting on exercises the cache-warming branch too (the returned
        // models must be identical regardless).
        let pre = precompute_review_caches(&s.diff, true);
        assert_eq!(pre.len(), s.diff.files.len());
        for (i, file) in s.diff.files.iter().enumerate() {
            assert_eq!(pre[i], word_diffed(file), "file {i} differs");
        }
    }

    /// Priming installs the precomputed models rather than recomputing them.
    #[test]
    fn primed_models_are_used_without_recompute() {
        let mut s = state_with_two_files();
        // A sentinel no real computation would produce: if the view recomputed,
        // it would not lay this out.
        let sentinel: Vec<diffgrid::FileDiff<'static>> = (0..s.diff.files.len())
            .map(|i| {
                diffgrid::FileDiff::new(
                    s.diff.files[i].display_path().to_string(),
                    s.diff.files[i].display_path().to_string(),
                    diffgrid::FileStatus::Modified,
                    Vec::new(),
                )
            })
            .collect();
        s.prime_views(sentinel);
        for i in 0..s.diff.files.len() {
            s.selected_file = i;
            assert_eq!(
                s.with_view(|v| v.file.selectable_count()),
                Some(0),
                "file {i} was recomputed instead of using the primed model"
            );
        }
    }

    #[test]
    fn an_empty_diff_lays_out_without_panicking() {
        let s = DiffReviewState::new(
            SessionId::new(),
            "test".to_string(),
            "HEAD".to_string(),
            parse_unified_diff(""),
            Vec::new(),
        );
        assert_eq!(s.total_body_rows(), 0);
        assert!(render_body(&s, 80, 20).is_empty());
    }

    fn state_with_long_line() -> DiffReviewState {
        // One short context line and one long addition (16 content columns).
        let diff = parse_unified_diff(
            "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1 +1,2 @@
 ctx
+0123456789ABCDEF
",
        );
        DiffReviewState::new(
            SessionId::new(),
            "test".to_string(),
            "main".to_string(),
            diff,
            Vec::new(),
        )
    }

    /// Soft wrapping is `diffgrid`'s, but the *width* it wraps into is this
    /// view's: a gutter spec, a right margin and a pane width. Getting that
    /// arithmetic wrong drifts every row mapping once a line wraps.
    #[test]
    fn a_wrapped_line_keeps_its_row_mappings_consistent() {
        let s = state_with_long_line();
        // Body width chosen so the content (wrap) width is exactly 8 columns,
        // making the 16-column addition take two rows.
        let width = inline_gutter().cols() + WRAP_RIGHT_MARGIN + 8;
        s.body.set((width, 20));
        assert_eq!(s.wrap_options().content_width(inline_gutter().cols()), 8);

        // header + ctx (1 row) + the long addition (2 rows).
        assert_eq!(s.total_body_rows(), 4);
        assert_eq!(render_body(&s, width, 20).len(), 4);
        // The cursor lands on the line's *first* row...
        assert_eq!(
            s.with_grid(|_, g| g.row_of(SelIdx::new(1)).map(|r| r.get()))
                .flatten(),
            Some(2)
        );
        // ...and a click anywhere on it — continuation row included — selects
        // that diff line, while the header row selects nothing.
        assert_eq!(s.selectable_at_body_row(2), Some(1));
        assert_eq!(s.selectable_at_body_row(3), Some(1));
        assert_eq!(s.selectable_at_body_row(0), None);
    }

    /// The two gutters are the view's own geometry, and every row's content
    /// column is measured from them.
    #[test]
    fn gutters_are_the_widths_the_body_is_laid_out_against() {
        // Edge, marker, two four-column numbers each followed by a space, the
        // sign, and one column of padding.
        assert_eq!(inline_gutter().cols(), 14);
        // One four-column number and a space; the halves say which side they are.
        assert_eq!(sbs_gutter().cols(), 5);
    }

    #[test]
    fn begin_comment_opens_box_for_selection() {
        let mut s = state_with_two_files();
        s.focus = ReviewFocus::Body;
        s.cursor = 2;
        s.toggle_visual(); // anchor at 2
        s.cursor = 0;
        assert!(s.begin_comment());
        let draft = s.comment.as_ref().unwrap();
        assert_eq!(draft.range, (0, 2));
        // A second call while a comment is open is a no-op.
        assert!(!s.begin_comment());
    }

    #[test]
    fn paste_into_draft_appends_text() {
        // Regression: pasting into the review comment box was silently
        // dropped because InputEvent::Paste had no ReviewDiff arm.
        let mut s = state_with_two_files();
        s.focus = ReviewFocus::Body;
        s.begin_comment();
        s.paste_into_draft("see ");
        assert!(s.paste_into_draft("the docs"));
        assert_eq!(s.comment.as_ref().unwrap().input.value(), "see the docs");
    }

    #[test]
    fn paste_into_draft_keeps_newlines_strips_carriage_returns() {
        // Bracketed paste delivers newlines as text, not Enter key events,
        // so a multi-line paste can keep its line breaks (compose_markdown
        // passes them through to the agent verbatim). CRs from CRLF
        // clipboards are dropped.
        let mut s = state_with_two_files();
        s.focus = ReviewFocus::Body;
        s.begin_comment();
        assert!(s.paste_into_draft("let x = 1;\r\nlet y = 2;\r\n"));
        assert_eq!(
            s.comment.as_ref().unwrap().input.value(),
            "let x = 1;\nlet y = 2;\n"
        );
    }

    #[test]
    fn paste_into_draft_without_open_box_is_noop() {
        let mut s = state_with_two_files();
        assert!(!s.paste_into_draft("ignored"));
        assert!(s.comment.is_none());
    }

    #[test]
    fn toggle_layout_flips() {
        let mut s = state_with_two_files();
        assert_eq!(s.layout, ReviewLayout::Inline);
        s.toggle_layout();
        assert_eq!(s.layout, ReviewLayout::SideBySide);
    }

    #[test]
    fn toggle_image_side_flips() {
        let mut s = state_with_two_files();
        assert_eq!(s.image_side, DiffSide::New);
        s.toggle_image_side();
        assert_eq!(s.image_side, DiffSide::Old);
        s.toggle_image_side();
        assert_eq!(s.image_side, DiffSide::New);
    }

    #[test]
    fn shown_image_side_forces_single_sided_statuses() {
        let mut f = file("img.png");
        // Added: always the new side, ignoring the preference.
        f.status = FileStatus::Added;
        assert_eq!(shown_image_side(&f, DiffSide::Old), DiffSide::New);
        // Deleted: always the old side.
        f.status = FileStatus::Deleted;
        assert_eq!(shown_image_side(&f, DiffSide::New), DiffSide::Old);
        // Modified: honours the preference.
        f.status = FileStatus::Modified;
        assert_eq!(shown_image_side(&f, DiffSide::Old), DiffSide::Old);
        assert_eq!(shown_image_side(&f, DiffSide::New), DiffSide::New);
    }

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(None), "? bytes");
        assert_eq!(human_size(Some(512)), "512 bytes");
        assert_eq!(human_size(Some(2048)), "2.0 KiB");
        assert_eq!(human_size(Some(3 * 1024 * 1024)), "3.0 MiB");
    }

    #[test]
    fn image_caption_shows_toggle_hint_only_for_modified() {
        // A modification has two sides, so the `o` toggle is meaningful.
        let modified = image_caption(FileStatus::Modified, "after", Some(2048));
        assert!(
            modified.contains("press o to toggle"),
            "modified caption should advertise the toggle: {modified:?}"
        );
        assert!(modified.contains("after") && modified.contains("2.0 KiB"));

        // Added/deleted images are single-sided; `o` is a no-op, so the hint
        // must not appear (it would be misleading UI).
        let added = image_caption(FileStatus::Added, "after", Some(2048));
        assert!(
            !added.contains("press o to toggle"),
            "added caption must not advertise a no-op toggle: {added:?}"
        );
        let deleted = image_caption(FileStatus::Deleted, "before", Some(512));
        assert!(
            !deleted.contains("press o to toggle"),
            "deleted caption must not advertise a no-op toggle: {deleted:?}"
        );
    }

    #[test]
    fn page_body_scrolls_a_page_and_clamps() {
        let mut s = state_with_two_files();
        // a.rs body has 4 rows (1 header + 3 lines); a 2-row pane can show the
        // last of them at the bottom from scroll 2.
        s.body.set((80, 2));
        // Focused on the file list, paging still scrolls the diff body.
        assert_eq!(s.focus, ReviewFocus::FileList);
        s.page_body(true);
        assert_eq!(s.scroll, 2);
        s.page_body(true);
        assert_eq!(s.scroll, 2, "paging past the end must not open a void");
        s.page_body(false);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn wheel_scrolls_within_bounds() {
        let mut s = state_with_two_files();
        // a.rs total body rows = 1 header + 3 lines = 4, pane height 2 → the
        // last row lands on the bottom row at scroll 2.
        s.body.set((80, 2));
        for _ in 0..10 {
            s.wheel(true);
        }
        assert_eq!(s.scroll, 2);
        s.wheel(false);
        assert_eq!(s.scroll, 1);
    }

    #[test]
    fn scroll_stops_with_the_last_row_on_the_bottom_row() {
        let mut s = state_with_two_files();
        // A pane taller than the file has nothing to scroll at all.
        s.body.set((80, 20));
        for _ in 0..10 {
            s.wheel(true);
        }
        assert_eq!(s.scroll, 0, "a pane taller than the file must not scroll");
        s.page_body(true);
        assert_eq!(s.scroll, 0);

        // And a scroll left over from a shorter pane is pulled back the next
        // time the body is scrolled, rather than leaving the void on screen.
        s.body.set((80, 2));
        s.wheel(true);
        s.wheel(true);
        s.wheel(true);
        assert_eq!(s.scroll, 2);
        s.body.set((80, 20));
        s.wheel(true);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn wheel_tree_scrolls_file_list_within_bounds() {
        let mut s = state_with_two_files();
        // Two root files → 2 visible rows, and a 1-row pane → max tree_scroll 1.
        s.body.set((80, 1));
        assert_eq!(s.tree_scroll, 0);
        for _ in 0..10 {
            s.wheel_tree(true);
        }
        assert_eq!(s.tree_scroll, 1);
        // The wheel scrolls the file list without touching the diff body.
        assert_eq!(s.scroll, 0);
        s.wheel_tree(false);
        assert_eq!(s.tree_scroll, 0);
        s.wheel_tree(false);
        assert_eq!(s.tree_scroll, 0);

        // A pane tall enough for every file never scrolls the list.
        s.body.set((80, 20));
        s.wheel_tree(true);
        assert_eq!(s.tree_scroll, 0);
    }
}
