// Renders the README's client screenshots from the REAL app driven against a
// REAL, hermetic claude-commander-server. Launch via docs/tool/capture-client.sh,
// which seeds the demo state (see docs/tool/fixture.sh) and passes the
// server's address/token plus an output directory through --dart-define.
//
// Not a test of behaviour: every expectation here exists so a capture can never
// silently write a half-rendered frame. If a wait times out, no image is
// produced and the capture script fails loudly.
//
// Capture mechanism: the app is pumped inside a RepaintBoundary and rasterised
// with `RenderRepaintBoundary.toImage`. `IntegrationTestWidgetsFlutterBinding
// .takeScreenshot` is not usable here — on desktop it needs a `flutter drive`
// host to receive the bytes, whereas this test writes them itself via dart:io.
//
// The layouts are produced by resizing the *view*, not by two different apps:
// `AdaptiveShell` picks the phone flow or the desktop rail+detail off the
// incoming constraints, so setting `tester.view.physicalSize` renders whichever
// one the README needs at a realistic device size.

import 'dart:io';
import 'dart:ui' as ui;

import 'package:claude_commander_client/main.dart';
import 'package:claude_commander_client/pages/edit_session_dialog.dart';
import 'package:claude_commander_client/pages/phone_shell.dart';
import 'package:claude_commander_client/server_config.dart';
import 'package:claude_commander_client/services/commander_api.dart';
import 'package:claude_commander_client/services/pref_store.dart';
import 'package:claude_commander_client/src/rust/frb_generated.dart';
import 'package:claude_commander_client/state/fleet_store.dart';
import 'package:claude_commander_client/theme/theme_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:xterm/xterm.dart';

const _baseUrl = String.fromEnvironment('CC_E2E_BASE_URL');
const _token = String.fromEnvironment('CC_E2E_TOKEN');
const _shotDir = String.fromEnvironment('CC_SHOT_DIR');
const _editOnly = bool.fromEnvironment('CC_SHOT_EDIT_ONLY');

