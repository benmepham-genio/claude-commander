import 'dart:async';
import 'dart:typed_data';

import 'package:claude_commander_client/services/commander_api.dart';
import 'package:claude_commander_client/src/rust/api/diff.dart';
import 'package:claude_commander_client/src/rust/api/mirrors.dart';
import 'package:claude_commander_client/src/rust/api/review.dart';
import 'package:claude_commander_client/src/rust/api/simple.dart'
    show ScanResultDto;
import 'package:claude_commander_client/src/rust/api/workspace.dart';
import 'package:uuid/uuid.dart';

import 'fake_diff_layout.dart';
import 'fake_workspace_rules.dart';

/// One recorded call: the method name plus its positional arg values, so tests
/// can assert both that a method fired and what it was passed.
class RecordedCall {
  final String method;
  final Map<String, Object?> args;
  const RecordedCall(this.method, this.args);

  @override
  String toString() => '$method($args)';
}

/// A hand-rolled [CommanderApi] for widget tests — no bridge, no mocktail.
///
/// Set the public `...Response` fields for canned results (each has a sensible
/// default so a test only overrides what it exercises); set a `...Error` field
/// to make that method throw. Every call is appended to [calls]. `attachTerminal`
/// returns a stream fed by [terminalController], which the test drives with
/// [emit].
class FakeCommanderApi implements CommanderApi {
  final List<RecordedCall> calls = [];

  // --- canned responses --------------------------------------------------
  String connectServerResponse = 'fake-handle';

  /// When set, `connectServer` awaits this before returning — lets a test hold a
  /// connect in flight (e.g. to dispose the store mid-connect).
  Completer<void>? connectGate;

  /// When set, `disconnectServer` awaits this before returning — lets a test park
  /// a `reconnect` inside its teardown window (old handle released, new config
  /// not yet adopted) and run a second reconnect past it.
  Completer<void>? disconnectGate;
  Object? snapshotError;
  bool healthResponse = true;
  bool healthTmuxResponse = true;
  List<SessionInfo> listSessionsResponse = const [];
  AgentStatesSnapshotDto agentStatesResponse = const AgentStatesSnapshotDto(
    states: [],
    commanderRunning: false,
  );
  PreviewDataDto previewResponse = const PreviewDataDto(diffText: '');
  CreateOptions createOptionsResponse = const CreateOptions(
    defaultProgram: 'claude',
    programs: [],
    sections: [],
  );
  List<BranchInfo> listBranchesResponse = const [];
  List<SessionId> pendingCommentSessionsResponse = const [];
  ScanResultDto scanDirectoryResponse = const ScanResultDto(
    added: 0,
    skipped: 0,
  );
  List<GithubRepo> githubReposResponse = const [];

  /// What `startClone` answers. Defaults to a `Running` job, which is what the
  /// real route always returns (every outcome arrives via `cloneJob`).
  CloneJobDto startCloneResponse = CloneJobDto(
    id: CloneJobId(
      field0: UuidValue.fromString('cccccccc-2222-3333-4444-555555555555'),
    ),
    sourceLabel: 'acme/widget',
    dest: '/srv/projects/widget',
    status: const CloneStatusDto(
      kind: CloneStatusKind.running,
      message: '',
      isGitRepo: false,
    ),
  );

  /// What each `cloneJob` poll answers. A test that starts a clone should set a
  /// TERMINAL status here, otherwise the page's poll loop never stops and the
  /// test ends with a pending timer.
  CloneJobDto? cloneJobResponse;

  /// When set, `startClone` awaits this before returning — lets a test hold the
  /// page in the window between the confirm sheet closing and the job arriving,
  /// which is where a second tap could otherwise start a parallel clone.
  Completer<void>? startCloneGate;
  OperationStatusDto operationStatusResponse = OperationStatusDto(
    id: BigInt.zero,
    kind: OperationKind.cascade,
    outcome: const OperationOutcomeDto(
      kind: OperationOutcomeKind.succeeded,
      detail: '',
    ),
  );
  SessionDetail? getSessionDetailResponse;
  String createSessionResponse = 'new-session-id';
  String addProjectResponse = 'new-project-id';
  ReviewSnapshotDto? openReviewResponse;
  ReviewSnapshotDto? refreshReviewResponse;
  String createCommentResponse = 'new-comment-id';
  ApplyResult applyCommentsResponse = const ApplyResult(
    kind: ApplyResultKind.applied,
    driftedIds: [],
    count: 1,
  );
  bool toggleKeepAliveResponse = false;
  Uint8List fetchBlobResponse = Uint8List(0);

