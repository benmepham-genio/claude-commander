import 'dart:async';

import 'package:flutter/foundation.dart';

import '../server_config.dart';
import '../services/commander_api.dart';
import '../src/rust/api/mirrors.dart';
import '../src/rust/api/simple.dart' show ScanResultDto;

/// One project paired with the sessions that belong to it, in the snapshot's
/// project order. Used by the grouped session view.
class ProjectSessions {
  final ProjectInfoDto project;
  final List<SessionInfo> sessions;
  const ProjectSessions({required this.project, required this.sessions});
}

/// The single reactive source of truth for a connected server. It owns the
/// opaque server [handle] for its whole lifetime — [connect] acquires it,
/// [reconnect] swaps it (tearing down the old one first), and [dispose] releases
/// it — so a handle can never be abandoned in the cdylib registry.
///
/// State is refreshed off the poller's change feed rather than a wall-clock
/// timer: every generation bump re-fetches the snapshot and agent
/// states (mirroring the TUI), and every connection-feed event updates
/// [connection]. Widgets listen via `ListenableBuilder`.
class CommanderStore extends ChangeNotifier {
  CommanderStore({required CommanderApi api, required ServerConfig config})
    : _api = api,
      _config = config;

  final CommanderApi _api;
  ServerConfig _config;

  /// The seam, exposed so pages can construct sub-flows (terminal/review/create)
  /// that still take a raw [CommanderApi] + [handle].
  CommanderApi get api => _api;
  ServerConfig get config => _config;

  String? _handle;

  /// The live server handle, or null before the first [connect] completes / while
  /// a [reconnect] is in flight.
  String? get handle => _handle;

  /// Monotonic connect generation. Bumped at the start of every [connect] (and
  /// [reconnect]) so an earlier in-flight connect, on resuming after an await,
  /// can detect it has been superseded — release the handle it just acquired and
  /// bail rather than leaking it and orphaning its feeds. Closes the
  /// connect-vs-connect race (e.g. a double-tapped retry, or an edit while a
  /// slow connect is still parked). [reconnect] checks it for the same reason
  /// before adopting its config, so a superseded reconnect can't roll the store
  /// back to an older edit than the one on disk.
  int _connectEpoch = 0;

  SnapshotDto? _snapshot;
  SnapshotDto? get snapshot => _snapshot;

  final Map<String, AgentState> _agentStates = {};
  bool _commanderRunning = false;
  bool get commanderRunning => _commanderRunning;

  ConnectionStateDto _connection = const ConnectionStateDto(
    kind: ConnectionStateKind.connecting,
    reason: '',
  );
  ConnectionStateDto get connection => _connection;

  Object? _error;
  Object? get error => _error;

  bool _loading = false;
  bool get loading => _loading;

  StreamSubscription<BigInt>? _changeSub;
  StreamSubscription<ConnectionStateDto>? _connectionSub;

  /// The id of the terminal attach the UI currently holds open, if any. The
  /// terminal body registers it via [setActiveTerminalAttach] on attach and
  /// clears it on detach, so [reconnect] / [dispose] can tear it down before
  /// releasing the handle — belt-and-braces with the cdylib's own
  /// disconnect-time teardown (`disconnect_server` also aborts in-flight
  /// attaches for the handle).
  String? _activeTerminalAttachId;

  // Coalescing guards so overlapping change-feed ticks don't stack refetches: a
  // tick arriving mid-fetch sets [_refreshQueued] and one more fetch runs after.
  bool _refreshing = false;
  bool _refreshQueued = false;
  bool _disposed = false;

  SnapshotDto? _indexedSnapshot;
  final Map<ProjectId, String?> _projectWorkspaces = {};
  final Map<String, SessionInfo> _sessionsById = {};
  List<ProjectSessions> _groups = const [];
  final Map<String?, List<SessionInfo>> _workspaceSessions = {};

  void _ensureIndexes() {
    if (identical(_indexedSnapshot, _snapshot)) return;
    _indexedSnapshot = _snapshot;
    _projectWorkspaces.clear();
    _sessionsById.clear();
    _workspaceSessions.clear();
    final grouped = <ProjectId, List<SessionInfo>>{};
    for (final project in projects) {
      _projectWorkspaces[project.id] = project.workspace;
    }
    for (final session in sessions) {
      _sessionsById[session.id] = session;
      grouped.putIfAbsent(session.projectId, () => []).add(session);
      _workspaceSessions
          .putIfAbsent(_projectWorkspaces[session.projectId], () => [])
          .add(session);
    }
    _groups = [
      for (final project in projects)
        ProjectSessions(
          project: project,
          sessions: grouped[project.id] ?? const [],
        ),
    ];
  }

