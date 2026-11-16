import 'dart:convert';
import '../src/rust/api/mirrors.dart';
import '../src/rust/api/session_editor.dart' as native;

/// Injection seam for widget tests, which do not load the native library.
SessionEditorBridge? debugSessionEditorBridge;

abstract class SessionEditorBridge {
  String model(
    SessionInfo session,
    List<SessionInfo> peers,
    String? mainBranch,
    CreateOptions? options,
  );
  String plan(String modelJson, String draftJson);
}

class NativeSessionEditorBridge implements SessionEditorBridge {
  const NativeSessionEditorBridge();
  @override
  String model(
    SessionInfo session,
    List<SessionInfo> peers,
    String? mainBranch,
    CreateOptions? options,
  ) => native.sessionEditorModel(
    session: session,
    peers: peers,
    mainBranch: mainBranch,
    options: options,
  );
  @override
  String plan(String modelJson, String draftJson) =>
      native.planSessionEdit(modelJson: modelJson, draftJson: draftJson);
}

/// A transport wrapper: missing and explicit null patch fields remain distinct.
class SessionEdit {
  final String patchJson;
  const SessionEdit(this.patchJson);
  Map<String, dynamic> get patch =>
      jsonDecode(patchJson) as Map<String, dynamic>;
}
