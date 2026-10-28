import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:claude_commander_client/pages/review_page.dart';
import 'package:claude_commander_client/src/rust/api/diff.dart';
import 'package:claude_commander_client/src/rust/api/review.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fake_commander_api.dart';
import 'support/fixtures.dart';

void main() {
  late FakeCommanderApi api;

  setUp(() => api = FakeCommanderApi());

  Widget wrap() => MaterialApp(
    home: ReviewPage(api: api, handle: testHandle, session: sessionInfo()),
  );

  testWidgets('a snapshot renders file cards and diff rows', (tester) async {
    api.openReviewResponse = reviewSnapshot(
      files: [
        reviewFile(
          displayPath: 'lib/foo.dart',
          hunks: [
            hunk(
              lines: [
                line(
                  ReviewLineOrigin.context,
                  'unchanged here',
                  oldLineno: 1,
                  newLineno: 1,
                ),
                line(ReviewLineOrigin.addition, 'brand new line', newLineno: 2),
              ],
            ),
          ],
        ),
      ],
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    expect(find.text('lib/foo.dart'), findsOneWidget);
    // The card starts collapsed; expand it to reveal the diff rows.
    await tester.tap(find.text('lib/foo.dart'));
    await tester.pumpAndSettle();
    expect(find.text('unchanged here'), findsOneWidget);
    expect(find.text('brand new line'), findsOneWidget);
  });

  testWidgets(
    'selecting a range then confirming the dialog calls createComment',
    (tester) async {
      api.openReviewResponse = reviewSnapshot(
        files: [
          reviewFile(
            displayPath: 'lib/foo.dart',
            hunks: [
              hunk(
                lines: [
                  line(ReviewLineOrigin.addition, 'target line', newLineno: 5),
                ],
              ),
            ],
          ),
        ],
      );
      await tester.pumpWidget(wrap());
      await tester.pumpAndSettle();
      await tester.tap(find.text('lib/foo.dart'));
      await tester.pumpAndSettle();

      // Tap the diff line to select it, then the "Comment on selection" action.
      await tester.tap(find.text('target line'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Comment on selection'));
      await tester.pumpAndSettle();

      // The comment dialog is up; type and confirm.
      expect(find.byType(AlertDialog), findsOneWidget);
      await tester.enterText(find.byType(TextField), 'needs a test');
      await tester.tap(find.widgetWithText(FilledButton, 'Add'));
      await tester.pumpAndSettle();

      expect(api.countOf('createComment'), 1);
      final call = api.lastCall('createComment')!;
      expect(call.args['comment'], 'needs a test');
      expect(call.args['side'], 'new');
      expect(call.args['lineStart'], 5);
    },
  );

  testWidgets(
    'a mixed deletion+addition selection comments on the new side with '
    'only new-side snippet content',
    (tester) async {
      api.openReviewResponse = reviewSnapshot(
        files: [
          reviewFile(
            displayPath: 'lib/foo.dart',
            hunks: [
              hunk(
                lines: [
                  line(
                    ReviewLineOrigin.deletion,
                    'old gone line',
                    oldLineno: 4,
                  ),
                  line(
                    ReviewLineOrigin.addition,
                    'fresh new line',
                    newLineno: 4,
                  ),
                ],
              ),
            ],
          ),
        ],
      );
      await tester.pumpWidget(wrap());
      await tester.pumpAndSettle();
      await tester.tap(find.text('lib/foo.dart'));
      await tester.pumpAndSettle();

      // Select the deletion row, then extend to the addition row: the selection
      // spans both a deletion and an addition (a "mixed" selection).
      InkWell rowOf(String content) => tester.widget<InkWell>(
        find.ancestor(of: find.text(content), matching: find.byType(InkWell)),
      );
      rowOf('old gone line').onTap!();
      await tester.pumpAndSettle();
      rowOf('fresh new line').onLongPress!();
      await tester.pumpAndSettle();

      await tester.tap(find.text('Comment on selection'));
      await tester.pumpAndSettle();
      await tester.enterText(find.byType(TextField), 'mixed comment');
      await tester.tap(find.widgetWithText(FilledButton, 'Add'));
      await tester.pumpAndSettle();

      expect(api.countOf('createComment'), 1);
      final call = api.lastCall('createComment')!;
      expect(call.args['side'], 'new');
      // The snippet must contain only the new-side line, not the deletion text —
      // otherwise the server's reanchor (which searches the chosen side only)
      // drifts the comment the moment it's created.
      expect(call.args['snippet'], 'fresh new line');
      expect(call.args['lineStart'], 4);
      expect(call.args['lineEnd'], 4);
    },
  );

  testWidgets('delete calls deleteComment', (tester) async {
    api.openReviewResponse = reviewSnapshot(
      comments: [comment(id: 'c-9', text: 'delete me')],
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    expect(find.text('delete me'), findsOneWidget);
    await tester.tap(find.widgetWithIcon(IconButton, Icons.delete_outline));
    await tester.pumpAndSettle();

    expect(api.countOf('deleteComment'), 1);
    expect(api.lastCall('deleteComment')!.args['commentId'], 'c-9');
  });

  testWidgets('a blocked apply result renders differently from applied', (
    tester,
  ) async {
    api.openReviewResponse = reviewSnapshot(
      comments: [comment(status: ReviewCommentStatus.staged)],
    );
    api.applyCommentsResponse = const ApplyResult(
      kind: ApplyResultKind.blocked,
      driftedIds: ['c-1', 'c-2'],
      count: 0,
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    await tester.tap(find.widgetWithText(FilledButton, 'Apply (1)'));
    await tester.pumpAndSettle();

    // Blocked → the drift count is surfaced, not an "Applied N" message.
    expect(find.textContaining('Blocked: 2 drifted'), findsOneWidget);
    expect(find.textContaining('Applied'), findsNothing);
  });

  testWidgets('an applied apply result surfaces the applied count', (
    tester,
  ) async {
    api.openReviewResponse = reviewSnapshot(
      comments: [comment(status: ReviewCommentStatus.staged)],
    );
    api.applyCommentsResponse = const ApplyResult(
      kind: ApplyResultKind.applied,
      driftedIds: [],
      count: 3,
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    await tester.tap(find.widgetWithText(FilledButton, 'Apply (1)'));
    await tester.pumpAndSettle();

    expect(find.textContaining('Applied 3 comment'), findsOneWidget);
  });

  testWidgets('the reviewed checkbox is optimistic and guarded against '
      'double-tap', (tester) async {
    // Gate setFileReviewed so the second tap arrives while the first is in
    // flight; the _toggling guard must swallow it.
    final gate = GatedReviewApi();
    gate.openReviewResponse = reviewSnapshot(
      files: [reviewFile(displayPath: 'lib/foo.dart')],
    );
    await tester.pumpWidget(
      MaterialApp(
        home: ReviewPage(api: gate, handle: testHandle, session: sessionInfo()),
      ),
    );
    await tester.pumpAndSettle();

    final checkbox = find.byType(Checkbox);
    await tester.tap(checkbox);
    await tester.pump();
    // Second rapid tap while the first flip is in flight.
    await tester.tap(checkbox);
    await tester.pump();

    expect(gate.countOf('setFileReviewed'), 1);

    // Complete the flip; the optimistic set flips the checkbox to true.
    gate.releaseToggle(true);
    await tester.pumpAndSettle();
    final cb = tester.widget<Checkbox>(checkbox);
    expect(cb.value, isTrue);
  });

  testWidgets('the reviewed checkbox asks for the state it flips to, not a '
      'toggle', (tester) async {
    // The store may hold a mark this page isn't showing (set by the CLI since
    // it loaded); a toggle would flip that instead of what the user sees.
    api.openReviewResponse = reviewSnapshot(
      files: [
        reviewFile(displayPath: 'lib/a.dart'),
        reviewFile(displayPath: 'lib/b.dart'),
      ],
      reviewed: ['lib/b.dart'],
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    await tester.tap(find.byType(Checkbox).at(0));
    await tester.pumpAndSettle();
    expect(api.lastCall('setFileReviewed')?.args, {
      'displayPath': 'lib/a.dart',
      'reviewed': true,
    });

    await tester.tap(find.byType(Checkbox).at(1));
    await tester.pumpAndSettle();
    expect(api.lastCall('setFileReviewed')?.args, {
      'displayPath': 'lib/b.dart',
      'reviewed': false,
    });
  });

  testWidgets('adding a comment does not setState after the hunk is disposed', (
    tester,
  ) async {
    final gate = GatedCommentApi();
    gate.openReviewResponse = reviewSnapshot(
      files: [
        reviewFile(
          displayPath: 'lib/foo.dart',
          hunks: [
            hunk(
              lines: [
                line(ReviewLineOrigin.addition, 'target line', newLineno: 5),
              ],
            ),
          ],
        ),
      ],
    );
    await tester.pumpWidget(
      MaterialApp(
        home: ReviewPage(api: gate, handle: testHandle, session: sessionInfo()),
      ),
    );
    await tester.pumpAndSettle(); // first openReview resolves normally

    await tester.tap(find.text('lib/foo.dart'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('target line'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Comment on selection'));
    await tester.pumpAndSettle();
    await tester.enterText(find.byType(TextField), 'boom');
    await tester.tap(find.widgetWithText(FilledButton, 'Add'));
    await tester.pumpAndSettle(); // dialog closes; createComment is in flight

    // Release createComment; the page's _addComment then calls _open(), whose
    // setState(_loading=true) swaps the diff list for a spinner and DISPOSES
    // this hunk's State while its re-open network call is still pending.
    gate.releaseCreate();
    await tester.pump(); // _open sets loading
    await tester.pump(); // rebuild → hunk subtree disposed

    // Release the re-open: _addComment returns and the hunk's _clear() runs
    // against the now-disposed State — which must not throw.
    gate.releaseReopen();
    await tester.pumpAndSettle();

    expect(tester.takeException(), isNull);
  });

  testWidgets('the wide layout shows the FILES CHANGED sidebar and switches '
      'the diff pane on file selection', (tester) async {
    tester.view.physicalSize = const Size(1400, 1000);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);

    api.openReviewResponse = reviewSnapshot(
      files: [
        reviewFile(
          displayPath: 'lib/first.dart',
          hunks: [
            hunk(
              lines: [
                line(
                  ReviewLineOrigin.addition,
                  'first file line',
                  newLineno: 1,
                ),
              ],
            ),
          ],
        ),
        reviewFile(
          displayPath: 'lib/second.dart',
          hunks: [
            hunk(
              lines: [
                line(
                  ReviewLineOrigin.addition,
                  'second file line',
                  newLineno: 1,
                ),
              ],
            ),
          ],
        ),
      ],
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    // The sidebar is a compressed tree: the shared `lib` directory is one row,
    // and each file shows its own leaf rather than repeating the path. The
    // first file's diff is shown by default (the pane is always open).
    expect(find.text('FILES CHANGED'), findsOneWidget);
    expect(find.text('lib'), findsOneWidget);
    expect(find.text('first.dart'), findsOneWidget);
    expect(find.text('second.dart'), findsOneWidget);
    expect(find.text('first file line'), findsOneWidget);
    expect(find.text('second file line'), findsNothing);

    // Selecting the second file swaps the diff pane to its hunks.
    await tester.tap(find.text('second.dart'));
    await tester.pumpAndSettle();
    expect(find.text('second file line'), findsOneWidget);
    expect(find.text('first file line'), findsNothing);

    // Collapsing the directory hides its files without disturbing the pane.
    await tester.tap(find.text('lib'));
    await tester.pumpAndSettle();
    expect(find.text('first.dart'), findsNothing);
    expect(find.text('second.dart'), findsNothing);
    expect(find.text('second file line'), findsOneWidget);
  });

  testWidgets('the wide layout surfaces a comment whose file is not among the '
      'changed files under "Other comments" with a reachable delete', (
    tester,
  ) async {
    tester.view.physicalSize = const Size(1400, 1000);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);

    api.openReviewResponse = reviewSnapshot(
      files: [reviewFile(displayPath: 'lib/first.dart')],
      // Comment on a file that is no longer in the diff (e.g. reverted): it must
      // not become unreachable on desktop, or a drifted one blocks apply.
      comments: [
        comment(id: 'c-orphan', file: 'lib/gone.dart', text: 'orphaned note'),
      ],
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    expect(find.text('OTHER COMMENTS'), findsOneWidget);
    expect(find.text('orphaned note'), findsOneWidget);

    // Delete stays reachable for the orphaned comment.
    await tester.tap(find.widgetWithIcon(IconButton, Icons.delete_outline));
    await tester.pumpAndSettle();
    expect(api.countOf('deleteComment'), 1);
    expect(api.lastCall('deleteComment')!.args['commentId'], 'c-orphan');
  });

  testWidgets('the wide layout can switch the diff to side by side', (
    tester,
  ) async {
    tester.view.physicalSize = const Size(1400, 1000);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);

    api.openReviewResponse = reviewSnapshot(
      files: [
        reviewFile(
          displayPath: 'lib/foo.dart',
          hunks: [
            hunk(
              lines: [line(ReviewLineOrigin.addition, 'a line', newLineno: 1)],
            ),
          ],
        ),
      ],
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    // Unified until asked otherwise — the layout mode is a property of the
    // request, so the toggle has to reach the cdylib, not just repaint.
    expect(api.lastCall('diffRows')!.args['mode'], DiffLayoutMode.inline);
    await tester.tap(find.text('split'));
    await tester.pumpAndSettle();
    expect(api.lastCall('diffRows')!.args['mode'], DiffLayoutMode.sideBySide);
  });

  testWidgets('a diff with hidden context fetches the file text so the expand '
      'controls can appear', (tester) async {
    // Expansion reveals lines the diff never carried, so they can only come from
    // the working-tree file — and the fetch is deferred until the layout says
    // there is something hidden, so a fully-shown file costs no round trip.
    api.diffRowsResponse = const DiffLayoutDto(
      rows: [],
      selectable: 0,
      hasHiddenContext: true,
    );
    api.openReviewResponse = reviewSnapshot(
      files: [reviewFile(displayPath: 'lib/foo.dart')],
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();
    await tester.tap(find.text('lib/foo.dart'));
    await tester.pumpAndSettle();

    final fetch = api.lastCall('fetchBlob');
    expect(fetch, isNotNull);
    expect(fetch!.args['side'], 'new');
    expect(fetch.args['path'], 'lib/foo.dart');
    // ...and the layout is rebuilt with the text, so the controls can render.
    expect(api.lastCall('diffRows')!.args['hasText'], isTrue);
  });

  testWidgets('a file text fetch that lands after the file was switched is '
      'dropped instead of poisoning the new file', (tester) async {
    // The diff pane keeps one state object and is handed a different file when
    // the sidebar selection changes, so an in-flight fetch for the old file
    // outlives the switch. Applied to the new file it would supply the wrong
    // `FileSource` — wrong trailing-gap size, wrong revealed lines — and
    // permanently overwrite the new file's own text if that arrived first.
    tester.view.physicalSize = const Size(1400, 1000);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);

    // Every file reports hidden context, so each one triggers a text fetch.
    api.diffRowsResponse = const DiffLayoutDto(
      rows: [],
      selectable: 0,
      hasHiddenContext: true,
    );
    api.fetchBlobResponses['lib/first.dart'] = Uint8List.fromList(
      utf8.encode('FIRST TEXT'),
    );
    api.fetchBlobResponses['lib/second.dart'] = Uint8List.fromList(
      utf8.encode('SECOND TEXT'),
    );
    // Hold the first file's fetch open across the switch.
    final firstFetch = Completer<void>();
    api.fetchBlobGates['lib/first.dart'] = firstFetch;

    api.openReviewResponse = reviewSnapshot(
      files: [
        reviewFile(displayPath: 'lib/first.dart'),
        reviewFile(displayPath: 'lib/second.dart'),
      ],
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    // The first file's fetch is in flight and cannot have completed.
    expect(api.lastCall('fetchBlob')!.args['path'], 'lib/first.dart');
    expect(api.lastCall('diffRows')!.args['text'], isNull);

    // Switch to the second file, whose own fetch completes normally.
    await tester.tap(find.text('second.dart'));
    await tester.pumpAndSettle();
    expect(api.lastCall('diffRows')!.args['file'], 'lib/second.dart');
    expect(api.lastCall('diffRows')!.args['text'], 'SECOND TEXT');
    final layoutsBefore = api.countOf('diffRows');

    // Now the stale fetch lands. It must be dropped: no re-layout, and the
    // second file keeps its own text.
    firstFetch.complete();
    await tester.pumpAndSettle();

    expect(
      api.countOf('diffRows'),
      layoutsBefore,
      reason: 'a stale fetch must not trigger a re-layout of the new file',
    );
    expect(api.lastCall('diffRows')!.args['text'], 'SECOND TEXT');
  });

  testWidgets('a snapshot refresh resets a now-out-of-range line selection', (
    tester,
  ) async {
    api.openReviewResponse = reviewSnapshot(
      contentHash: 'A',
      files: [
        reviewFile(
          displayPath: 'lib/foo.dart',
          hunks: [
            hunk(
              lines: [
                line(
                  ReviewLineOrigin.context,
                  'line one',
                  oldLineno: 1,
                  newLineno: 1,
                ),
                line(
                  ReviewLineOrigin.context,
                  'line two',
                  oldLineno: 2,
                  newLineno: 2,
                ),
                line(ReviewLineOrigin.addition, 'line three', newLineno: 3),
              ],
            ),
          ],
        ),
      ],
    );
    api.refreshReviewResponse = reviewSnapshot(
      contentHash: 'B',
      files: [
        reviewFile(
          displayPath: 'lib/foo.dart',
          hunks: [
            hunk(
              lines: [
                line(ReviewLineOrigin.addition, 'only line', newLineno: 1),
              ],
            ),
          ],
        ),
      ],
    );
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();
    await tester.tap(find.text('lib/foo.dart'));
    await tester.pumpAndSettle();

    // Select the last line of the 3-line hunk (index 2).
    await tester.tap(find.text('line three'));
    await tester.pumpAndSettle();
    expect(find.text('Comment on selection'), findsOneWidget);

    // Refresh to a snapshot whose hunk has a single line: the reused hunk State
    // must drop its stale index-2 selection rather than range-error on it.
    await tester.tap(find.widgetWithIcon(IconButton, Icons.refresh));
    await tester.pumpAndSettle();

    expect(tester.takeException(), isNull);
    expect(find.text('only line'), findsOneWidget);
    expect(find.text('Comment on selection'), findsNothing);
  });
}

/// A [FakeCommanderApi] that holds `createComment` and the *second* `openReview`
/// (the re-open triggered after staging a comment) behind completers, so a test
/// can interleave the disposal of the hunk subtree with the in-flight re-open.
class GatedCommentApi extends FakeCommanderApi {
  final Completer<void> _createGate = Completer<void>();
  final Completer<void> _reopenGate = Completer<void>();
  int _openCount = 0;

  void releaseCreate() => _createGate.complete();
  void releaseReopen() => _reopenGate.complete();

  @override
  Future<ReviewSnapshotDto> openReview({
    required String handle,
    required String sessionId,
  }) async {
    calls.add(RecordedCall('openReview', {'sessionId': sessionId}));
    _openCount++;
    if (_openCount > 1) await _reopenGate.future;
    final snap = openReviewResponse;
    if (snap == null) {
      throw StateError('openReviewResponse not set on GatedCommentApi');
    }
    return snap;
  }

  @override
  Future<String> createComment({
    required String handle,
    required String sessionId,
    required String file,
    required String side,
    required int lineStart,
    required int lineEnd,
    required String snippet,
    required String comment,
  }) async {
    calls.add(
      RecordedCall('createComment', {
        'file': file,
        'side': side,
        'lineStart': lineStart,
        'lineEnd': lineEnd,
        'snippet': snippet,
        'comment': comment,
      }),
    );
    await _createGate.future;
    return createCommentResponse;
  }
}

/// A [FakeCommanderApi] whose `setFileReviewed` blocks until released, so a
/// test can hold the flip in flight and exercise the `_toggling` guard.
class GatedReviewApi extends FakeCommanderApi {
  final Completer<bool> _gate = Completer<bool>();

  void releaseToggle(bool result) => _gate.complete(result);

  @override
  Future<bool> setFileReviewed({
    required String handle,
    required String sessionId,
    required String displayPath,
    required bool reviewed,
  }) {
    calls.add(
      RecordedCall('setFileReviewed', {
        'displayPath': displayPath,
        'reviewed': reviewed,
      }),
    );
    return _gate.future;
  }
}