  // --- convenience getters the pages render from ---------------------------

  List<SessionInfo> get sessions => _snapshot?.sessions ?? const [];

  /// Sessions grouped under their project, in the snapshot's project order.
  List<ProjectSessions> get sessionsByProject {
    _ensureIndexes();
    return _groups;
  }

  List<OperationStatusDto> get operations => _snapshot?.operations ?? const [];

  List<SessionId> get pendingCommentSessions =>
      _snapshot?.pendingCommentSessions ?? const [];

  /// The session whose cascade is currently paused awaiting a decision, or null
  /// when no cascade is paused. Drives the resume/abandon banner.
  SessionId? get cascadePaused => _snapshot?.cascadePaused;

  /// The projects known to the server, in snapshot order.
  List<ProjectInfoDto> get projects => _snapshot?.projects ?? const [];

  // --- workspace scoping ---------------------------------------------------
  //
  // A workspace is a label on a project (`ProjectInfoDto.workspace`, null =
  // Main), and a session belongs to its project's workspace. Membership is plain
  // equality of the two names — `viewmodel::workspace::in_workspace` — so it is
  // compared here rather than bridged per row.

  /// The workspace [projectId] is tagged with, or null for Main — including for
  /// a project this snapshot does not know, which reads as Main like it does in
  /// `viewmodel::workspace::project_workspace`.
  String? workspaceOfProject(ProjectId projectId) {
    _ensureIndexes();
    return _projectWorkspaces[projectId];
  }

  /// The workspace a session belongs to (its project's), or null for Main.
  String? workspaceOfSession(SessionInfo session) =>
      workspaceOfProject(session.projectId);

  /// The projects in [workspace] (null = Main), in snapshot order.
  List<ProjectInfoDto> projectsIn(String? workspace) => [
    for (final p in projects)
      if (p.workspace == workspace) p,
  ];

  /// The sessions in [workspace] (null = Main), in snapshot order.
  List<SessionInfo> sessionsIn(String? workspace) {
    _ensureIndexes();
    return _workspaceSessions[workspace] ?? const [];
  }

  /// [sessionsByProject] limited to the projects in [workspace].
  List<ProjectSessions> sessionsByProjectIn(String? workspace) => [
    for (final g in sessionsByProject)
      if (g.project.workspace == workspace) g,
  ];

  /// The agent state for a session id (the [SessionInfo.id] string form), or
  /// [AgentState.unknown] if the snapshot has no entry for it.
  AgentState agentStateFor(String id) => _agentStates[id] ?? AgentState.unknown;

  /// The live [SessionInfo] for an id from the latest snapshot, or null if the
  /// session is no longer present (deleted/stopped-and-removed).
  SessionInfo? sessionById(String id) {
    _ensureIndexes();
    return _sessionsById[id];
  }

  // --- lifecycle ----------------------------------------------------------

  /// Acquire the handle, wire up the feeds, and load the first snapshot.
  Future<void> connect() async {
    final epoch = ++_connectEpoch;
    _loading = true;
    _error = null;
    if (!_disposed) notifyListeners();
    try {
      final h = await _api.connectServer(
        baseUrl: _config.baseUrl,
        token: _config.token,
      );
      // While connectServer was in flight the store may have been disposed
      // (server removed) or superseded by a newer connect/reconnect (double-tap,
      // edit). Release the freshly-acquired handle and bail rather than
      // subscribing feeds that would never be torn down.
      if (_disposed || epoch != _connectEpoch) {
        unawaited(_api.disconnectServer(handle: h));
        return;
      }
      _handle = h;
      _changeSub = _api
          .changeFeed(handle: h)
          .listen(_onChange, onError: (_) {});
      _connectionSub = _api
          .connectionFeed(handle: h)
          .listen(_onConnection, onError: (_) {});
      await _refresh();
    } catch (e) {
      if (epoch == _connectEpoch) _error = e;
    } finally {
      // Only the live connect owns the shared state; a superseded one must not
      // clobber the newer connect's loading flag / error.
      if (epoch == _connectEpoch) {
        _loading = false;
        if (!_disposed) notifyListeners();
      }
    }
  }