  /// Matches the real cap in `claude_commander_protocol::paste::MAX_IMAGE_BYTES`
  /// so size-limit tests exercise realistic numbers.
  int imageMaxBytesResponse = 10 * 1024 * 1024;

  /// Matches `claude_commander_protocol::ws::attach_dead_after()` so
  /// foreground-reconnect tests reason about the real heartbeat deadline.
  Duration attachDeadAfterResponse = const Duration(seconds: 60);

  /// When set, [pasteImage] throws this instead of succeeding — for the upload
  /// failure path (a rejected image, a dead server).
  Object? pasteImageError;

  /// Bytes handed to the most recent [pasteImage] call, so a test can assert the
  /// picked/pasted image reached the API unmodified.
  Uint8List? lastPastedImage;

  /// Per-path blob bodies, taking precedence over [fetchBlobResponse]. Lets a
  /// test tell two files' contents apart.
  final Map<String, Uint8List> fetchBlobResponses = {};

  /// Per-path gates: when a path has one, `fetchBlob` awaits it before
  /// returning, so a test can hold one file's fetch in flight while another
  /// completes.
  final Map<String, Completer<void>> fetchBlobGates = {};

  /// Overrides the synthesized layout `diffRows` returns, for a test that needs
  /// rows the fake layout does not produce (side by side, gaps, emphasis).
  DiffLayoutDto? diffRowsResponse;

  /// The session whose cascade is paused, surfaced in the snapshot.
  /// Null (the default) means no cascade is paused.
  SessionId? cascadePausedResponse;

  /// Explicit project list for the snapshot. When null (the default) projects
  /// are synthesized from [listSessionsResponse]; set it to render projects that
  /// have no sessions (e.g. the projects manager).
  List<ProjectInfoDto>? projectsResponse;

  /// The server's workspace definitions, as the snapshot reports them. The
  /// workspace mutations below write through to these (and to
  /// [projectWorkspaceTags]), like the real server would, so a test sees its
  /// own edit come back on the next snapshot.
  List<WorkspaceDef> workspacesResponse = const [];
  WorkspaceDef? mainWorkspaceResponse;
  String startupWorkspaceResponse = 'last';

  /// Per-project workspace tags keyed by project uuid, overriding whatever the
  /// project carries (`null` value = Main). Written by [setProjectWorkspace],
  /// [renameWorkspace] and [deleteWorkspace].
  final Map<String, String?> projectWorkspaceTags = {};

  /// When set, every workspace mutation throws it (and changes nothing) — the
  /// "one server refused" path of the eager fan-out.
  Object? workspaceMutationError;

  String? _tagOf(ProjectInfoDto p) {
    final id = p.id.field0.uuid;
    return projectWorkspaceTags.containsKey(id)
        ? projectWorkspaceTags[id]
        : p.workspace;
  }

  ProjectInfoDto _tagged(ProjectInfoDto p) => ProjectInfoDto(
    id: p.id,
    name: p.name,
    repoPath: p.repoPath,
    mainBranch: p.mainBranch,
    sessionIds: p.sessionIds,
    originUrl: p.originUrl,
    workspace: _tagOf(p),
  );

  /// The default snapshot echoes [listSessionsResponse] so a test that
  /// only sets sessions gets a coherent snapshot for free. It synthesizes one
  /// project per distinct session `projectId` (in first-seen order) so grouped
  /// views — which read `sessionsByProject` — have projects to group under, as a
  /// real server snapshot always would.
  SnapshotDto get snapshotResponse {
    final projects = <String, ProjectInfoDto>{};
    for (final s in listSessionsResponse) {
      projects.putIfAbsent(
        s.projectId.field0.uuid,
        () => ProjectInfoDto(
          id: s.projectId,
          name: s.projectName,
          repoPath: '/repo/${s.projectName}',
          mainBranch: 'main',
          sessionIds: const [],
        ),
      );
    }
    return SnapshotDto(
      projects: [
        for (final p in projectsResponse ?? projects.values) _tagged(p),
      ],
      sessions: listSessionsResponse,
      cascadePaused: cascadePausedResponse,
      pendingCommentSessions: const [],
      projectPull: const [],
      operations: const [],
      server: const ServerStatus(
        ghAvailable: true,
        tmuxOk: true,
        version: '0.0.0-test',
      ),
      workspaces: workspacesResponse,
      mainWorkspace: mainWorkspaceResponse,
      startupWorkspace: startupWorkspaceResponse,
    );
  }

