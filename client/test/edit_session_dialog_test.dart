import 'package:claude_commander_client/pages/edit_session_dialog.dart';
import 'package:claude_commander_client/src/rust/api/mirrors.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'support/fixtures.dart';
import 'support/fake_session_editor.dart';

void main() {
  for (final restart in [false, true]) {
    testWidgets(
      'renders shared aliases and submits restart=$restart after warning',
      (tester) async {
        final bridge = FakeSessionEditorBridge();
        bridge.planValue = {
          'edit': {'program': 'codex', 'restart': false},
          'requires_restart': true,
        };
        SessionEdit? saved;
        await tester.pumpWidget(
          MaterialApp(
            home: Scaffold(
              body: Builder(
                builder: (context) => TextButton(
                  onPressed: () async {
                    saved = await showDialog<SessionEdit>(
                      context: context,
                      builder: (_) => EditSessionDialog(
                        session: sessionInfo(),
                        peers: const [],
                        bridge: bridge,
                        options: Future.value(
                          const CreateOptions(
                            defaultProgram: 'claude',
                            programs: [],
                            sections: [],
                          ),
                        ),
                      ),
                    );
                  },
                  child: const Text('Edit'),
                ),
              ),
            ),
          ),
        );
        await tester.tap(find.text('Edit'));
        await tester.pumpAndSettle();
        expect(find.text('Claude alias'), findsOneWidget);
        expect(find.widgetWithText(TextFormField, 'Program'), findsNothing);
        await tester.tap(
          find.widgetWithText(DropdownButtonFormField<String>, 'Program'),
        );
        await tester.pumpAndSettle();
        await tester.tap(find.text('Codex alias').last);
        await tester.pumpAndSettle();
        await tester.tap(find.widgetWithText(FilledButton, 'Save'));
        await tester.pumpAndSettle();
        expect(bridge.lastDraft!['program'], 'codex');
        expect(find.text('Restart session?'), findsOneWidget);
        expect(saved, isNull);
        await tester.tap(find.text('Back'));
        await tester.pumpAndSettle();
        expect(find.text('Codex alias'), findsOneWidget);
        await tester.tap(find.widgetWithText(FilledButton, 'Save'));
        await tester.pumpAndSettle();
        await tester.tap(
          find.text(
            restart ? 'Yes, restart and clear' : 'No, save for next restart',
          ),
        );
        await tester.pumpAndSettle();
        expect(saved!.patch, {'program': 'codex', 'restart': restart});
      },
    );
  }
}