  /// Swap to a new server: tear down the old feeds, release the old handle
  /// (BEFORE opening the new one, so the cdylib registry never grows), then
  /// connect afresh. This is the fix for the reconnect leak — the store owns the
  /// handle so a settings change can't abandon it.
  Future<void> reconnect(ServerConfig next) async {
    // Supersede any in-flight connect immediately (before our own awaits) so it
    // releases its handle when it resumes instead of racing this reconnect.
    final epoch = ++_connectEpoch;
    // Claim the old handle synchronously too, for the same reason: a reconnect
    // that starts while this one is tearing down must see no handle to release
    // (this one owns it) and must not have the handle it later acquires nulled
    // out from under it here.
    final old = _handle;
    _handle = null;
    await _teardownSubs();
    if (old != null) {
      // Detach any open terminal before releasing the handle so the persistent
      // pane doesn't outlive its server.
      await _detachActiveTerminal();
      try {
        await _api.disconnectServer(handle: old);
      } catch (_) {
        // Best-effort: a failed disconnect must not block the new connection.
      }
    }
    // Releasing the old handle involves network calls (terminal detach,
    // disconnect), so a second server edit can land while we are parked in them.
    // If it did, it owns `_config` and the live connection now: adopting `next`
    // here would roll the config back to this (older) edit while the persisted
    // list holds the newer one, and our `connect()` would supersede its. The old
    // handle is already released above, so bailing leaks nothing.
    if (_disposed || epoch != _connectEpoch) return;
    _config = next;
    _snapshot = null;
    _agentStates.clear();
    _commanderRunning = false;
    _connection = const ConnectionStateDto(
      kind: ConnectionStateKind.connecting,
      reason: '',
    );
    if (!_disposed) notifyListeners();
    await connect();
  }

  /// Update the stored config (name/URL/token) synchronously, ahead of a
  /// [reconnect]. Lets the fleet persist the edited config immediately —
  /// `reconnect` only assigns `_config` after several awaits, so a concurrent
  /// save would otherwise write the pre-edit config back to disk.
  void applyConfig(ServerConfig config) {
    _config = config;
    if (!_disposed) notifyListeners();
  }

  /// Force a snapshot refetch (pull-to-refresh); a no-op reconnect if the handle
  /// was lost.
  Future<void> refresh() => _refresh();

  /// Retry after a connect/refresh failure: re-establish the handle if it was
  /// never acquired, else just refetch.
  Future<void> retry() => _handle == null ? connect() : _refresh();

  // --- mutations (thin wrappers; `_mutate` refetches the snapshot after each)

  Future<void> killSession(String id) =>
      _mutate(() => _api.killSession(handle: _requireHandle, id: id));

  Future<void> restartSession(String id) =>
      _mutate(() => _api.restartSession(handle: _requireHandle, id: id));

  Future<void> deleteSession(String id) =>
      _mutate(() => _api.deleteSession(handle: _requireHandle, id: id));

  Future<String?> editSession(String id, {required String patchJson}) =>
      _mutate(
        () => _api.editSession(
          handle: _requireHandle,
          id: id,
          patchJson: patchJson,
        ),
      );

  Future<void> renameSession(String id, String title) => _mutate(
    () => _api.renameSession(handle: _requireHandle, id: id, title: title),
  );

  Future<void> setSection(String id, String? section) => _mutate(
    () => _api.setSection(handle: _requireHandle, id: id, section: section),
  );

  Future<void> markRead(String id) =>
      _mutate(() => _api.markRead(handle: _requireHandle, id: id));

  Future<void> markUnread(List<String> ids) =>
      _mutate(() => _api.markUnread(handle: _requireHandle, ids: ids));

  Future<bool> toggleKeepAlive(String id) =>
      _mutate(() => _api.toggleKeepAlive(handle: _requireHandle, id: id));

  /// Cascade-merge this session's stack. Returns the terminal operation status
  /// (succeeded / paused / failed) for the caller to surface.
  Future<OperationStatusDto> cascadeMerge(String id) =>
      _mutate(() => _api.cascadeMerge(handle: _requireHandle, id: id));

  /// Push this session's stack. Returns the terminal operation status.
  Future<OperationStatusDto> pushStack(String id) =>
      _mutate(() => _api.pushStack(handle: _requireHandle, id: id));

  /// Resume a paused cascade. Returns the next terminal operation status.
  Future<OperationStatusDto> cascadeResume() =>
      _mutate(() => _api.cascadeResume(handle: _requireHandle));

  /// Abandon a paused cascade, leaving the stack where it stopped.
  Future<void> cascadeAbandon() =>
      _mutate(() => _api.cascadeAbandon(handle: _requireHandle));

  /// Register a new project by its server-side repo path, tagged with
  /// [workspace] (null = Main); returns its new id.
  Future<String> addProject(String path, {String? workspace}) => _mutate(
    () => _api.addProject(
      handle: _requireHandle,
      path: path,
      workspace: workspace,
    ),
  );