  // --- error injection ---------------------------------------------------
  Object? healthError;
  Object? healthTmuxError;
  Object? listSessionsError;
  Object? getSessionDetailError;
  Object? createSessionError;
  Object? killSessionError;
  Object? restartSessionError;
  Object? deleteSessionError;
  Object? openReviewError;
  Object? createCommentError;
  Object? deleteCommentError;
  Object? applyCommentsError;
  Object? setFileReviewedError;
  Object? githubReposError;
  Object? startCloneError;

  // --- terminal ----------------------------------------------------------
  /// The controller behind the current [attachTerminal]. A test pushes events
  /// via [emit]. A fresh (single-subscription) controller is minted on each
  /// attach so a reconnect gets a clean stream.
  StreamController<TerminalEvent> terminalController =
      StreamController<TerminalEvent>();
  int attachTerminalCount = 0;

  void emit(TerminalEvent event) => terminalController.add(event);

  // --- feeds -------------------------------------------------------------
  /// Broadcast so a test can push events without a buffered listener, and so a
  /// reconnect (which re-listens) works against the same controller. A test
  /// drives them via [emitChange] / [emitConnection].
  final StreamController<BigInt> changeController =
      StreamController<BigInt>.broadcast();
  final StreamController<ConnectionStateDto> connectionController =
      StreamController<ConnectionStateDto>.broadcast();

  void emitChange([BigInt? generation]) =>
      changeController.add(generation ?? BigInt.one);

  void emitConnection(ConnectionStateDto state) =>
      connectionController.add(state);

  void _record(String method, [Map<String, Object?> args = const {}]) =>
      calls.add(RecordedCall(method, args));

  int countOf(String method) => calls.where((c) => c.method == method).length;

  RecordedCall? lastCall(String method) {
    for (final c in calls.reversed) {
      if (c.method == method) return c;
    }
    return null;
  }

  @override
  Future<String> connectServer({required String baseUrl, String? token}) async {
    _record('connectServer', {'baseUrl': baseUrl, 'token': token});
    if (connectGate != null) await connectGate!.future;
    return connectServerResponse;
  }

  @override
  Future<void> disconnectServer({required String handle}) async {
    _record('disconnectServer', {'handle': handle});
    if (disconnectGate != null) await disconnectGate!.future;
  }

  @override
  Future<bool> health({required String baseUrl}) async {
    _record('health', {'baseUrl': baseUrl});
    if (healthError != null) throw healthError!;
    return healthResponse;
  }

  @override
  Future<bool> healthTmux({
    required String baseUrl,
    required String token,
  }) async {
    _record('healthTmux', {'baseUrl': baseUrl, 'token': token});
    if (healthTmuxError != null) throw healthTmuxError!;
    return healthTmuxResponse;
  }

  /// When set, `snapshot` awaits this before returning — lets a test
  /// hold a refresh in flight (e.g. a slow server mid-fetch). Checked before
  /// [onSnapshot] runs, so a hook can arm the gate for the *next* fetch
  /// without parking its own.
  Completer<void>? snapshotGate;

  /// Called on every [snapshot] — the seam for a test that needs
  /// something to happen *while* a refresh is in flight (e.g. [emitChange],
  /// modelling a poller tick landing mid-fetch).
  void Function()? onSnapshot;

  @override
  Future<SnapshotDto> snapshot({required String handle}) async {
    _record('snapshot', {'handle': handle});
    if (snapshotGate != null) await snapshotGate!.future;
    onSnapshot?.call();
    if (snapshotError != null) throw snapshotError!;
    return snapshotResponse;
  }

  @override
  Future<AgentStatesSnapshotDto> agentStates({
    required String handle,
    required bool fresh,
  }) async {
    _record('agentStates', {'fresh': fresh});
    return agentStatesResponse;
  }

  @override
  Future<List<SessionInfo>> listSessions({
    required String handle,
    required bool includeStopped,
  }) async {
    _record('listSessions', {'includeStopped': includeStopped});
    if (listSessionsError != null) throw listSessionsError!;
    return listSessionsResponse;
  }

