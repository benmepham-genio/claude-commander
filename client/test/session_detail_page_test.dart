import 'dart:async';
import 'dart:convert';
import 'package:claude_commander_client/services/session_editor.dart';
import 'support/fake_session_editor.dart';

import 'package:claude_commander_client/pages/review_page.dart';
import 'package:claude_commander_client/pages/session_detail_page.dart';
import 'package:claude_commander_client/pages/terminal_page.dart';
import 'package:claude_commander_client/src/rust/api/mirrors.dart';
import 'package:claude_commander_client/state/commander_store.dart';
import 'package:claude_commander_client/state/commander_store_scope.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fake_commander_api.dart';
import 'support/fixtures.dart';
import 'theme/mission_control_reference.dart';

void main() {
  late FakeCommanderApi api;
  late CommanderStore store;

  setUp(() {
    api = FakeCommanderApi();
    store = CommanderStore(api: api, config: testConfig);
  });

  tearDown(() => store.dispose());

  Widget scope(Widget child) => CommanderStoreScope(
    store: store,
    child: MaterialApp(home: child),
  );

  Widget wrap(SessionInfo session) =>
      scope(SessionDetailPage(session: session));

  /// Connect the store (so the page has a live handle), then pump the page and
  /// let the initial on-demand detail fetch resolve.
  Future<void> pump(WidgetTester tester, SessionInfo session) async {
    await store.connect();
    await tester.pumpWidget(wrap(session));
    await tester.pumpAndSettle();
  }

  testWidgets('renders detail from the store', (tester) async {
    api.getSessionDetailResponse = sessionDetail(
      info: sessionInfo(title: 'Detail me', status: SessionStatus.running),
      diffStat: '3 files changed',
    );
    await pump(tester, sessionInfo(title: 'Detail me'));

    expect(find.text('Detail me'), findsWidgets);
    expect(find.text('3 files changed'), findsOneWidget);
  });

  testWidgets('the header surfaces section and keep-alive as chips', (
    tester,
  ) async {
    // Section + keep-alive moved into the ⋮ menu, so their state is shown as
    // read-only chips instead. An explicit section override wins over the
    // current section (matching the menu/edit precedence).
    final info = sessionInfo(
      keepAlive: true,
      currentSection: 'auto',
      sectionOverride: 'review',
    );
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    expect(find.text('▤ review'), findsOneWidget);
    expect(find.text('▤ auto'), findsNothing);
    expect(find.text('✓ keep-alive'), findsOneWidget);
  });

  testWidgets('the header omits the chips when unset', (tester) async {
    final info = sessionInfo(); // keepAlive false, no section
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    expect(find.textContaining('▤'), findsNothing);
    expect(find.text('✓ keep-alive'), findsNothing);
  });

  /// Build an agent-states snapshot marking [info]'s session with [state], so
  /// the store's live agent state (not the detail fixture) drives the header.
  AgentStatesSnapshotDto agentStates(SessionInfo info, AgentState state) =>
      AgentStatesSnapshotDto(
        states: [AgentStateEntryDto(sessionId: info.sessionId, state: state)],
        commanderRunning: true,
      );

  testWidgets('a waiting agent shows the amber waiting-input hint', (
    tester,
  ) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    api.agentStatesResponse = agentStates(info, AgentState.waitingForInput);
    await pump(tester, info);

    expect(find.textContaining('answer in the Agent terminal'), findsOneWidget);
  });

  testWidgets('a non-waiting agent omits the waiting-input hint', (
    tester,
  ) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    api.agentStatesResponse = agentStates(info, AgentState.working);
    await pump(tester, info);

    expect(find.textContaining('answer in the Agent terminal'), findsNothing);
  });

  testWidgets('the phone detail hides the terminal-snapshot preview', (
    tester,
  ) async {
    // Even when the server would return pane content, the phone layout does
    // not render the snapshot card — the live terminal is one tap away and far
    // more useful in a small viewport.
    api.getSessionDetailResponse = sessionDetail(
      info: sessionInfo(title: 'Detail me', status: SessionStatus.running),
      paneContent: 'hello world',
    );
    await pump(tester, sessionInfo(title: 'Detail me'));

    expect(find.text('Terminal snapshot'), findsNothing);
    expect(find.text('hello world'), findsNothing);
  });

  testWidgets('the phone detail fetch skips pane capture (lines null)', (
    tester,
  ) async {
    // With no preview to render, the phone must not ask the server to capture
    // pane lines — `lines: null` tells the server to skip the tmux capture.
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    expect(api.lastCall('getSessionDetail')!.args['lines'], isNull);
  });

  testWidgets('a deleted session shows a gone state and stops fetching', (
    tester,
  ) async {
    // getSessionDetail returns null (404 → session gone by design).
    api.getSessionDetailResponse = null;
    await pump(tester, sessionInfo(title: 'Gone one'));

    expect(find.textContaining('no longer exists'), findsOneWidget);
    // The lifecycle action bar is gone — no live controls.
    expect(find.byTooltip('Kill'), findsNothing);
    expect(find.byTooltip('Restart'), findsNothing);
    expect(find.byTooltip('Delete'), findsNothing);

    // Once gone, a change-feed tick must not fetch detail again.
    final callsSoFar = api.countOf('getSessionDetail');
    api.emitChange();
    await tester.pumpAndSettle();
    expect(api.countOf('getSessionDetail'), callsSoFar);
  });

  Future<void> confirmAction(
    WidgetTester tester, {
    required Finder trigger,
    required String confirmLabel,
  }) async {
    await tester.tap(trigger);
    await tester.pump();
    // The confirm dialog is up.
    expect(find.byType(AlertDialog), findsOneWidget);
    // The dialog's confirm button sits above the page's action button of the
    // same label, so target the dialog one via the AlertDialog subtree.
    await tester.tap(
      find.descendant(
        of: find.byType(AlertDialog),
        matching: find.widgetWithText(FilledButton, confirmLabel),
      ),
    );
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 10));
  }

  testWidgets('kill opens a confirm dialog then calls killSession', (
    tester,
  ) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    await confirmAction(
      tester,
      trigger: find.byTooltip('Kill'),
      confirmLabel: 'Kill',
    );
    expect(api.countOf('killSession'), 1);
  });

  testWidgets('restart opens a confirm dialog then calls restartSession', (
    tester,
  ) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    await confirmAction(
      tester,
      trigger: find.byTooltip('Restart'),
      confirmLabel: 'Restart',
    );
    expect(api.countOf('restartSession'), 1);
  });

  // A restart is when a deferred program change takes effect, and the switch
  // discards the conversation — the confirm has to say so, the way the TUI's
  // `reset_confirm_message` does. Without a pending program it must not.
  testWidgets('restart warns that a pending program takes over', (
    tester,
  ) async {
    final info = sessionInfo(
      status: SessionStatus.running,
      pendingProgram: 'codex',
    );
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    await tester.tap(find.byTooltip('Restart'));
    await tester.pump();
    final dialog = find.byType(AlertDialog);
    expect(dialog, findsOneWidget);
    expect(
      find.descendant(of: dialog, matching: find.textContaining('"codex"')),
      findsOneWidget,
    );
    expect(
      find.descendant(
        of: dialog,
        matching: find.textContaining('conversation is cleared'),
      ),
      findsOneWidget,
    );
  });

  testWidgets('restart without a pending program names no program', (
    tester,
  ) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    await tester.tap(find.byTooltip('Restart'));
    await tester.pump();
    final dialog = find.byType(AlertDialog);
    expect(
      find.descendant(
        of: dialog,
        matching: find.text('Restarts the program in this session.'),
      ),
      findsOneWidget,
    );
    expect(
      find.descendant(
        of: dialog,
        matching: find.textContaining('conversation is cleared'),
      ),
      findsNothing,
    );
  });

  testWidgets('delete confirms, calls deleteSession, and pops', (tester) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await store.connect();
    // Push the detail page as a second route (over a placeholder home) so the
    // delete-pop has an underlying route to return to.
    await tester.pumpWidget(
      scope(
        Builder(
          builder: (context) => Scaffold(
            body: Center(
              child: ElevatedButton(
                onPressed: () => Navigator.of(context).push(
                  MaterialPageRoute(
                    builder: (_) => SessionDetailPage(session: info),
                  ),
                ),
                child: const Text('open'),
              ),
            ),
          ),
        ),
      ),
    );
    await tester.tap(find.text('open'));
    await tester.pumpAndSettle();

    await confirmAction(
      tester,
      trigger: find.byTooltip('Delete'),
      confirmLabel: 'Delete',
    );
    expect(api.countOf('deleteSession'), 1);
    // popOnSuccess: true → the page is gone, back to the placeholder home.
    await tester.pumpAndSettle();
    expect(find.byType(SessionDetailPage), findsNothing);
    expect(find.text('open'), findsOneWidget);
  });

  testWidgets('the busy guard disables the actions while one is in flight', (
    tester,
  ) async {
    // Gate killSession on a completer so the action stays in flight and _busy
    // stays set while we inspect the buttons.
    final gate = GatedCommanderApi();
    gate.getSessionDetailResponse = sessionDetail(
      info: sessionInfo(status: SessionStatus.running),
    );
    final gstore = CommanderStore(api: gate, config: testConfig);
    addTearDown(gstore.dispose);
    await gstore.connect();
    final info = sessionInfo(status: SessionStatus.running);
    await tester.pumpWidget(
      CommanderStoreScope(
        store: gstore,
        child: MaterialApp(home: SessionDetailPage(session: info)),
      ),
    );
    await tester.pumpAndSettle();

    // Confirm kill; the completer is not yet complete, so the action hangs.
    await tester.tap(find.byTooltip('Kill'));
    await tester.pump();
    await tester.tap(
      find.descendant(
        of: find.byType(AlertDialog),
        matching: find.widgetWithText(FilledButton, 'Kill'),
      ),
    );
    await tester.pump();

    // _busy is set → the Restart/Delete icon buttons are disabled (onPressed
    // null).
    final restart = tester.widget<IconButton>(
      find.widgetWithIcon(IconButton, Icons.restart_alt),
    );
    final delete = tester.widget<IconButton>(
      find.widgetWithIcon(IconButton, Icons.delete_outline),
    );
    expect(restart.onPressed, isNull);
    expect(delete.onPressed, isNull);
    expect(gate.countOf('killSession'), 1);

    // Release the in-flight call so the widget can settle cleanly.
    gate.releaseKill();
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 10));
  });

  testWidgets('editor forwards the shared changed-field plan', (tester) async {
    final bridge = FakeSessionEditorBridge();
    debugSessionEditorBridge = bridge;
    addTearDown(() => debugSessionEditorBridge = null);
    final info = sessionInfo(title: 'Old', status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);
    await tester.tap(find.byIcon(Icons.more_vert));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Edit session'));
    await tester.pumpAndSettle();
    await tester.enterText(
      find.widgetWithText(TextFormField, 'Session name'),
      'New name',
    );
    await tester.tap(find.widgetWithText(FilledButton, 'Save'));
    await tester.pumpAndSettle();
    expect(bridge.lastDraft!['title'], 'New name');
    expect(api.countOf('editSession'), 1);
    expect(
      jsonDecode(api.lastCall('editSession')!.args['patchJson'] as String),
      {'title': 'New name', 'restart': false},
    );
  });

  testWidgets('opening an unread session marks it read once', (tester) async {
    final info = sessionInfo(unread: true);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    expect(api.countOf('markRead'), 1);

    // A subsequent change-feed tick must not re-mark it read.
    api.emitChange();
    await tester.pumpAndSettle();
    expect(api.countOf('markRead'), 1);
  });

  testWidgets('opening an already-read session does not mark read', (
    tester,
  ) async {
    final info = sessionInfo(unread: false);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    expect(api.countOf('markRead'), 0);
  });

  testWidgets('cascade merge confirms, calls cascadeMerge, reports outcome', (
    tester,
  ) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    api.operationStatusResponse = OperationStatusDto(
      id: BigInt.one,
      kind: OperationKind.cascade,
      outcome: const OperationOutcomeDto(
        kind: OperationOutcomeKind.succeeded,
        detail: '',
      ),
    );
    await pump(tester, info);

    await confirmAction(
      tester,
      trigger: find.byTooltip('Cascade merge'),
      confirmLabel: 'Cascade',
    );
    expect(api.countOf('cascadeMerge'), 1);
    expect(find.textContaining('Cascade merge succeeded'), findsOneWidget);
  });

  testWidgets('a paused cascade outcome is reported in the snackbar', (
    tester,
  ) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    api.operationStatusResponse = OperationStatusDto(
      id: BigInt.one,
      kind: OperationKind.cascade,
      outcome: const OperationOutcomeDto(
        kind: OperationOutcomeKind.paused,
        detail: 'conflict in foo.rs',
      ),
    );
    await pump(tester, info);

    await confirmAction(
      tester,
      trigger: find.byTooltip('Cascade merge'),
      confirmLabel: 'Cascade',
    );
    expect(
      find.textContaining('Cascade merge paused: conflict in foo.rs'),
      findsOneWidget,
    );
  });

  testWidgets('the lifecycle bar keeps Kill amber and Restart teal', (
    tester,
  ) async {
    // The *call site* assertion. `bar_button_accent_test.dart` proves the chrome
    // honours an accent; this proves this bar still passes one. Without it,
    // deleting `accent:` from `_lifecycleBar` flattens both icons to the neutral
    // textBright and every other test still passes — which is how that regression
    // reached review in the first place.
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    // IconButton builds its Tooltip *inside* itself, so the button is the
    // tooltip's ancestor, not its descendant.
    Color? iconColour(String tooltip) => tester
        .widget<IconButton>(
          find
              .ancestor(
                of: find.byTooltip(tooltip),
                matching: find.byType(IconButton),
              )
              .first,
        )
        .color;

    // Only these two: they are the pair that regressed. That a *normal* button
    // stays neutral is covered by `chrome/bar_button_accent_test.dart`, and the
    // other lifecycle labels appear elsewhere in this page's tree, so matching
    // them by tooltip here is ambiguous.
    expect(iconColour('Kill'), McRef.amberText);
    expect(iconColour('Restart'), McRef.teal);
  });

  testWidgets('push stack confirms then calls pushStack', (tester) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    await confirmAction(
      tester,
      trigger: find.byTooltip('Push stack'),
      confirmLabel: 'Push',
    );
    expect(api.countOf('pushStack'), 1);
  });

  testWidgets('the Shell action opens a shell terminal attach', (tester) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    // Shell now lives in the lifecycle icon bar rather than as its own button.
    await tester.tap(find.byTooltip('Shell'));
    await tester.pump();
    // Let the pushed route build (and TerminalBody attach); avoid pumpAndSettle
    // because the terminal's 1s throughput timer never settles.
    await tester.pump(const Duration(milliseconds: 400));

    expect(find.byType(TerminalPage), findsOneWidget);
    expect(api.lastCall('attachTerminal')!.args['kind'], AttachKind.shell);
  });

  testWidgets('the Agent hero opens an agent terminal attach', (tester) async {
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(info: info);
    await pump(tester, info);

    await tester.tap(find.widgetWithText(FilledButton, 'Open Agent terminal'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 400));

    expect(find.byType(TerminalPage), findsOneWidget);
    expect(api.lastCall('attachTerminal')!.args['kind'], AttachKind.agent);
  });

  testWidgets('the Changes card opens the review view (no Review button)', (
    tester,
  ) async {
    // The standalone Review button is gone; the Changes card is the diff entry
    // point.
    final info = sessionInfo(status: SessionStatus.running);
    api.getSessionDetailResponse = sessionDetail(
      info: info,
      diffStat: '3 files changed',
    );
    api.openReviewResponse = reviewSnapshot();
    await pump(tester, info);

    expect(find.widgetWithText(OutlinedButton, 'Review'), findsNothing);

    await tester.tap(find.text('Changes'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 400));

    expect(find.byType(ReviewPage), findsOneWidget);
    expect(api.countOf('openReview'), 1);
  });
}

/// A [FakeCommanderApi] whose `killSession` blocks on a completer, so a test can
/// hold the action in flight and observe the `_busy` gate.
class GatedCommanderApi extends FakeCommanderApi {
  final _killGate = Completer<void>();

  void releaseKill() => _killGate.complete();

  @override
  Future<void> killSession({required String handle, required String id}) async {
    calls.add(RecordedCall('killSession', {'id': id}));
    await _killGate.future;
  }
}