  /// Deregister a project by id (does not touch the repo on disk).
  Future<void> removeProject(String id) =>
      _mutate(() => _api.removeProject(handle: _requireHandle, id: id));

  /// Scan a server-side directory for git repos and register any it finds,
  /// each new one tagged with [workspace] (null = Main).
  Future<ScanResultDto> scanDirectory(String path, {String? workspace}) =>
      _mutate(
        () => _api.scanDirectory(
          handle: _requireHandle,
          path: path,
          workspace: workspace,
        ),
      );

  /// Register a project by its server-side repo path, or return the id of the
  /// project already registered for it.
  ///
  /// The idempotent counterpart to [addProject]: `POST /projects/ensure` rather
  /// than `POST /projects`. The dedupe stays on the server, which is the only
  /// side that can resolve a path to a repository root — so this never compares
  /// paths itself, and there is no second copy of the rule to drift.
  ///
  /// [workspace] tags the project only if this call newly registers it.
  Future<String> ensureProject(String path, {String? workspace}) => _mutate(
    () => _api.ensureProject(
      handle: _requireHandle,
      path: path,
      workspace: workspace,
    ),
  );

  /// Replace this server's workspace definitions (see
  /// [CommanderApi.setWorkspaces]). Fleet-wide edits go through
  /// `FleetStore`, which sends the same request to every server.
  Future<void> setWorkspaces(SetWorkspacesRequestDto request) => _mutate(
    () => _api.setWorkspaces(handle: _requireHandle, request: request),
  );

  /// Rename a workspace on this server, rewriting its projects' tags.
  Future<void> renameWorkspace(String from, String to) => _mutate(
    () => _api.renameWorkspace(handle: _requireHandle, from: from, to: to),
  );

  /// Delete a workspace on this server, moving its projects to Main.
  Future<void> deleteWorkspace(String name) =>
      _mutate(() => _api.deleteWorkspace(handle: _requireHandle, name: name));

  /// Move a project to [workspace] (null = Main) on this server.
  Future<void> setProjectWorkspace(String projectId, String? workspace) =>
      _mutate(
        () => _api.setProjectWorkspace(
          handle: _requireHandle,
          projectId: projectId,
          workspace: workspace,
        ),
      );

  /// Every repo the server-side `gh` user can clone, for the repo picker.
  /// Throws when the server has no `gh`, or when listing outruns the client's
  /// request ceiling — the picker words both inline.
  Future<List<GithubRepo>> githubRepos() =>
      _api.githubRepos(handle: _requireHandle);

  /// Provider-neutral repository listing from the connected server.
  Future<RepositoryListing> repositories() =>
      _api.repositories(handle: _requireHandle);

  /// Start a clone. The returned job's status is not terminal; poll [cloneJob].
  Future<CloneJobDto> startClone(CloneRequestDto request) =>
      _api.startClone(handle: _requireHandle, request: request);

  /// One poll of a clone job. Null means the server has pruned it, which is a
  /// normal answer rather than a failure.
  Future<CloneJobDto?> cloneJob(CloneJobId id) =>
      _api.cloneJob(handle: _requireHandle, id: id);

  /// A clone source's stable `host/owner/name` identity, or null when it has
  /// none. No handle: it is pure string work in the shared protocol crate.
  ///
  /// **A null on either side of a comparison is never a match.** See
  /// [CommanderApi.canonicalRepoSlug].
  Future<String?> canonicalRepoSlug(String url) =>
      _api.canonicalRepoSlug(url: url);

  /// List a project's branches (local, plus remotes when [fetch] is set).
  Future<List<BranchInfo>> listBranches(
    String projectId, {
    bool fetch = false,
  }) => _api.listBranches(
    handle: _requireHandle,
    projectId: projectId,
    fetch: fetch,
  );

  /// Fetch a single session's detail (pane snapshot / diff stat) — data the
  /// snapshot doesn't carry, so the detail page fetches it on demand.
  Future<SessionDetail?> sessionDetail(String id, {int? lines}) =>
      _api.getSessionDetail(handle: _requireHandle, query: id, lines: lines);

  /// Register (or clear, with null) the terminal attach the UI currently holds
  /// open, so [reconnect] / [dispose] can detach it before releasing the handle.
  void setActiveTerminalAttach(String? attachId) =>
      _activeTerminalAttachId = attachId;

  /// Clear the registered attach only if it's still [attachId]. A disposing
  /// terminal body calls this instead of `setActiveTerminalAttach(null)`, because
  /// when the wide detail pane switches agent↔shell the INCOMING body's
  /// `initState` registers its new attach BEFORE the outgoing body's `dispose`
  /// runs — an unconditional clear would then null out the live new attach.
  void clearActiveTerminalAttach(String attachId) {
    if (_activeTerminalAttachId == attachId) _activeTerminalAttachId = null;
  }