  @override
  Future<SessionDetail?> getSessionDetail({
    required String handle,
    required String query,
    int? lines,
  }) async {
    _record('getSessionDetail', {'query': query, 'lines': lines});
    if (getSessionDetailError != null) throw getSessionDetailError!;
    return getSessionDetailResponse;
  }

  @override
  Future<PreviewDataDto> sessionPreview({
    required String handle,
    required String id,
    int? lines,
  }) async {
    _record('sessionPreview', {'id': id, 'lines': lines});
    return previewResponse;
  }

  @override
  Future<PreviewDataDto> projectPreview({
    required String handle,
    required String id,
  }) async {
    _record('projectPreview', {'id': id});
    return previewResponse;
  }

  @override
  Future<String> branchDiff({
    required String handle,
    required String id,
  }) async {
    _record('branchDiff', {'id': id});
    return previewResponse.diffText;
  }

  @override
  Future<List<BranchInfo>> listBranches({
    required String handle,
    required String projectId,
    required bool fetch,
  }) async {
    _record('listBranches', {'projectId': projectId, 'fetch': fetch});
    return listBranchesResponse;
  }

  @override
  Future<CreateOptions> createOptions({required String handle}) async {
    _record('createOptions', {'handle': handle});
    return createOptionsResponse;
  }

  /// The program list passed to the most recent [setPrograms] call, so a test
  /// can assert the exact rows saved (not just the count).
  List<ProgramInfo>? lastSetPrograms;

  @override
  Future<void> setPrograms({
    required String handle,
    required List<ProgramInfo> programs,
  }) async {
    lastSetPrograms = programs;
    _record('setPrograms', {'programs': programs.length});
  }

  @override
  Future<List<SessionId>> pendingCommentSessions({
    required String handle,
  }) async {
    _record('pendingCommentSessions', {'handle': handle});
    return pendingCommentSessionsResponse;
  }

  @override
  Future<String> createSession({
    required String handle,
    required String projectPath,
    required String title,
    String? program,
    String? initialPrompt,
    String? effort,
    String? mode,
    String? baseBranch,
  }) async {
    _record('createSession', {
      'projectPath': projectPath,
      'title': title,
      'program': program,
      'initialPrompt': initialPrompt,
      'baseBranch': baseBranch,
    });
    if (createSessionError != null) throw createSessionError!;
    return createSessionResponse;
  }

  @override
  Future<void> killSession({required String handle, required String id}) async {
    _record('killSession', {'id': id});
    if (killSessionError != null) throw killSessionError!;
  }

  @override
  Future<void> restartSession({
    required String handle,
    required String id,
  }) async {
    _record('restartSession', {'id': id});
    if (restartSessionError != null) throw restartSessionError!;
  }

  @override
  Future<void> deleteSession({
    required String handle,
    required String id,
  }) async {
    _record('deleteSession', {'id': id});
    if (deleteSessionError != null) throw deleteSessionError!;
  }

  @override
  Future<void> renameSession({
    required String handle,
    required String id,
    required String title,
  }) async {
    _record('renameSession', {'id': id, 'title': title});
  }

  Object? setSectionError;

  @override
  Future<void> setSection({
    required String handle,
    required String id,
    String? section,
  }) async {
    _record('setSection', {'id': id, 'section': section});
    if (setSectionError != null) throw setSectionError!;
  }

  @override
  Future<void> markRead({required String handle, required String id}) async {
    _record('markRead', {'id': id});
  }

  @override
  Future<void> markUnread({
    required String handle,
    required List<String> ids,
  }) async {
    _record('markUnread', {'ids': ids});
  }

  @override
  Future<bool> toggleKeepAlive({
    required String handle,
    required String id,
  }) async {
    _record('toggleKeepAlive', {'id': id});
    return toggleKeepAliveResponse;
  }

  @override
  Future<void> pasteImage({
    required String handle,
    required String id,
    required Uint8List bytes,
  }) async {
    _record('pasteImage', {'id': id, 'bytes': bytes.length});
    lastPastedImage = bytes;
    if (pasteImageError != null) {
      throw pasteImageError!;
    }
  }

  @override
  Future<int> imageMaxBytes() async {
    _record('imageMaxBytes', {});
    return imageMaxBytesResponse;
  }

  @override
  Future<Duration> attachDeadAfter() async {
    _record('attachDeadAfter', {});
    return attachDeadAfterResponse;
  }

  @override
  Future<String> addProject({
    required String handle,
    required String path,
    String? workspace,
  }) async {
    _record('addProject', {'path': path, 'workspace': workspace});
    return addProjectResponse;
  }

