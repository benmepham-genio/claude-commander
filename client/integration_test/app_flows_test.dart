// Full-stack e2e: drives the REAL app (RustCommanderApi over the frb bridge)
// against a REAL, hermetic claude-commander-server, on the Linux desktop target.
// Launch via client/tool/e2e.sh, which boots the server over throwaway XDG state
// and passes its address/token/repo through --dart-define. Running this file with
// a plain `flutter test` (no server) fails at the connect step by design.
//
// One continuous journey (not several tests): the detail page polls on a 2s
// Timer and the terminal streams events, so the integration-test binding can
// otherwise trip its between-test frame/`inTest` assertions on desktop. A single
// test that ends on the timer-free session list keeps the teardown clean while
// still covering every happy path in sequence.
//
// Pumping idiom: `pumpAndSettle` never settles on the polling/streaming pages, so
// every wait uses `pumpUntil`, which pumps frames until a condition holds or a
// real-time deadline passes (real time advances because network + PTY I/O
// complete between pumps). Terminal output is read from the xterm `Terminal`
// buffer via the public `TerminalView.terminal` field (glyphs are canvas-painted,
// so `find.text` can't see them); diff rows are ordinary `Text` widgets.

import 'package:claude_commander_client/chrome/chrome_forms.dart';
import 'package:claude_commander_client/main.dart';
import 'package:claude_commander_client/server_config.dart';
import 'package:claude_commander_client/services/commander_api.dart';
import 'package:claude_commander_client/src/rust/frb_generated.dart';
import 'package:claude_commander_client/state/fleet_store.dart';
import 'package:flutter/material.dart';
import 'package:claude_commander_client/services/pref_store.dart';
import 'package:claude_commander_client/theme/theme_controller.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:xterm/xterm.dart';

