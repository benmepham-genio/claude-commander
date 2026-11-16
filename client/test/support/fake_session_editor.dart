import 'dart:convert';
import 'package:claude_commander_client/services/session_editor.dart';
import 'package:claude_commander_client/src/rust/api/mirrors.dart';

/// Canned Rust projections: widget tests check rendering/dispatch, while the
/// actual decisions are tested in viewmodel::session_editor.
class FakeSessionEditorBridge implements SessionEditorBridge {
  Map<String, dynamic> modelValue = {
    'original': {
      'title': 'Old',
      'program': 'claude',
      'section': null,
      'keep_alive': false,
      'base': 'project',
    },
    'live_program': 'claude',
    'programs': [
      {'label': 'Claude alias', 'command': 'claude'},
      {'label': 'Codex alias', 'command': 'codex'},
    ],
    'sections': ['Review'],
    'bases': [
      {'id': 'project', 'label': 'Project base', 'parent_session_id': null},
    ],
  };
  Map<String, dynamic> planValue = {
    'edit': {'title': 'New name', 'restart': false},
    'requires_restart': false,
  };
  Map<String, dynamic>? lastDraft;
  @override
  String model(
    SessionInfo session,
    List<SessionInfo> peers,
    String? mainBranch,
    CreateOptions? options,
  ) => jsonEncode(modelValue);
  @override
  String plan(String modelJson, String draftJson) {
    lastDraft = jsonDecode(draftJson) as Map<String, dynamic>;
    return jsonEncode(planValue);
  }
}