  @override
  Future<String> ensureProject({
    required String handle,
    required String path,
    String? workspace,
  }) async {
    _record('ensureProject', {'path': path, 'workspace': workspace});
    // Idempotent like the route it stands in for: a path already in the snapshot
    // answers with that project's id. A fake that always returned a fresh id
    // would let a caller that used the non-idempotent `addProject` pass a test
    // about not duplicating.
    for (final p in snapshotResponse.projects) {
      if (p.repoPath == path) return p.id.field0.uuid;
    }
    return addProjectResponse;
  }

  @override
  Future<void> setProjectWorkspace({
    required String handle,
    required String projectId,
    String? workspace,
  }) async {
    _record('setProjectWorkspace', {
      'handle': handle,
      'projectId': projectId,
      'workspace': workspace,
    });
    if (workspaceMutationError != null) throw workspaceMutationError!;
    projectWorkspaceTags[projectId] = workspace;
    // The server defines a workspace it is asked to move a project into.
    if (workspace != null &&
        !workspacesResponse.any((w) => w.name == workspace)) {
      workspacesResponse = [
        ...workspacesResponse,
        WorkspaceDef(name: workspace),
      ];
    }
  }

  @override
  Future<void> setWorkspaces({
    required String handle,
    required SetWorkspacesRequestDto request,
  }) async {
    _record('setWorkspaces', {'handle': handle, 'request': request});
    if (workspaceMutationError != null) throw workspaceMutationError!;
    workspacesResponse = request.workspaces;
    if (request.main != null) mainWorkspaceResponse = request.main;
    if (request.startupWorkspace != null) {
      startupWorkspaceResponse = request.startupWorkspace!;
    }
  }

  /// Every project in the current snapshot currently tagged [name].
  Iterable<ProjectInfoDto> _projectsTagged(String name) =>
      snapshotResponse.projects.where((p) => p.workspace == name);

  @override
  Future<void> renameWorkspace({
    required String handle,
    required String from,
    required String to,
  }) async {
    _record('renameWorkspace', {'handle': handle, 'from': from, 'to': to});
    if (workspaceMutationError != null) throw workspaceMutationError!;
    for (final p in _projectsTagged(from).toList()) {
      projectWorkspaceTags[p.id.field0.uuid] = to;
    }
    workspacesResponse = [
      for (final w in workspacesResponse)
        w.name == from ? WorkspaceDef(name: to) : w,
    ];
    if (startupWorkspaceResponse == from) startupWorkspaceResponse = to;
  }

  @override
  Future<void> deleteWorkspace({
    required String handle,
    required String name,
  }) async {
    _record('deleteWorkspace', {'handle': handle, 'name': name});
    if (workspaceMutationError != null) throw workspaceMutationError!;
    for (final p in _projectsTagged(name).toList()) {
      projectWorkspaceTags[p.id.field0.uuid] = null;
    }
    workspacesResponse = [
      for (final w in workspacesResponse)
        if (w.name != name) w,
    ];
    if (startupWorkspaceResponse == name) startupWorkspaceResponse = 'main';
  }

  @override
  List<MergedWorkspace> mergeWorkspaces(List<WorkspaceSourceDto> sources) =>
      fakeMergeWorkspaces(sources);

  @override
  String? resolveStartupWorkspace({
    required String startup,
    String? last,
    required List<MergedWorkspace> workspaces,
  }) => fakeResolveStartupWorkspace(
    startup: startup,
    last: last,
    workspaces: workspaces,
  );

  @override
  String? workspaceNameError(String raw) => fakeWorkspaceNameError(raw);

  @override
  bool workspaceNameTaken(
    List<MergedWorkspace> workspaces,
    String name, {
    MergedWorkspace? except,
  }) => fakeWorkspaceNameTaken(workspaces, name, except: except);

  @override
  List<WorkspaceDef> definitionsForServer({
    required List<WorkspaceDef> wanted,
    required List<WorkspaceDef> own,
    String? mainLabel,
  }) =>
      fakeDefinitionsForServer(wanted: wanted, own: own, mainLabel: mainLabel);

  @override
  String? workspaceLabelError(String raw) => fakeWorkspaceLabelError(raw);

  @override
  Future<void> removeProject({
    required String handle,
    required String id,
  }) async {
    _record('removeProject', {'id': id});
  }

