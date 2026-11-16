import 'package:flutter/material.dart';

import '../src/rust/api/diff.dart';
import '../src/rust/api/review.dart' show ReviewLineOrigin;
import '../theme/diff_theme.dart';
import '../theme/tokens.dart';

/// Signature for staging a comment on a selected line range.
typedef AddCommentFn =
    Future<void> Function({
      required String file,
      required String side,
      required int lineStart,
      required int lineEnd,
      required String snippet,
    });

/// Renders a diff laid out by the cdylib: one widget per [DiffRowDto], with
/// line selection, the comment affordance, and the gap-expansion controls.
///
/// Everything structural — which rows exist, how the two sides pair up, which
/// runs the word diff marked as changed, what a gap still hides — was decided
/// by `diffgrid` before this widget ran. What is left here is genuinely
/// presentation: fills, gutters, hit targets and buttons.
///
/// Selection is tracked in `diffgrid`'s **selectable-line index** rather than a
/// row or hunk position, because that index survives everything this view can
/// do to the presentation: switching to side by side, expanding a gap, or
/// re-laying-out after a refresh.
class DiffView extends StatefulWidget {
  const DiffView({
    super.key,
    required this.file,
    required this.layout,
    required this.sideBySide,
    required this.onAddComment,
    required this.onExpand,
    this.dualGutter = false,
    this.sliver = false,
  });

  /// Display path of the file, for the comment the selection produces.
  final String file;

  final DiffLayoutDto layout;

  /// Two columns rather than one. Set by the caller from the available width;
  /// the layout itself must have been built in the matching mode.
  final bool sideBySide;

  /// Stage a comment for the selected range; `null` disables selection.
  final AddCommentFn? onAddComment;

  /// Reveal more of a gap. The caller appends to its expansion list and rebuilds
  /// the layout.
  final void Function(DiffExpansion) onExpand;

  /// Show old *and* new line numbers in the single-column gutter. Off by
  /// default: on a phone the second number costs more than it tells you.
  final bool dualGutter;
  final bool sliver;

  @override
  State<DiffView> createState() => _DiffViewState();
}

class _DiffViewState extends State<DiffView> {
  /// The selected range, in selectable-line indices.
  int? _anchor;
  int? _focus;

  bool _inSelection(int? sel) {
    if (sel == null || _anchor == null || _focus == null) return false;
    final lo = _anchor! < _focus! ? _anchor! : _focus!;
    final hi = _anchor! > _focus! ? _anchor! : _focus!;
    return sel >= lo && sel <= hi;
  }

  void _tap(int sel) => setState(() {
    _anchor = sel;
    _focus = sel;
  });

  void _extend(int sel) => setState(() {
    _anchor ??= sel;
    _focus = sel;
  });

  void _clear() => setState(() {
    _anchor = null;
    _focus = null;
  });

  @override
  void didUpdateWidget(covariant DiffView old) {
    super.didUpdateWidget(old);
    // A new layout may have fewer selectable lines (a refresh that shrank the
    // diff), so a selection carried over could point past the end. Expanding a
    // gap only *adds* rows and leaves the index space alone, which is exactly
    // why selection is tracked in that space — so clamp rather than clear, and
    // only when the file itself changed.
    if (old.file != widget.file) {
      _anchor = null;
      _focus = null;
    } else if (widget.layout.selectable != old.layout.selectable) {
      final last = widget.layout.selectable - 1;
      if (_anchor != null && _anchor! > last) _anchor = null;
      if (_focus != null && _focus! > last) _focus = null;
      if (_anchor == null || _focus == null) {
        _anchor = null;
        _focus = null;
      }
    }
  }

  /// Every selectable cell in the layout, keyed by its selectable-line index —
  /// the lookup the comment builder needs and the rows already carry.
  Map<int, DiffCellDto> get _cells {
    final out = <int, DiffCellDto>{};
    for (final row in widget.layout.rows) {
      for (final cell in [row.left, row.right]) {
        final sel = cell.sel;
        if (sel != null) out[sel] = cell;
      }
    }
    return out;
  }