/// The session the screenshots focus on. Seeded by the fixture with the demo
/// agent in its "working" state, so the terminal has live-looking output.
const _focusSession = 'Fix connection pool leak';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  final shotKey = GlobalKey();

  // Capturing needs a seeded server, so this file is inert unless the capture
  // script supplied one — that way running the whole integration_test/ directory
  // by hand doesn't fail on a missing fixture.
  final configured =
      _baseUrl.isNotEmpty && _token.isNotEmpty && _shotDir.isNotEmpty;

  setUpAll(() async {
    if (!configured) return;
    await RustLib.init();
  });

  // Pump frames until [cond] holds or [timeout] (real time) elapses. Neither
  // `pumpAndSettle` nor a plain `pump` works on these pages: the store polls and
  // the terminal streams, so there is always another frame scheduled, and real
  // time has to advance for the network and PTY I/O to complete.
  Future<void> pumpUntil(
    WidgetTester tester,
    bool Function() cond, {
    Duration timeout = const Duration(seconds: 30),
    String reason = 'condition',
  }) async {
    final deadline = DateTime.now().add(timeout);
    while (DateTime.now().isBefore(deadline)) {
      if (cond()) return;
      await tester.pump(const Duration(milliseconds: 120));
    }
    if (!cond()) {
      // Dump what *is* onscreen: a capture that stalls is nearly always the app
      // sitting on a different surface than expected, and the visible labels say
      // which one.
      final visible = tester
          .widgetList<Text>(find.byType(Text))
          .map((t) => t.data)
          .whereType<String>()
          .toList();
      throw TestFailure('pumpUntil timed out: $reason\nvisible text: $visible');
    }
  }

  Future<void> waitFor(WidgetTester tester, Finder finder) => pumpUntil(
    tester,
    () => finder.evaluate().isNotEmpty,
    reason: 'finding $finder',
  );

  /// Wait until the attached terminal has painted a screen's worth of the demo
  /// agent's transcript. The glyphs live on a canvas, so this reads the xterm
  /// buffer rather than looking for widgets — and dumps that buffer on failure,
  /// which is the only way to tell "never attached" from "attached but blank".
  Future<void> waitForTerminalOutput(WidgetTester tester) async {
    String text() =>
        _terminalText(tester).replaceAll(RegExp(r'\s+'), ' ').trim();
    final deadline = DateTime.now().add(const Duration(seconds: 30));
    while (DateTime.now().isBefore(deadline)) {
      if (text().length > 120) return;
      await tester.pump(const Duration(milliseconds: 120));
    }
    throw TestFailure(
      'terminal never streamed the agent pane. buffer was:\n'
      '${_terminalText(tester)}',
    );
  }

  /// Tap a session row by its title. The title `Text` sits inside the row's
  /// `InkWell`, but tapping the text itself can land on a descendant that
  /// swallows the gesture — so drive the row's own tappable ancestor.
  Future<void> tapRow(WidgetTester tester, String title) async {
    // Match on behaviour rather than a concrete type: Mission Control's rows are
    // InkWells, LCARS's are GestureDetectors, and a row that is neither is a bug
    // worth failing on rather than papering over.
    final tappable = find.ancestor(
      of: find.text(title),
      matching: find.byWidgetPredicate(
        (w) => w is InkResponse || w is GestureDetector,
      ),
    );
    expect(tappable, findsWidgets, reason: 'row "$title" is tappable');
    await tester.tap(tappable.first);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 500));
  }

  /// Let animations, the first data fetch and any in-flight paint land, so the
  /// captured frame is the settled one rather than a transition.
  Future<void> settle(WidgetTester tester) async {
    for (var i = 0; i < 12; i++) {
      await tester.pump(const Duration(milliseconds: 100));
    }
  }

  Future<void> shoot(WidgetTester tester, String name) async {
    await settle(tester);
    final boundary =
        shotKey.currentContext!.findRenderObject()! as RenderRepaintBoundary;
    final image = await boundary.toImage(
      pixelRatio: tester.view.devicePixelRatio,
    );
    final png = await image.toByteData(format: ui.ImageByteFormat.png);
    image.dispose();
    final file = File('$_shotDir/$name.png');
    await file.writeAsBytes(png!.buffer.asUint8List(), flush: true);
    expect(await file.length(), greaterThan(0));
    // ignore: avoid_print — the capture script echoes this back to the operator.
    print('screenshot: wrote ${file.path}');
  }

  /// Size the view in *logical* pixels at a realistic device pixel ratio, so
  /// text and hairlines rasterise the way they do on the real device rather than
  /// at the test's default 1:1.
  void useDevice(WidgetTester tester, Size logical, double dpr) {
    tester.view.devicePixelRatio = dpr;
    tester.view.physicalSize = Size(logical.width * dpr, logical.height * dpr);
  }

  testWidgets('capture phone and desktop screenshots', (tester) async {
    addTearDown(tester.view.reset);

    // Phone first: a 6.1" class viewport.
    useDevice(tester, const Size(393, 852), 3);

    // The server is seeded rather than typed into the connect form: the
    // screenshots are of the fleet, and a throwaway in-memory list store keeps
    // the run from touching the real device's secure storage or preferences.
    final api = RustCommanderApi();
    // A throwaway prefs store: the run must not read or write the real device's
    // theme choice, and it starts every capture on the default theme.
    final theme = ThemeController(store: InMemoryPrefStore());
    final fleet = FleetStore(
      api: api,
      listStore: InMemoryServerListStore(const [
        ServerConfig(
          id: 'demo',
          name: 'workstation',
          baseUrl: _baseUrl,
          token: _token,
        ),
      ]),
    );
    await fleet.loadAndConnectAll();

    await tester.pumpWidget(
      RepaintBoundary(
        key: shotKey,
        child: CommanderApp(api: api, fleet: fleet, theme: theme),
      ),
    );

    // ---- phone: the fleet list ----
    await waitFor(tester, find.text(_focusSession));
    // Every seeded session present means the snapshot fetch finished, so the
    // capture can't catch a half-populated list.
    for (final title in const [
      'Add rate limiting',
      'Refactor auth module',
      'Migrate to sqlx',
    ]) {
      await waitFor(tester, find.text(title));
    }
    // …and wait for the *live* agent states to land, so the fleet shows the mix
    // the fixture seeded (working / waiting / unread) rather than ten grey dots
    // from before the server's first state poll.
    await pumpUntil(
      tester,
      () =>
          find.text('working').evaluate().isNotEmpty &&
          find.text('waiting').evaluate().isNotEmpty,
      reason: 'live agent states arrived',
    );
    if (!_editOnly) await shoot(tester, 'client-sessions');

    // ---- phone: the agent terminal ----
    await tapRow(tester, _focusSession);

    // ---- phone: edit settings and the fresh-restart warning ----
    await tester.tap(find.byTooltip('Manage session'));
    await waitFor(tester, find.text('Edit session'));
    await tester.tap(find.text('Edit session'));
    await waitFor(tester, find.byType(EditSessionDialog));
    await shoot(tester, 'client-edit-session');
    final program = find.widgetWithText(
      DropdownButtonFormField<String>,
      'Program',
    );
    await tester.tap(program);
    await settle(tester);
    await tester.tap(find.text('Codex').last);
    await settle(tester);
    await tester.tap(find.widgetWithText(FilledButton, 'Save'));
    await waitFor(tester, find.text('Restart session?'));
    await shoot(tester, 'client-edit-session-warning');
    await tester.tap(find.widgetWithText(TextButton, 'Back'));
    await settle(tester);
    await waitFor(tester, find.widgetWithText(TextButton, 'Cancel'));
    await tester.tap(find.widgetWithText(TextButton, 'Cancel'));
    await settle(tester);
    expect(find.byType(EditSessionDialog), findsNothing);
    if (_editOnly) return;
    // The detail page's hero button and its pane snapshot's "Live" button both
    // open the agent terminal; either will do, so match the icon rather than a
    // label. The hero comes first in paint order.
    await waitFor(tester, find.byIcon(Icons.terminal));
    await tester.tap(find.byIcon(Icons.terminal).first);
    await waitFor(tester, find.byType(TerminalView));
    await waitForTerminalOutput(tester);
    await shoot(tester, 'client-terminal');

    // ---- back to the fleet list ----
    // Back out of the pushed routes first. The phone flow is list → detail →
    // terminal, so there are two routes to pop, and both pushed pages carry the
    // session title in their app bar — popping until the fleet header is showing
    // is what keeps the next step's row finder unambiguous.
    for (var i = 0; i < 4 && find.text('Fleet').evaluate().isEmpty; i++) {
      Navigator.of(tester.element(find.byType(Scaffold).last)).pop();
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 600));
    }
    await waitFor(tester, find.text('Fleet'));

    // ---- phone: the LCARS theme ----
    // Switching themes is a live repaint driven by the controller the app already
    // holds — so set it here rather than walking the settings UI. LCARS re-inflates
    // the shells (different chrome, different widget types), which is why this
    // happens on the list, after the terminal attach has been popped.
    await theme.select(ThemeId.lcars);
    // LCARS renders row titles upper-cased, so the settled frame is the one where
    // *either* spelling is on screen — matching only the mixed-case one would wait
    // out the timeout on a list that is already correct.
    await pumpUntil(
      tester,
      () =>
          find.text(_focusSession.toUpperCase()).evaluate().isNotEmpty ||
          find.text(_focusSession).evaluate().isNotEmpty,
      reason: 'the fleet list re-rendered in LCARS',
    );
    await shoot(tester, 'client-lcars');
    // Back to the default for the desktop shot: the desktop layout is worth
    // showing in the theme most people run.
    await theme.select(ThemeId.missionControl);
    await waitFor(tester, find.text(_focusSession));

    // ---- desktop: rail + detail ----
    // Resizing the view re-runs AdaptiveShell's LayoutBuilder, which swaps the
    // phone flow for the wide layout. Wait for the swap explicitly: capturing a
    // phone shell stretched across a 1440px frame would look like the desktop
    // layout at a glance and be wrong.
    useDevice(tester, const Size(1440, 900), 2);
    await pumpUntil(
      tester,
      () => find.byType(PhoneShell).evaluate().isEmpty,
      reason: 'wide layout replaced the phone shell',
    );
    await waitFor(tester, find.text(_focusSession));
    // Selecting a session fills the detail pane beside the list. The Agent tab is
    // the one worth showing — go there by name, and fall back to the Overview
    // body's hero button if this chrome renders its tabs some other way.
    await tapRow(tester, _focusSession);
    if (find.text('Agent').evaluate().isNotEmpty) {
      await tester.tap(find.text('Agent'));
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 500));
    } else if (find.byIcon(Icons.terminal).evaluate().isNotEmpty) {
      await tester.tap(find.byIcon(Icons.terminal).first);
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 500));
    }
    await waitFor(tester, find.byType(TerminalView));
    await waitForTerminalOutput(tester);
    await shoot(tester, 'client-desktop');
  }, skip: !configured);
}

String _terminalText(WidgetTester tester) {
  final term = tester.widget<TerminalView>(find.byType(TerminalView)).terminal;
  return [
    for (var i = 0; i < term.buffer.lines.length; i++)
      term.buffer.lines[i].getText(),
  ].join('\n');
}