const _baseUrl = String.fromEnvironment('CC_E2E_BASE_URL');
const _token = String.fromEnvironment('CC_E2E_TOKEN');
const _repo = String.fromEnvironment('CC_E2E_REPO');

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  setUpAll(() async {
    await RustLib.init();
    expect(
      _baseUrl.isNotEmpty && _token.isNotEmpty && _repo.isNotEmpty,
      isTrue,
      reason:
          'CC_E2E_BASE_URL / CC_E2E_TOKEN / CC_E2E_REPO must be passed via '
          '--dart-define (run through client/tool/e2e.sh)',
    );
  });

  // Pump frames until [cond] holds or [timeout] (real time) elapses.
  Future<void> pumpUntil(
    WidgetTester tester,
    bool Function() cond, {
    Duration timeout = const Duration(seconds: 25),
    String reason = 'condition',
  }) async {
    final deadline = DateTime.now().add(timeout);
    while (DateTime.now().isBefore(deadline)) {
      if (cond()) return;
      await tester.pump(const Duration(milliseconds: 120));
    }
    if (!cond()) {
      final visible = tester
          .widgetList<Text>(find.byType(Text))
          .map((t) => t.data)
          .where((s) => s != null)
          .toList();
      throw TestFailure('pumpUntil timed out: $reason\nvisible text: $visible');
    }
  }

  Future<void> waitFor(
    WidgetTester tester,
    Finder finder, {
    Duration timeout = const Duration(seconds: 25),
  }) => pumpUntil(
    tester,
    () => finder.evaluate().isNotEmpty,
    timeout: timeout,
    reason: 'finding $finder',
  );

  String terminalText(WidgetTester tester) {
    final term = tester
        .widget<TerminalView>(find.byType(TerminalView))
        .terminal;
    return [
      for (var i = 0; i < term.buffer.lines.length; i++)
        term.buffer.lines[i].getText(),
    ].join('\n');
  }

  void typeInTerminal(WidgetTester tester, String text) {
    tester
        .widget<TerminalView>(find.byType(TerminalView))
        .terminal
        .textInput(text);
  }

  // A detail tab in the WIDE shell. The desktop target is wider than
  // `kWideBreakpoint` (900 logical px, adaptive_shell.dart:25), so the app lays
  // out as a fleet list beside a tabbed detail pane and switches views in
  // place — it does NOT push terminal/review routes the way the phone shell
  // does. Hence tabs here and no back-navigation at all.
  //
  // Addressed by key, from adaptive_shell.dart:311 (`ValueKey('detail-tab-${tab.name}')`),
  // for two reasons: the enum spellings differ from the display labels
  // (detail=Overview, terminal=Agent, shell=Shell, review=Changes), and 'Shell'
  // is BOTH a tab label and the lifecycle bar's caption for its shell button, so
  // `find.text('Shell')` is ambiguous.
  Finder wsTab(String name) => find.byKey(ValueKey('detail-tab-$name'));

  Future<void> openTab(WidgetTester tester, String name) async {
    await tester.tap(wsTab(name));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 300));
  }

  // Tap a lifecycle action then confirm its dialog. Addressed by tooltip: the
  // lifecycle bar is the chrome's (`ChromeButtonBar`), which Mission Control
  // renders as IconButtons whose `tooltip` is the action's own label
  // (mission_control_chrome.dart:409 — `button.tooltip ?? button.label`), and
  // LCARS likewise uses the label as the tooltip. Not FilledButton: these have
  // not been FilledButtons since the chrome layer landed.
  Future<void> confirmAction(
    WidgetTester tester,
    String actionLabel,
    String confirmLabel,
  ) async {
    await tester.tap(find.byTooltip(actionLabel));
    await waitFor(tester, find.byType(AlertDialog));
    await tester.tap(
      find.descendant(
        of: find.byType(AlertDialog),
        matching: find.widgetWithText(FilledButton, confirmLabel),
      ),
    );
    await pumpUntil(
      tester,
      () => find.byType(AlertDialog).evaluate().isEmpty,
      reason: 'dialog "$confirmLabel" dismissed',
    );
  }

  testWidgets('full journey: connect, create, terminal + rejoin, review, '
      'lifecycle', (tester) async {
    // A surface big enough to hold the whole journey without scrolling.
    //
    // Width > kWideBreakpoint (900) keeps the desktop two-pane shell, which is
    // what this target renders. Height matters just as much: the Overview tab's
    // lifecycle bar is the last child of a ListView
    // (session_detail_page.dart:467), and a ListView builds lazily — on a short
    // pane the bar is absent from the widget tree entirely, not merely offscreen,
    // so `find.byTooltip('Kill')` finds nothing and no amount of retrying helps.
    // Sizing the surface to fit is far steadier than driving scrollUntilVisible
    // against a pane that rebuilds after every lifecycle call.
    tester.view.physicalSize = const Size(1600, 1400);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    // ---- connect (with auth) ----
    // One instance, as production has (main.dart): it owns the per-attach
    // control queues, so two would order their calls independently.
    final api = RustCommanderApi();
    await tester.pumpWidget(
      CommanderApp(
        api: api,
        fleet: FleetStore(api: api, listStore: InMemoryServerListStore()),
        // A throwaway store: the e2e run must not read or write the real
        // device preferences, and it exercises the default theme.
        theme: ThemeController(store: InMemoryPrefStore()),
      ),
    );
    // 'Connect' is the add-a-server button's label; it reads 'Save' only when
    // editing an existing entry (connection_page.dart:286).
    await waitFor(tester, find.text('Connect'));
    // Address the fields by their production keys. Positional lookup is brittle
    // when the connection form gains another input. Assign through the real
    // form controllers because the Linux integration driver intermittently
    // drops keyboard-driven `enterText` calls under xvfb, leaving the prefilled
    // default URL untouched. The Connect action still exercises the production
    // validation, persistence and authenticated API path below.
    final urlField = find.byKey(const Key('urlField'));
    final tokenField = find.byKey(const Key('tokenField'));
    tester.widget<TextFormField>(urlField).controller!.text = _baseUrl;
    tester.widget<TextFormField>(tokenField).controller!.text = _token;
    await tester.pump();
    // Guard: the URL must actually be the e2e server before we connect.
    expect(tester.widget<TextFormField>(urlField).controller?.text, _baseUrl);
    await tester.tap(find.widgetWithText(FilledButton, 'Connect'));
    await waitFor(tester, find.text('Fleet'));
    await waitFor(tester, find.text('No sessions')); // fresh hermetic server

    // ---- create a bash session ----
    await tester.tap(find.byIcon(Icons.add));
    await waitFor(tester, find.text('New session'));
    // The project is PICKED, not typed: the page offers a dropdown over the
    // projects registered on the server (e2e.sh registers $CC_E2E_REPO before the
    // app starts) and there is no free-text path field any more. Assert the
    // picker arrived with the repo preselected rather than selecting it — the
    // page preselects the first project, and there is exactly one here, so a tap
    // to open the menu would be theatre. That the preselection is the RIGHT repo
    // is proved downstream: the terminal writes a file into the session's
    // worktree and the review step then finds that file in the diff, neither of
    // which works if the session was branched from somewhere else.
    await waitFor(tester, find.widgetWithText(TextFormField, 'Title'));
    expect(
      find.widgetWithText(TextFormField, 'Project path (on the server)'),
      findsNothing,
      reason: 'the typed-path field was replaced by the project dropdown',
    );
    expect(
      find.text('Project'),
      findsOneWidget,
      reason:
          "the project picker's label; absent if the page fell back to "
          'its no-projects empty state',
    );
    await tester.enterText(
      find.widgetWithText(TextFormField, 'Title'),
      'e2e-journey',
    );
    // e2e.sh configures this shell as the default program. Once create-options
    // arrives, the page uses the alias dropdown instead of its fallback field.
    await waitFor(tester, find.text('E2E shell'));
    expect(
      find.widgetWithText(TextFormField, 'Program (optional)'),
      findsNothing,
    );
    await tester.tap(find.widgetWithText(FilledButton, 'Create session'));
    // Wait for the actual session row to render — not the create page's Title
    // field, whose 'e2e-journey' text transiently matches during the pop
    // transition. The list has exactly one row; the create page has none.
    //
    // ChromeListRow, not ListTile: the fleet list renders its rows through the
    // chrome's own row widget (session_list_page.dart's _recentRow/_groupedRow),
    // and the only ListTile left in that file builds the multi-server picker
    // sheet. Keying on ListTile silently matched nothing here.
    await waitFor(tester, find.byType(ChromeListRow));
    expect(find.widgetWithText(ChromeListRow, 'e2e-journey'), findsOneWidget);

    // ---- open detail ---- (tap the row itself, not its title Text, so the
    // row's InkWell reliably receives the gesture)
    await tester.tap(find.byType(ChromeListRow));
    await tester.pump();
    // Selecting a row fills the detail pane, so wait for its tab strip.
    await waitFor(tester, wsTab('review'));

    // ---- terminal: do work + write a file for the later review ----
    // The pane opens on the Agent tab; the paired shell is its own tab.
    await openTab(tester, 'shell');
    await waitFor(tester, find.byType(TerminalView));
    await pumpUntil(
      tester,
      () => find.textContaining('attached:').evaluate().isNotEmpty,
      reason: 'terminal attached',
    );
    typeInTerminal(tester, 'echo cc_e2e_marker\n');
    await pumpUntil(
      tester,
      () => terminalText(tester).contains('cc_e2e_marker'),
      reason: 'PTY echoes the marker',
    );
    typeInTerminal(tester, "printf 'hello e2e\\n' > cc_e2e_file.txt\n");
    typeInTerminal(tester, 'echo cc_wrote_file\n');
    await pumpUntil(
      tester,
      () => terminalText(tester).contains('cc_wrote_file'),
      reason: 'file-write command completes',
    );

    // ---- re-join: leave and re-attach; the pane replays prior output ----
    // Switching away tears the attach down and switching back joins the existing
    // tmux session, which is the behaviour under test — the wide shell does this
    // by tab, where the phone shell pops and re-pushes a route.
    await openTab(tester, 'detail');
    await waitFor(tester, wsTab('shell'));
    await openTab(tester, 'shell');
    await waitFor(tester, find.byType(TerminalView));
    await pumpUntil(
      tester,
      () => terminalText(tester).contains('cc_e2e_marker'),
      reason: 're-attach replays prior output (join existing session)',
    );

    // ---- review: the diff of the file written over the terminal renders, and
    // marking the file reviewed round-trips to the server. Comment create + apply
    // are covered deterministically elsewhere — by the L2 cdylib↔server test
    // `review_round_trip` (real server create_comment/apply_comments/
    // toggle_file_reviewed) and the L3 review widget test (real UI line-selection
    // → createComment/delete/apply). Driving the thin diff row's line-selection
    // gesture is unreliable under the live desktop test binding, so it's left to
    // those layers.
    await openTab(tester, 'review');
    await waitFor(tester, find.text('cc_e2e_file.txt'));
    // The filename appears twice in this layout: once as a row in the FILES
    // CHANGED tree and once as the diff card's own header (review_page.dart:841
    // and :1028). Only the card header carries the expand toggle, hence `.last`.
    // Guarded because the card may already be expanded — tapping an open card
    // would collapse it, and the wide layout does not use the phone flow's
    // collapsed-by-default file cards.
    if (find.text('hello e2e').evaluate().isEmpty) {
      await tester.tap(find.text('cc_e2e_file.txt').last);
      await tester.pump();
    }
    await waitFor(tester, find.text('hello e2e')); // the added line renders

    // mark the file reviewed — a real toggle_file_reviewed round-trip.
    // In this layout the control is the tree row's InkWell icon
    // (review_page.dart:951), not the phone flow's Checkbox: it shows
    // radio_button_unchecked until reviewed and check_circle after, so the icon
    // swap is itself the assertion that the round-trip landed.
    await tester.tap(find.byIcon(Icons.radio_button_unchecked));
    await waitFor(tester, find.byIcon(Icons.check_circle));
    // back to Overview, which is where the lifecycle bar lives (confirmAction
    // scrolls it into the tree — see there)
    await openTab(tester, 'detail');
    await waitFor(tester, find.text('Open Agent terminal'));

    // ---- lifecycle: kill → restart → delete ----
    await confirmAction(tester, 'Kill', 'Kill');
    await waitFor(tester, find.text('Session killed'));
    await confirmAction(tester, 'Restart', 'Restart');
    await waitFor(tester, find.text('Session restarted'));
    await confirmAction(tester, 'Delete', 'Delete');

    // delete clears the selection; the session is gone from the list.
    await waitFor(tester, find.text('Fleet'));
    await pumpUntil(
      tester,
      () => find.text('e2e-journey').evaluate().isEmpty,
      reason: 'deleted session disappears from the list',
    );
    await tester.pumpAndSettle();
  });
}