  /// Resolve the selection into a comment and hand it to the host.
  ///
  /// The side is taken from the selected lines: an all-deletion selection
  /// anchors on the old side, anything else on the new. Both the line bounds and
  /// the snippet come from the lines that exist on the chosen side — a mixed
  /// selection whose snippet carried the other side's text could not be
  /// re-anchored by the server, so the comment would drift the moment it was
  /// created.
  Future<void> _comment() async {
    final cb = widget.onAddComment;
    if (cb == null || _anchor == null || _focus == null) return;
    final lo = _anchor! < _focus! ? _anchor! : _focus!;
    final hi = _anchor! > _focus! ? _anchor! : _focus!;
    final cells = _cells;
    final selected = [
      for (var i = lo; i <= hi; i++)
        if (cells[i] != null) cells[i]!,
    ];
    if (selected.isEmpty) return _clear();

    final allDeletions = selected.every(
      (c) => c.origin == ReviewLineOrigin.deletion,
    );
    final side = allDeletions ? 'old' : 'new';
    final sideLines = selected
        .where((c) => (allDeletions ? c.oldLineno : c.newLineno) != null)
        .toList();
    if (sideLines.isEmpty) return _clear();

    final numbers =
        sideLines
            .map((c) => (allDeletions ? c.oldLineno : c.newLineno)!)
            .toList()
          ..sort();
    await cb(
      file: widget.file,
      side: side,
      lineStart: numbers.first,
      lineEnd: numbers.last,
      snippet: sideLines.map((c) => c.text).join('\n'),
    );
    // The re-open after staging disposes this subtree mid-await.
    if (!mounted) return;
    _clear();
  }

  @override
  Widget build(BuildContext context) {
    final t = CommanderTokens.of(context);
    final colors = DiffTheme.of(context);
    final selectable = widget.onAddComment != null;
    final hi = (_anchor != null && _focus != null)
        ? (_anchor! > _focus! ? _anchor! : _focus!)
        : null;

    final actionAfter = hi == null
        ? null
        : widget.layout.rows.indexWhere(
            (row) => row.left.sel == hi || row.right.sel == hi,
          );
    final hasActions = actionAfter != null && actionAfter >= 0;
    Widget rowAt(int index) {
      if (hasActions && index == actionAfter + 1) return _actions();
      final source = hasActions && index > actionAfter + 1 ? index - 1 : index;
      return _row(t, colors, widget.layout.rows[source], selectable);
    }

    final count = widget.layout.rows.length + (hasActions ? 1 : 0);
    if (widget.sliver) {
      return SliverList(
        delegate: SliverChildBuilderDelegate(
          (context, index) => rowAt(index),
          childCount: count,
        ),
      );
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [for (var i = 0; i < count; i++) rowAt(i)],
    );
  }

  Widget _actions() => Padding(
    padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
    child: Row(
      children: [
        TextButton.icon(
          onPressed: _comment,
          icon: const Icon(Icons.add_comment, size: 16),
          label: const Text('Comment on selection'),
        ),
        TextButton(onPressed: _clear, child: const Text('Clear')),
      ],
    ),
  );

