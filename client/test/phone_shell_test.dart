import 'dart:async';

import 'package:claude_commander_client/chrome/lcars/elbow.dart';
import 'package:claude_commander_client/pages/create_session_page.dart';
import 'package:claude_commander_client/pages/phone_shell.dart';
import 'package:claude_commander_client/pages/session_detail_page.dart';
import 'package:claude_commander_client/pages/settings_page.dart';
import 'package:claude_commander_client/services/pref_store.dart';
import 'package:claude_commander_client/state/commander_store.dart';
import 'package:claude_commander_client/state/commander_store_scope.dart';
import 'package:claude_commander_client/state/fleet_store.dart';
import 'package:claude_commander_client/theme/theme_controller.dart';
import 'package:claude_commander_client/theme/theme_data.dart';
import 'package:claude_commander_client/theme/tokens.dart';
import 'package:claude_commander_client/widgets/brand_mark.dart';
import 'package:claude_commander_client/window/window_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fake_commander_api.dart';
import 'support/fixtures.dart';
import 'support/golden.dart';
import 'support/ink.dart';
import 'support/insets.dart';

void main() {
  late FakeCommanderApi api;
  late CommanderStore store;
  late FleetStore fleet;

  setUp(() {
    api = FakeCommanderApi();
    store = CommanderStore(api: api, config: testConfig);
    fleet = FleetStore.withStores([store]);
  });

  tearDown(() => fleet.dispose());

  /// The shell under the scopes `main()` gives it. Themeless falls back to
  /// Mission Control tokens, which is how the shell's own tests pump it; passing
  /// [tokens] opts into LCARS. The theme and window scopes are here so the
  /// settings route can actually be pushed and built.
  ///
  /// [textScale] rescales through a `copyWith` on the inherited data rather than
  /// a fresh `MediaQueryData`, which would drop the surface size along with it.
  ///
  /// The whole tree sits under the [inkBoundary] repaint boundary so a test can
  /// measure painted pixels ([inkCentre]); it is a proxy box over the surface,
  /// so it changes no geometry for the tests that do not.
  Widget wrap({CommanderTokens? tokens, double? textScale}) => RepaintBoundary(
    key: inkBoundary,
    child: FleetScope(
      fleet: fleet,
      child: WindowScope(
        controller: null,
        child: ThemeScope(
          // Never the device's real preferences.
          controller: ThemeController(store: InMemoryPrefStore()),
          child: MaterialApp(
            theme: tokens == null ? null : themeDataFor(tokens),
            home: textScale == null
                ? const PhoneShell()
                : Builder(
                    builder: (context) => MediaQuery(
                      data: MediaQuery.of(
                        context,
                      ).copyWith(textScaler: TextScaler.linear(textScale)),
                      child: const PhoneShell(),
                    ),
                  ),
          ),
        ),
      ),
    ),
  );

  testWidgets('shows the Fleet header, both nav tabs, and the create FAB', (
    tester,
  ) async {
    api.listSessionsResponse = [sessionInfo(title: 'Alpha')];
    unawaited(store.connect());
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    // The branded Fleet header (BrandMark + title) is on the Fleet tab.
    expect(find.byType(BrandMark), findsOneWidget);
    expect(find.text('Fleet'), findsOneWidget);
    expect(find.text('Alpha'), findsOneWidget);

    // Both bottom-nav tabs and the centre FAB are present.
    expect(find.text('FLEET'), findsOneWidget);
    expect(find.text('ACTIVITY'), findsOneWidget);
    expect(find.byType(FloatingActionButton), findsOneWidget);
  });

  testWidgets('switching to the Activity tab does not throw', (tester) async {
    api.listSessionsResponse = [sessionInfo(title: 'Alpha')];
    unawaited(store.connect());
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    await tester.tap(find.text('ACTIVITY'));
    await tester.pumpAndSettle();

    // Both bodies are kept alive in the IndexedStack, so the Activity header is
    // in the tree; the tap just switches which is shown.
    expect(tester.takeException(), isNull);
    expect(find.text('Activity'), findsOneWidget);
  });

  testWidgets('tapping a session row pushes the detail route', (tester) async {
    api.listSessionsResponse = [sessionInfo(title: 'Alpha')];
    api.getSessionDetailResponse = sessionDetail();
    unawaited(store.connect());
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    await tester.tap(find.text('Alpha'));
    await tester.pumpAndSettle();

    expect(find.byType(SessionDetailPage), findsOneWidget);
  });

  testWidgets('the FAB pushes the create route', (tester) async {
    api.listSessionsResponse = const [];
    unawaited(store.connect());
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();

    await tester.tap(find.byType(FloatingActionButton));
    await tester.pumpAndSettle();

    expect(find.byType(CreateSessionPage), findsOneWidget);
  });

  /// Settings is a *shell* action, not a view one: it used to hang off the Fleet
  /// view's frame, which left it unreachable from the Activity tab and stacked a
  /// second bottom terminator above the footer in LCARS.
  group('settings in the footer', () {
    Future<void> pump(
      WidgetTester tester, {
      CommanderTokens? tokens,
      double? textScale,
    }) async {
      api.listSessionsResponse = [sessionInfo(title: 'Alpha')];
      unawaited(store.connect());
      await tester.pumpWidget(wrap(tokens: tokens, textScale: textScale));
      await tester.pumpAndSettle();
    }

    testWidgets('Mission Control puts it in the bottom bar', (tester) async {
      await pump(tester);

      // Not in the Fleet header, where the view used to carry it — the shell's
      // bar, so it is the same control on either tab.
      expect(
        find.descendant(
          of: find.byType(BottomAppBar),
          matching: find.byIcon(Icons.settings),
        ),
        findsOneWidget,
      );
    });

    /// A third labelled slot in the tab row, not a bare icon parked in the
    /// corner — the lone leading gear read as misplaced on a device.
    testWidgets('Mission Control makes it a labelled tab after the others', (
      tester,
    ) async {
      await pump(tester);

      final label = find.descendant(
        of: find.byType(BottomAppBar),
        matching: find.text('SETTINGS'),
      );
      expect(label, findsOneWidget);
      // Same label row as the nav tabs, and the last slot in it.
      final settings = tester.getCenter(label);
      final activity = tester.getCenter(find.text('ACTIVITY'));
      expect(settings.dy, moreOrLessEquals(activity.dy, epsilon: 0.5));
      expect(settings.dx, greaterThan(activity.dx));
      // Its glyph is centred over its label, as each tab's is.
      expect(
        tester.getCenter(find.byIcon(Icons.settings)).dx,
        moreOrLessEquals(settings.dx, epsilon: 0.5),
      );
    });

    testWidgets('Mission Control opens settings from its tab', (tester) async {
      await pump(tester);

      await tester.tap(find.text('SETTINGS'));
      await tester.pumpAndSettle();

      expect(find.byType(SettingsPage), findsOneWidget);
    });

    testWidgets('Mission Control keeps the notch centred under the FAB', (
      tester,
    ) async {
      await pump(tester);

      // The FAB is centred on the Scaffold, not on the bar's row, so the gap
      // the tabs leave for it must be centred on the screen however many
      // slots sit either side of it.
      final fab = tester.getCenter(find.byType(FloatingActionButton)).dx;
      final fleet = tester.getRect(find.text('FLEET'));
      final activity = tester.getRect(find.text('ACTIVITY'));
      final width = tester.getSize(find.byType(BottomAppBar)).width;
      expect(fab, moreOrLessEquals(width / 2, epsilon: 0.5));
      expect(fleet.right, lessThan(fab - 28));
      expect(activity.left, greaterThan(fab + 28));
    });

    /// The tabs are a glyph over a label, so their glyphs sit *above* the bar's
    /// own centre line. A lone icon button centred in the bar therefore lands
    /// half a label below them — 8.5dp at 1× — which reads as a dropped gear
    /// rather than a third slot in the same row.
    ///
    /// Box centres, not ink: what is being pinned is the slot's vertical
    /// structure, and this suite loads neither the mono face the glyph is drawn
    /// in nor (outside `pumpGolden`) MaterialIcons, so an ink centroid here
    /// would measure notdef boxes. `footer_nav_test.dart` carries the ink
    /// receipt that a Material `Icon` centres its glyph in its box, which is
    /// what makes the icon's box the right proxy for its glyph.
    testWidgets('Mission Control sits it on the tabs\' glyph row', (
      tester,
    ) async {
      await pump(tester);

      expect(
        tester.getRect(find.byIcon(Icons.settings)).center.dy,
        moreOrLessEquals(
          tester.getRect(find.text('▤')).center.dy,
          epsilon: 0.5,
        ),
      );
    });

    /// The offset between the two rows is a label's height plus its gap, which
    /// grows with the text scale — so an alignment written as a constant would
    /// pass the test above and drift here. 1.3× is the same scale the LCARS
    /// footer's wrap case uses.
    testWidgets('Mission Control holds that row at 1.3× text', (tester) async {
      await pump(tester, textScale: 1.3);

      expect(
        tester.getRect(find.byIcon(Icons.settings)).center.dy,
        moreOrLessEquals(
          tester.getRect(find.text('▤')).center.dy,
          epsilon: 0.5,
        ),
      );
    });

    testWidgets('LCARS makes it the leading block of the footer run', (
      tester,
    ) async {
      await pump(tester, tokens: lcarsTokens);

      final settings = tester.getRect(
        find.widgetWithText(ChromeElbow, 'SETTINGS'),
      );
      final rail = tester.getRect(find.widgetWithText(ChromeElbow, '47-A'));
      final fleet = tester.getRect(find.widgetWithText(ChromeElbow, 'FLEET'));

      // Directly under the rail: same left edge, same width.
      expect(settings.left, rail.left);
      expect(settings.width, lcarsTokens.railWidth);
      // And inline with the run it leads, rather than a block above it.
      expect(settings.center.dy, moreOrLessEquals(fleet.center.dy));
    });

    testWidgets('LCARS keeps the run on the screen edge at 1.3× text', (
      tester,
    ) async {
      // 'SETTINGS' fits its 62px block at 11px by a whisker, so any accessibility
      // scaling wraps it and `ChromeElbow` grows that block to fit two lines. It
      // is the run's only fixed-width block, so it is the only one that can grow
      // — and a centred `Row` would then lift every other block off the bottom of
      // the screen, on the one run whose premise is meeting that edge.
      await pump(tester, tokens: lcarsTokens, textScale: 1.3);

      final settings = tester.getRect(
        find.widgetWithText(ChromeElbow, 'SETTINGS'),
      );
      final fleet = tester.getRect(find.widgetWithText(ChromeElbow, 'FLEET'));
      final activity = tester.getRect(
        find.widgetWithText(ChromeElbow, 'ACTIVITY'),
      );

      expect(settings.height, greaterThan(fleet.height));
      expect(fleet.bottom, settings.bottom);
      expect(activity.bottom, settings.bottom);
    });

    testWidgets('LCARS opens settings from the Activity tab', (tester) async {
      await pump(tester, tokens: lcarsTokens);

      await tester.tap(find.widgetWithText(ChromeElbow, 'ACTIVITY'));
      await tester.pumpAndSettle();
      await tester.tap(find.widgetWithText(ChromeElbow, 'SETTINGS'));
      await tester.pumpAndSettle();

      expect(find.byType(SettingsPage), findsOneWidget);
    });
  });

  /// The footer run grows into the gesture inset while its labels stay put, so
  /// the coloured blocks meet the physical edge of the screen instead of ending
  /// in a black band above it.
  group('LCARS safe-area bleed', () {
    /// A footer label. Scoped to its block because the Fleet *view* also titles
    /// itself 'FLEET' (`session_list_page.dart:222`, uppercased by the rail), so
    /// a bare `find.text` matches two widgets on this shell.
    Finder footerLabel(String text) => find.descendant(
      of: find.byType(ChromeElbow),
      matching: find.text(text),
    );

    /// The block behind a footer label.
    Finder footerBlock(String text) => find.widgetWithText(ChromeElbow, text);

    /// The screen-space centre of a footer label.
    double labelCentre(WidgetTester tester, String text) =>
        tester.getRect(footerLabel(text)).center.dy;

    void seed() {
      api.listSessionsResponse = [sessionInfo(title: 'Alpha')];
      unawaited(store.connect());
    }

    Future<void> pumpLcars(WidgetTester tester) async {
      await tester.pumpWidget(wrap(tokens: lcarsTokens));
      await tester.pumpAndSettle();
    }

    testWidgets('the footer run reaches the physical bottom edge', (
      tester,
    ) async {
      seed();
      useInsets(tester, bottom: 48);
      await pumpLcars(tester);

      expect(
        tester.getRect(footerLabel('FLEET')).bottom,
        lessThan(surfaceHeight(tester) - 48),
        reason: 'the label itself must stay clear of the gesture strip',
      );
      // The block behind that label, however, must meet the edge.
      expect(
        tester.getRect(footerBlock('FLEET')).bottom,
        surfaceHeight(tester),
      );
    });

    // The load-bearing assertion, and it takes both halves. Edge-reaching alone
    // is also satisfied by "labels follow the fill down", which is the
    // treatment this design rejected, so only the shift pins "held relative to
    // the safe region" — but the shift alone is satisfied by the `SafeArea`
    // this replaces, which moves label *and* fill up together. The pair is
    // "the label moved off the edge and the fill did not".
    testWidgets('footer labels hold the safe region, shifting by the inset', (
      tester,
    ) async {
      seed();
      await pumpLcars(tester);
      final flat = labelCentre(tester, 'FLEET');
      final edge = tester.getRect(footerBlock('FLEET')).bottom;

      useInsets(tester, bottom: 48);
      await pumpLcars(tester);

      expect(labelCentre(tester, 'FLEET'), flat - 48);
      expect(
        tester.getRect(footerBlock('FLEET')).bottom,
        edge,
        reason: 'the fill must not retreat with the label',
      );
    });

    // The bled half of the ink-centroid pin. `footer_nav_test.dart` already
    // holds the unbled case (ink on the block's centre); bled, "centre" means
    // the centre of the *padded* box, which is the visible part of the block.
    testWidgets('the create glyph centres on the visible part when bled', (
      tester,
    ) async {
      // The shell's own tests do not load the app's faces, and an `Icon`
      // without one is a notdef square whose ink is centred on its box whatever
      // the padding — which would pass this on a glyph that was never drawn.
      await loadCommanderFonts();
      seed();
      useInsets(tester, bottom: 48);
      await pumpLcars(tester);
      expect(
        materialIconsLoaded,
        isTrue,
        reason: 'MaterialIcons did not load; this test cannot measure an icon',
      );

      final block = tester.getRect(
        find
            .ancestor(
              of: find.byIcon(Icons.add),
              matching: find.byType(ChromeElbow),
            )
            .first,
      );
      final centre = await inkCentre(tester, block, lcarsTokens.attention);

      expect(centre.dy, closeTo(block.top + (block.height - 48) / 2, 0.6));
    });

    // The bleed is a function of `padding`, which the platform collapses on any
    // edge the keyboard covers — so it disappears exactly when the footer is no
    // longer against the bezel. This pins *the app's response to a given
    // padding*; it does not assert the platform's collapse, which is
    // `FlutterView.padding`'s documented contract, not something a widget test
    // can hold. `terminal_page_test.dart:36` fakes the same collapse by hand.
    testWidgets('no bleed while the keyboard covers the bottom edge', (
      tester,
    ) async {
      seed();
      // Genuinely keyboard-up: `padding.bottom` is 0 (what the platform
      // collapses it to) while `viewPadding.bottom` still holds the inset —
      // see `useInsets`'s doc for why the two must differ here.
      useInsets(tester, viewBottom: 48);
      await pumpLcars(tester);

      expect(tester.getRect(footerBlock('FLEET')).height, 38);
    });

    // `view_rail_test.dart`'s top-band group pumps `LcarsBleedScope` by hand,
    // which pins that `buildViewRail` *consumes* the scope but not that
    // `buildShell` *publishes* it — nothing in this suite sets a top inset
    // through the shell at all. This is the other half: through the real
    // shell, with no scope wired in by the test.
    testWidgets('the view rail meets the physical top edge', (tester) async {
      seed();
      useInsets(tester, top: 24);
      await pumpLcars(tester);

      final id = find.widgetWithText(ChromeElbow, '47-A');
      expect(tester.getRect(id).top, 0);
      expect(tester.getSize(id).height, 74 + 24);
    });

    // The gutter is one uninterrupted gap from the physical top edge down, and
    // that is a deliberate trade rather than an oversight. It was filled across
    // the status-bar inset for a while, because on a Pixel 8a the system
    // clock's last digit sat exactly on the seam and the black column ran
    // through it — but a filled gutter leaves the cap's bottom-left corner
    // nothing to curve out of, so the band met the rail at a bare 90° angle.
    // The elbow is worth more than the notch; see `page_bleed_test.dart` for
    // the corner this buys.
    testWidgets('the rail/content gutter runs open through the top inset', (
      tester,
    ) async {
      seed();
      useInsets(tester, top: 24);
      await pumpLcars(tester);

      final rail = tester.getRect(find.widgetWithText(ChromeElbow, '47-A'));
      final gutterX = rail.right + 2;

      for (final y in [4.0, 12.0, 30.0]) {
        expect(
          await pixelAt(tester, Offset(gutterX, y)),
          lcarsTokens.canvas,
          reason: 'the gutter is filled at y=$y',
        );
      }
    });

    testWidgets('a horizontal inset is held, not bled', (tester) async {
      seed();
      useInsets(tester, left: 30);
      await pumpLcars(tester);

      // A cutout occludes; the rail must sit inboard of it rather than paint
      // under it.
      expect(tester.getRect(footerLabel('SETTINGS')).left, greaterThan(30));
    });

    // SETTINGS is the run's only fixed-width, bottom-aligned block (see the
    // 1.3× test above) and its bleed is threaded through a different path
    // than the other footer blocks' (`_navSettings` vs `_navBlock`), so
    // nothing above pins it directly. Without its own bleed, SETTINGS would
    // stay 38 tall while its run grows to 86 for the inset, leaving its label
    // a full inset lower than FLEET/ACTIVITY — inside the gesture strip.
    //
    // An inequality against the safe region, not a label-centre equality
    // against its neighbours: this suite does not load the Antonio font, so a
    // wrap-sensitive centre-alignment assertion would be fragile.
    testWidgets('the settings block holds its label off the gesture strip', (
      tester,
    ) async {
      seed();
      useInsets(tester, bottom: 48);
      await pumpLcars(tester);

      expect(
        tester.getRect(footerLabel('SETTINGS')).bottom,
        lessThan(surfaceHeight(tester) - 48),
      );
    });
  });

  // `buildPage`'s right edge is pinned by the `settings_lcars` golden. An
  // LCARS phone-shell golden is banned (`goldens/pages_golden_test.dart`, read
  // the comment there for why), so the other two surfaces that used to carry a
  // 10dp right margin — the footer run and the view rail's content column —
  // get behavioural pins here instead.
  group('LCARS runs flush to the right bezel', () {
    Future<void> pump(WidgetTester tester) async {
      api.listSessionsResponse = [sessionInfo(title: 'Alpha')];
      unawaited(store.connect());
      await tester.pumpWidget(wrap(tokens: lcarsTokens));
      await tester.pumpAndSettle();
    }

    testWidgets('the footer run\'s last block meets the right bezel', (
      tester,
    ) async {
      await pump(tester);

      // The run's slot order (`buildFooterNav`): SETTINGS leads, then FLEET,
      // the centre create action, then ACTIVITY — so ACTIVITY is the last
      // block, and its right edge is the run's own.
      expect(
        tester.getRect(find.widgetWithText(ChromeElbow, 'ACTIVITY')).right,
        surfaceWidth(tester),
      );
    });

    testWidgets('the view rail\'s content column meets the right bezel', (
      tester,
    ) async {
      await pump(tester);

      // The content column stretches (`crossAxisAlignment.stretch`) to fill
      // the `Expanded` `buildViewRail` gives it beside the rail, so its
      // nearest `Column` ancestor is the column itself. Anchored on the
      // count line rather than the 'FLEET' title: that title string also
      // occurs as the footer's own nav-block label, so `find.text('FLEET')`
      // is not unique on this shell, but the count line is.
      final content = find
          .ancestor(
            of: find.text('1 ACTIVE · 1 TOTAL · 1 SERVER'),
            matching: find.byType(Column),
          )
          .first;
      expect(tester.getRect(content).right, surfaceWidth(tester));
    });
  });
}