  @override
  Future<ScanResultDto> scanDirectory({
    required String handle,
    required String path,
    String? workspace,
  }) async {
    _record('scanDirectory', {'path': path, 'workspace': workspace});
    return scanDirectoryResponse;
  }

  @override
  Future<List<GithubRepo>> githubRepos({required String handle}) async {
    _record('githubRepos');
    if (githubReposError != null) throw githubReposError!;
    return githubReposResponse;
  }

  @override
  Future<CloneJobDto> startClone({
    required String handle,
    required CloneRequestDto request,
  }) async {
    _record('startClone', {'request': request});
    if (startCloneGate != null) await startCloneGate!.future;
    if (startCloneError != null) throw startCloneError!;
    return startCloneResponse;
  }

  @override
  Future<CloneJobDto?> cloneJob({
    required String handle,
    required CloneJobId id,
  }) async {
    _record('cloneJob', {'id': id});
    return cloneJobResponse;
  }

  /// Stands in for the bridge's `canonical_repo_slug`, which is a Rust function
  /// in `claude_commander_protocol::github` and unreachable from a widget test.
  ///
  /// **This is a test double, not a second implementation to depend on** — no
  /// `lib/` code may canonicalise slugs itself; it must call the seam. What this
  /// pins is the *contract* the page codes against, and the two halves that
  /// matter for the picker's badge:
  ///
  /// - the same repo spelled `https://…/o/r.git`, `ssh://git@…/o/r` and
  ///   `git@host:o/r.git` reduces to one identity, so a raw string comparison in
  ///   the page fails the badge tests;
  /// - a source with no GitHub identity answers **null**, and null is not an
  ///   identity — a page that lets two nulls compare equal badges every row.
  ///
  /// Deliberately narrower than the real rule (no IPv6 authorities, no port
  /// handling); the Rust side owns those and has its own tests.
  @override
  Future<String?> canonicalRepoSlug({required String url}) async {
    final source = url.trim();
    final String host;
    final String path;
    final scheme = RegExp(r'^([A-Za-z][A-Za-z0-9+.-]*)://').firstMatch(source);
    if (scheme != null) {
      // A `file://` URL is a local checkout wearing a URL's clothes.
      if (scheme.group(1)!.toLowerCase() == 'file') return null;
      final rest = source.substring(scheme.end);
      final slash = rest.indexOf('/');
      if (slash < 0) return null;
      host = _hostOf(rest.substring(0, slash));
      path = rest.substring(slash + 1);
    } else if (source.contains(':') && !source.startsWith('/')) {
      // scp form: `[user@]host:path`.
      final colon = source.indexOf(':');
      host = _hostOf(source.substring(0, colon));
      path = source.substring(colon + 1);
    } else {
      return null; // a local path has no GitHub identity
    }
    if (host.isEmpty) return null;
    // Lower-case BEFORE stripping, as the real rule does, so `repo.GIT` and
    // `repo.git` cannot become two identities for one repo.
    var p = path.toLowerCase();
    p = _trim(p, '/');
    if (p.endsWith('.git')) p = p.substring(0, p.length - 4);
    while (p.endsWith('/')) {
      p = p.substring(0, p.length - 1);
    }
    if (p.isEmpty) return null;
    return '${host.toLowerCase()}/$p';
  }

  /// The authority's host, dropping any `user:token@` userinfo. Split on the
  /// LAST `@` so an `@` inside a token can't be mistaken for the delimiter.
  static String _hostOf(String authority) {
    final at = authority.lastIndexOf('@');
    return at < 0 ? authority : authority.substring(at + 1);
  }

  static String _trim(String s, String char) {
    var out = s;
    while (out.startsWith(char)) {
      out = out.substring(1);
    }
    while (out.endsWith(char)) {
      out = out.substring(0, out.length - 1);
    }
    return out;
  }

  @override
  Future<OperationStatusDto> cascadeMerge({
    required String handle,
    required String id,
  }) async {
    _record('cascadeMerge', {'id': id});
    return operationStatusResponse;
  }

  @override
  Future<OperationStatusDto> pushStack({
    required String handle,
    required String id,
  }) async {
    _record('pushStack', {'id': id});
    return operationStatusResponse;
  }

  @override
  Future<OperationStatusDto> cascadeResume({required String handle}) async {
    _record('cascadeResume', {'handle': handle});
    return operationStatusResponse;
  }