  Widget _row(
    CommanderTokens t,
    DiffColors colors,
    DiffRowDto row,
    bool selectable,
  ) => switch (row.kind) {
    DiffRowKind.hunkHeader => _hunkHeader(colors, row),
    DiffRowKind.expandControl => _expandControl(t, colors, row),
    DiffRowKind.alignmentGap => Container(
      height: 18,
      color: colors.alignmentGapFill,
    ),
    // `IntrinsicHeight` so the two halves — and each half's gutter — share
    // one height. Without it the shorter side's fill stops early and a
    // soft-wrapped line leaves a notch in the gutter band. It costs an extra
    // layout pass per visible row; the sliver list limits that work to the viewport.
    DiffRowKind.line || DiffRowKind.expandedContext => IntrinsicHeight(
      child: widget.sideBySide && !row.fullWidth
          ? Row(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Expanded(child: _half(t, colors, row, row.left, selectable)),
                VerticalDivider(width: 1, color: t.divider),
                Expanded(child: _half(t, colors, row, row.right, selectable)),
              ],
            )
          : _half(t, colors, row, row.left, selectable),
    ),
  };

  Widget _hunkHeader(DiffColors colors, DiffRowDto row) => Container(
    width: double.infinity,
    color: colors.hunkHeaderFill,
    padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 4),
    child: Text.rich(
      TextSpan(
        children: [
          for (final s in row.left.spans)
            TextSpan(text: s.text, style: colors.spanStyle(s, size: 11)),
        ],
      ),
    ),
  );

  /// A gap's "reveal more" affordance. `diffgrid` also knows how to lay this out
  /// as centred glyphs for a terminal; here the counts drive real buttons.
  Widget _expandControl(CommanderTokens t, DiffColors colors, DiffRowDto row) {
    final gap = row.gap;
    if (gap == null) return const SizedBox.shrink();
    void expand(DiffExpandAction action) =>
        widget.onExpand(DiffExpansion(gap: gap, action: action));
    return Container(
      color: colors.expandedFill,
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
      child: Row(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          if (row.canExpandDown)
            _expandButton(
              t,
              Icons.keyboard_arrow_down,
              'Expand down',
              () => expand(DiffExpandAction.down),
            ),
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 10),
            child: Text(
              '${row.hidden} hidden line${row.hidden == 1 ? '' : 's'}',
              style: t.meta(color: colors.expandedFg),
            ),
          ),
          if (row.canExpandUp)
            _expandButton(
              t,
              Icons.keyboard_arrow_up,
              'Expand up',
              () => expand(DiffExpandAction.up),
            ),
          _expandButton(
            t,
            Icons.unfold_more,
            'Expand all',
            () => expand(DiffExpandAction.all),
          ),
        ],
      ),
    );
  }

  Widget _expandButton(
    CommanderTokens t,
    IconData icon,
    String tooltip,
    VoidCallback onTap,
  ) => IconButton(
    visualDensity: VisualDensity.compact,
    padding: EdgeInsets.zero,
    constraints: const BoxConstraints(minWidth: 32, minHeight: 28),
    iconSize: 16,
    color: t.textMuted,
    tooltip: tooltip,
    onPressed: onTap,
    icon: Icon(icon),
  );

  /// One column of a row: the line-number gutter, then the code.
  ///
  /// The gutter is a widget rather than part of the text run — which is why the
  /// bridge strips it off the spans — so a soft-wrapped continuation stays in
  /// the code column instead of running back under the numbers.
  Widget _half(
    CommanderTokens t,
    DiffColors colors,
    DiffRowDto row,
    DiffCellDto cell,
    bool selectable,
  ) {
    if (!cell.present) {
      return Container(color: colors.alignmentGapFill);
    }
    final expanded = row.kind == DiffRowKind.expandedContext;
    final selected = _inSelection(cell.sel);
    final fill = selected
        ? colors.selectionFill
        : expanded
        ? colors.expandedFill
        : colors.lineFill(cell.origin);
    final (sign, signColor) = colors.sign(cell.origin);
    // Revealed context is not part of the diff, so it cannot be commented on.
    final sel = expanded ? null : cell.sel;
    final canSelect = selectable && sel != null;

    return InkWell(
      onTap: canSelect ? () => _tap(sel) : null,
      onLongPress: canSelect ? () => _extend(sel) : null,
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          _gutter(t, colors, cell, expanded),
          Expanded(
            child: Container(
              color: fill,
              padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 1),
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  SizedBox(
                    width: 10,
                    child: Text(
                      sign,
                      style: t.meta(size: 12, color: signColor, height: 1.45),
                    ),
                  ),
                  Expanded(
                    child: Text.rich(
                      TextSpan(
                        children: [
                          for (final s in cell.spans)
                            TextSpan(text: s.text, style: colors.spanStyle(s)),
                        ],
                      ),
                    ),
                  ),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _gutter(
    CommanderTokens t,
    DiffColors colors,
    DiffCellDto cell,
    bool expanded,
  ) {
    final style = t.meta(size: 11, color: colors.gutterFg, height: 1.45);
    // Side by side already splits the numbers across the two halves, so the dual
    // gutter is a single-column affordance only.
    final dual = widget.dualGutter && !widget.sideBySide;
    Widget number(int? n, double width) => SizedBox(
      width: width,
      child: Text(
        n?.toString() ?? '',
        textAlign: TextAlign.right,
        style: style,
      ),
    );
    return Container(
      color: expanded ? colors.expandedFill : colors.gutterFill(cell.origin),
      padding: const EdgeInsets.only(left: 6, right: 8, top: 1, bottom: 1),
      child: dual
          ? Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                number(cell.oldLineno, 32),
                const SizedBox(width: 6),
                number(cell.newLineno, 32),
              ],
            )
          : number(cell.newLineno ?? cell.oldLineno, 32),
    );
  }
}