  /// Detach the currently registered terminal attach (if any) and forget it.
  /// Best-effort: the cdylib's disconnect teardown is the primary path.
  Future<void> _detachActiveTerminal() async {
    final attach = _activeTerminalAttachId;
    _activeTerminalAttachId = null;
    if (attach == null) return;
    try {
      await _api.terminalDetach(attachId: attach);
    } catch (_) {
      // Ignore: disconnect_server also aborts the attach server-side.
    }
  }

  // --- internals ----------------------------------------------------------

  String get _requireHandle =>
      _handle ?? (throw StateError('CommanderStore is not connected'));

  Future<T> _mutate<T>(Future<T> Function() action) async {
    final epoch = _connectEpoch;
    final result = await action();
    if (!_disposed && epoch == _connectEpoch) await _refresh();
    return result;
  }

  final _changes = StreamController<void>.broadcast();

  @visibleForTesting
  bool get hasPendingChangeWaiters => _changes.hasListener;

  /// Wait for server invalidation or a compatibility/reconciliation deadline.
  Future<void> waitForChange(Duration fallback) async {
    if (_disposed) return;
    final completion = Completer<void>();
    void finish() {
      if (!completion.isCompleted) completion.complete();
    }

    final subscription = _changes.stream.listen(
      (_) => finish(),
      onDone: finish,
    );
    final timer = Timer(fallback, finish);
    try {
      await completion.future;
    } finally {
      timer.cancel();
      // This broadcast controller has no asynchronous cleanup; cancelling
      // removes the listener immediately without delaying the waiting page.
      // Pinned by 'timed-out clone waits release subscriptions'.
      unawaited(subscription.cancel());
    }
  }

  void _onChange(BigInt _) {
    _changes.add(null);
    unawaited(_refresh());
  }

  void _onConnection(ConnectionStateDto state) {
    _connection = state;
    if (!_disposed) notifyListeners();
  }

  Future<void> _refresh() async {
    final h = _handle;
    if (h == null) return;
    if (_refreshing) {
      _refreshQueued = true;
      return;
    }
    _refreshing = true;
    final epoch = _connectEpoch;
    try {
      final results = await Future.wait<Object>([
        _api.snapshot(handle: h),
        _api.agentStates(handle: h, fresh: false),
      ]);
      if (_disposed || h != _handle || epoch != _connectEpoch) return;
      final ws = results[0] as SnapshotDto;
      final states = results[1] as AgentStatesSnapshotDto;
      _snapshot = ws;
      _agentStates
        ..clear()
        ..addEntries(
          states.states.map((e) => MapEntry(e.sessionId.field0.uuid, e.state)),
        );
      _commanderRunning = states.commanderRunning;
      _error = null;
    } catch (e) {
      if (!_disposed && h == _handle && epoch == _connectEpoch) {
        _error = e;
      }
    } finally {
      _refreshing = false;
      if (!_disposed && h == _handle && epoch == _connectEpoch) {
        notifyListeners();
      }
      if (_refreshQueued && !_disposed) {
        _refreshQueued = false;
        // Fire-and-forget: the coalesced follow-up must not extend THIS call's
        // await. Awaiting it chains a fresh refresh onto every tick that landed
        // mid-fetch, so on a server whose state keeps moving (the poller bumps
        // its generation every couple of seconds) the chain re-arms faster than
        // it unwinds and the awaited refresh never returns — stranding
        // `connect()`/`reconnect()` and any UI spinner waiting on them.
        unawaited(_refresh());
      }
    }
  }

  /// Cancel and forget both feeds. Claims the fields synchronously before the
  /// first await for the same reason [reconnect] claims the handle: a cancel that
  /// parks here must not resume to read — and cancel — a NEWER connect's feeds,
  /// nor null out the fields that reference them.
  Future<void> _teardownSubs() async {
    final change = _changeSub;
    final connection = _connectionSub;
    _changeSub = null;
    _connectionSub = null;
    await change?.cancel();
    await connection?.cancel();
  }

  @override
  void dispose() {
    _disposed = true;
    unawaited(_changes.close());
    // cancel() returns a Future; nothing awaits it during teardown.
    unawaited(_teardownSubs());
    final h = _handle;
    if (h != null) {
      unawaited(_detachActiveTerminal());
      unawaited(_api.disconnectServer(handle: h));
    }
    super.dispose();
  }
}