  @override
  Future<void> cascadeAbandon({required String handle}) async {
    _record('cascadeAbandon', {'handle': handle});
  }

  @override
  Future<void> requestPrRefresh({required String handle}) async {
    _record('requestPrRefresh', {'handle': handle});
  }

  @override
  Future<ReviewSnapshotDto> openReview({
    required String handle,
    required String sessionId,
  }) async {
    _record('openReview', {'sessionId': sessionId});
    if (openReviewError != null) throw openReviewError!;
    final snap = openReviewResponse;
    if (snap == null) {
      throw StateError('openReviewResponse not set on FakeCommanderApi');
    }
    return snap;
  }

  @override
  Future<ReviewSnapshotDto?> refreshReview({
    required String handle,
    required String sessionId,
    required String prevHash,
  }) async {
    _record('refreshReview', {'sessionId': sessionId, 'prevHash': prevHash});
    return refreshReviewResponse;
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
    _record('createComment', {
      'file': file,
      'side': side,
      'lineStart': lineStart,
      'lineEnd': lineEnd,
      'snippet': snippet,
      'comment': comment,
    });
    if (createCommentError != null) throw createCommentError!;
    return createCommentResponse;
  }

  @override
  Future<void> deleteComment({
    required String handle,
    required String sessionId,
    required String commentId,
  }) async {
    _record('deleteComment', {'commentId': commentId});
    if (deleteCommentError != null) throw deleteCommentError!;
  }

  @override
  Future<ApplyResult> applyComments({
    required String handle,
    required String sessionId,
  }) async {
    _record('applyComments', {'sessionId': sessionId});
    if (applyCommentsError != null) throw applyCommentsError!;
    return applyCommentsResponse;
  }

  @override
  Future<bool> setFileReviewed({
    required String handle,
    required String sessionId,
    required String displayPath,
    required bool reviewed,
  }) async {
    _record('setFileReviewed', {
      'displayPath': displayPath,
      'reviewed': reviewed,
    });
    if (setFileReviewedError != null) throw setFileReviewedError!;
    return reviewed;
  }

  @override
  Future<Uint8List> fetchBlob({
    required String handle,
    required String sessionId,
    required String side,
    required String path,
  }) async {
    _record('fetchBlob', {'side': side, 'path': path});
    final gate = fetchBlobGates[path];
    if (gate != null) await gate.future;
    return fetchBlobResponses[path] ?? fetchBlobResponse;
  }

  @override
  Future<DiffLayoutDto> diffRows({
    required String? raw,
    required ReviewFileDto file,
    required DiffLayoutMode mode,
    String? fileText,
    List<DiffExpansion> expansions = const [],
  }) async {
    _record('diffRows', {
      'file': file.displayPath,
      'mode': mode,
      'hasText': fileText != null,
      'text': fileText,
      'expansions': expansions.length,
    });
    return diffRowsResponse ?? fakeDiffLayout(file);
  }

  @override
  Stream<TerminalEvent> attachTerminal({
    required String handle,
    required String attachId,
    required String sessionId,
    required AttachKind kind,
    required int cols,
    required int rows,
  }) {
    attachTerminalCount++;
    _record('attachTerminal', {
      'handle': handle,
      'attachId': attachId,
      'sessionId': sessionId,
      'kind': kind,
      'cols': cols,
      'rows': rows,
    });
    // Each attach gets a fresh single-subscription controller: the page cancels
    // its old subscription on reconnect, and a new listen needs a clean stream.
    terminalController = StreamController<TerminalEvent>();
    return terminalController.stream;
  }

  @override
  Future<void> terminalSendInput({
    required String attachId,
    required List<int> bytes,
  }) async {
    _record('terminalSendInput', {'attachId': attachId, 'bytes': bytes});
  }

  @override
  Future<void> terminalResize({
    required String attachId,
    required int cols,
    required int rows,
  }) async {
    _record('terminalResize', {
      'attachId': attachId,
      'cols': cols,
      'rows': rows,
    });
  }

  @override
  Future<void> terminalDetach({required String attachId}) async {
    _record('terminalDetach', {'attachId': attachId});
  }

  @override
  Stream<BigInt> changeFeed({required String handle}) {
    _record('changeFeed', {'handle': handle});
    return changeController.stream;
  }

  @override
  Stream<ConnectionStateDto> connectionFeed({required String handle}) {
    _record('connectionFeed', {'handle': handle});
    return connectionController.stream;
  }
}
