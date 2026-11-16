import 'package:claude_commander_client/src/rust/api/diff.dart';
import 'package:claude_commander_client/src/rust/api/review.dart'
    show ReviewLineOrigin;
import 'package:claude_commander_client/theme/diff_theme.dart';
import 'package:claude_commander_client/theme/tokens.dart';
import 'package:claude_commander_client/widgets/diff_view.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/fake_diff_layout.dart';

void main() {
  final colors = DiffColors.fromTokens(missionControlTokens);
  final expanded = <DiffExpansion>[];

  setUp(expanded.clear);

  Widget wrap(
    DiffLayoutDto layout, {
    bool sideBySide = false,
    AddCommentFn? onAddComment,
  }) => MaterialApp(
    home: Scaffold(
      body: SingleChildScrollView(
        child: DiffView(
          file: 'lib/foo.dart',
          layout: layout,
          sideBySide: sideBySide,
          onAddComment:
              onAddComment ??
              (({
                required file,
                required side,
                required lineStart,
                required lineEnd,
                required snippet,
              }) async {}),
          onExpand: expanded.add,
        ),
      ),
    ),
  );

  /// The style the renderer gave the run whose text is [text].
  TextStyle? styleOfRun(WidgetTester tester, String text) {
    TextStyle? found;
    for (final w in tester.widgetList<Text>(find.byType(Text))) {
      w.textSpan?.visitChildren((span) {
        if (span is TextSpan && span.text == text) found = span.style;
        return found == null;
      });
      if (found != null) break;
    }
    return found;
  }

  DiffRowDto lineRow({
    required List<DiffSpanDto> spans,
    required ReviewLineOrigin origin,
    int sel = 0,
    int? newLineno = 1,
  }) => DiffRowDto(
    kind: DiffRowKind.line,
    fullWidth: false,
    left: DiffCellDto(
      present: true,
      origin: origin,
      newLineno: newLineno,
      sel: sel,
      spans: spans,
      text: spans.map((s) => s.text).join(),
    ),
    right: absentCell,
    hidden: 0,
    canExpandUp: false,
    canExpandDown: false,
  );

  testWidgets('a large sliver diff builds only the visible rows', (
    tester,
  ) async {
    final rows = List.generate(
      5000,
      (i) => lineRow(
        origin: ReviewLineOrigin.addition,
        sel: i,
        newLineno: i + 1,
        spans: [
          DiffSpanDto(text: 'row $i', role: DiffRole.addition, emphasis: false),
        ],
      ),
    );
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: DiffTheme(
            colors: colors,
            child: CustomScrollView(
              slivers: [
                DiffView(
                  file: 'large.rs',
                  layout: DiffLayoutDto(
                    rows: rows,
                    selectable: 5000,
                    hasHiddenContext: false,
                  ),
                  sideBySide: false,
                  onAddComment: null,
                  onExpand: expanded.add,
                  sliver: true,
                ),
              ],
            ),
          ),
        ),
      ),
    );
    expect(find.textContaining('row 0'), findsWidgets);
    expect(find.textContaining('row 4999'), findsNothing);
    expect(
      tester.widgetList(find.byType(IntrinsicHeight)).length,
      lessThan(100),
    );
    expect(tester.takeException(), isNull);
  });

  testWidgets('a word-diff-emphasised run is tinted over its line fill', (
    tester,
  ) async {
    await tester.pumpWidget(
      wrap(
        DiffLayoutDto(
          rows: [
            lineRow(
              origin: ReviewLineOrigin.addition,
              spans: const [
                DiffSpanDto(
                  text: 'let b = ',
                  role: DiffRole.addition,
                  emphasis: false,
                ),
                DiffSpanDto(text: '3', role: DiffRole.addition, emphasis: true),
                DiffSpanDto(
                  text: ';',
                  role: DiffRole.addition,
                  emphasis: false,
                ),
              ],
            ),
          ],
          selectable: 1,
          hasHiddenContext: false,
        ),
      ),
    );

    // The changed run carries the emphasis fill; its neighbours carry none and
    // sit on the line fill the row's Container paints.
    expect(
      styleOfRun(tester, '3')!.backgroundColor,
      colors.emphasisFill(DiffRole.addition),
    );
    expect(styleOfRun(tester, 'let b = ')!.backgroundColor, isNull);
  });

  testWidgets('side by side draws both halves, and a missing half as a gap', (
    tester,
  ) async {
    final paired = DiffRowDto(
      kind: DiffRowKind.line,
      fullWidth: false,
      left: cellOf(
        'old text',
        DiffRole.deletion,
        origin: ReviewLineOrigin.deletion,
        oldLineno: 4,
        sel: 0,
      ),
      right: cellOf(
        'new text',
        DiffRole.addition,
        origin: ReviewLineOrigin.addition,
        newLineno: 4,
        sel: 1,
      ),
      hidden: 0,
      canExpandUp: false,
      canExpandDown: false,
    );
    final unbalanced = DiffRowDto(
      kind: DiffRowKind.line,
      fullWidth: false,
      left: absentCell,
      right: cellOf(
        'extra addition',
        DiffRole.addition,
        origin: ReviewLineOrigin.addition,
        newLineno: 5,
        sel: 2,
      ),
      hidden: 0,
      canExpandUp: false,
      canExpandDown: false,
    );
    await tester.pumpWidget(
      wrap(
        DiffLayoutDto(
          rows: [paired, unbalanced],
          selectable: 3,
          hasHiddenContext: false,
        ),
        sideBySide: true,
      ),
    );

    expect(find.text('old text'), findsOneWidget);
    expect(find.text('new text'), findsOneWidget);
    expect(find.text('extra addition'), findsOneWidget);
    // The blank half is drawn as a dim fill, not left empty — "nothing here",
    // not "not drawn yet".
    expect(
      tester
          .widgetList<Container>(find.byType(Container))
          .any((c) => c.color == colors.alignmentGapFill),
      isTrue,
    );
  });

  testWidgets('an expand control reports the direction it was asked for', (
    tester,
  ) async {
    await tester.pumpWidget(
      wrap(
        DiffLayoutDto(
          rows: [
            DiffRowDto(
              kind: DiffRowKind.expandControl,
              fullWidth: true,
              left: absentCell,
              right: absentCell,
              gap: 2,
              hidden: 27,
              canExpandUp: true,
              canExpandDown: true,
            ),
          ],
          selectable: 0,
          hasHiddenContext: true,
        ),
      ),
    );

    expect(find.text('27 hidden lines'), findsOneWidget);
    await tester.tap(find.byIcon(Icons.keyboard_arrow_up));
    await tester.pump();
    await tester.tap(find.byIcon(Icons.unfold_more));
    await tester.pump();

    expect(expanded.map((e) => e.gap), [2, 2]);
    expect(expanded.map((e) => e.action), [
      DiffExpandAction.up,
      DiffExpandAction.all,
    ]);
  });

  testWidgets('revealed context cannot be commented on', (tester) async {
    // It is display-only — not part of the diff — so there is nothing for the
    // server to anchor a comment to. The bridge already withholds its `sel`;
    // this pins that the view does not invent a hit target anyway.
    await tester.pumpWidget(
      wrap(
        DiffLayoutDto(
          rows: [
            DiffRowDto(
              kind: DiffRowKind.expandedContext,
              fullWidth: false,
              left: cellOf(
                'revealed line',
                DiffRole.expandedContext,
                newLineno: 12,
              ),
              right: absentCell,
              gap: 1,
              hidden: 0,
              canExpandUp: false,
              canExpandDown: false,
            ),
          ],
          selectable: 0,
          hasHiddenContext: true,
        ),
      ),
    );

    await tester.tap(find.text('revealed line'));
    await tester.pump();
    expect(find.text('Comment on selection'), findsNothing);
  });

  testWidgets('the comment action follows the last selected line', (
    tester,
  ) async {
    String? gotSnippet;
    int? gotStart;
    int? gotEnd;
    await tester.pumpWidget(
      wrap(
        DiffLayoutDto(
          rows: [
            lineRow(
              origin: ReviewLineOrigin.addition,
              sel: 0,
              newLineno: 7,
              spans: const [
                DiffSpanDto(
                  text: 'first',
                  role: DiffRole.addition,
                  emphasis: false,
                ),
              ],
            ),
            lineRow(
              origin: ReviewLineOrigin.addition,
              sel: 1,
              newLineno: 8,
              spans: const [
                DiffSpanDto(
                  text: 'second',
                  role: DiffRole.addition,
                  emphasis: false,
                ),
              ],
            ),
          ],
          selectable: 2,
          hasHiddenContext: false,
        ),
        onAddComment:
            ({
              required file,
              required side,
              required lineStart,
              required lineEnd,
              required snippet,
            }) async {
              gotSnippet = snippet;
              gotStart = lineStart;
              gotEnd = lineEnd;
            },
      ),
    );

    await tester.tap(find.text('first'));
    await tester.pump();
    await tester.longPress(find.text('second'));
    await tester.pump();
    await tester.tap(find.text('Comment on selection'));
    await tester.pump();

    // The range spans both lines, and the snippet is their text in file order —
    // what the server re-anchors against.
    expect(gotStart, 7);
    expect(gotEnd, 8);
    expect(gotSnippet, 'first\nsecond');
  });
}
