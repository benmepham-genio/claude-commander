import 'dart:async';

import 'package:claude_commander_client/server_config.dart';
import 'package:claude_commander_client/src/rust/api/mirrors.dart';
import 'package:claude_commander_client/state/commander_store.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:uuid/uuid.dart';

import '../support/fake_commander_api.dart';
import '../support/fixtures.dart';

void main() {
  late FakeCommanderApi api;

  setUp(() => api = FakeCommanderApi());

  const otherConfig = ServerConfig(
    id: 'other-server',
    name: 'other',
    baseUrl: 'http://other.test:9999',
    token: 'other-token',
  );

  const thirdConfig = ServerConfig(
    id: 'other-server',
    name: 'other renamed',
    baseUrl: 'http://third.test:7777',
    token: 'third-token',
  );

  const id = '11111111-2222-3333-4444-555555555555';

  AgentStatesSnapshotDto statesWith(AgentState state) => AgentStatesSnapshotDto(
    states: [
      AgentStateEntryDto(
        sessionId: SessionId(field0: UuidValue.fromString(id)),
        state: state,
      ),
    ],
    commanderRunning: true,
  );

  CommanderStore build() => CommanderStore(api: api, config: testConfig);

  test('snapshot indexes reuse reads and discard removed membership', () async {
    final session = sessionInfo(
      id: id,
      projectId: 'aaaaaaaa-2222-3333-4444-555555555555',
    );
    api.listSessionsResponse = [session];
    api.projectsResponse = [projectInfo(workspace: 'work')];
    final store = build();
    addTearDown(store.dispose);
    await store.connect();
    final grouped = store.sessionsByProject;
    final members = store.sessionsIn('work');
    expect(identical(grouped, store.sessionsByProject), isTrue);
    expect(identical(members, store.sessionsIn('work')), isTrue);
    expect(store.sessionById(id), session);
    expect(members, [session]);
    expect(store.sessionsIn(null), isEmpty);
    api.listSessionsResponse = [];
    api.projectsResponse = [projectInfo(workspace: null)];
    await store.refresh();
    expect(store.sessionById(id), isNull);
    expect(store.sessionsIn('work'), isEmpty);
    expect(store.sessionsByProject.single.sessions, isEmpty);
    expect(identical(grouped, store.sessionsByProject), isFalse);
  });

  test('timed-out clone waits release their subscriptions', () async {
    final store = build();
    addTearDown(store.dispose);
    for (var i = 0; i < 3; i++) {
      await store.waitForChange(const Duration(milliseconds: 1));
      expect(store.hasPendingChangeWaiters, isFalse);
    }
  });

  test('clone tracking wakes on a server invalidation', () async {
    final store = build();
    addTearDown(store.dispose);
    await store.connect();
    final wake = store.waitForChange(const Duration(seconds: 30));
    api.emitChange();
    await wake.timeout(const Duration(seconds: 1));
  });

  test(
    'superseded refresh failures cannot overwrite a reconnected store',
    () async {
      final store = build();
      addTearDown(store.dispose);
      await store.connect();
      final oldHandle = store.handle!;
      final oldGate = Completer<void>();
      final newGate = Completer<void>();
      addTearDown(() {
        if (!newGate.isCompleted) newGate.complete();
      });
      api.snapshotGates[oldHandle] = oldGate;
      api.snapshotErrors[oldHandle] = StateError('old server');
      final oldRefresh = store.refresh();
      await Future<void>.delayed(Duration.zero);
      await store.reconnect(otherConfig);
      api.snapshotGates[store.handle!] = newGate;
      oldGate.complete();
      await oldRefresh;
      expect(store.error, isNull);
      newGate.complete();
      await Future<void>.delayed(Duration.zero);
      expect(store.config.baseUrl, otherConfig.baseUrl);
    },
  );

  test(
    'a successful mutation refreshes without waiting for the change feed',
    () async {
      final store = build();
      addTearDown(store.dispose);
      await store.connect();
      final before = api.countOf('snapshot');
      await store.renameSession(id, 'new name');
      expect(api.countOf('snapshot'), greaterThan(before));
    },
  );

  test('a superseded in-flight connect releases its handle (no leak)', () async {
    final store = build();
    addTearDown(store.dispose);

    // First connect parks inside connectServer.
    final gate = Completer<void>();
    api.connectGate = gate;
    final first = store.connect();
    await Future<void>.delayed(Duration.zero);

    // A second connect (e.g. a double-tapped retry) supersedes the first.
    api.connectGate = null;
    await store.connect();

    // The first now resumes on a stale epoch: it must release the handle it
    // acquired and wire no feeds, rather than overwriting the live connection.
    gate.complete();
    await first;
    await Future<void>.delayed(Duration.zero);

    expect(api.countOf('connectServer'), 2);
    expect(api.countOf('disconnectServer'), 1); // the superseded connect
    expect(api.countOf('changeFeed'), 1); // only the live connect wired a feed
    expect(store.handle, isNotNull);
  });

  test(
    'connect() acquires the handle and populates the snapshot + agent states',
    () async {
      api.connectServerResponse = 'handle-1';
      api.listSessionsResponse = [sessionInfo(id: id, title: 'Alpha')];
      api.agentStatesResponse = statesWith(AgentState.working);

      final store = build();
      addTearDown(store.dispose);
      await store.connect();

      expect(store.handle, 'handle-1');
      expect(store.sessions.map((s) => s.title), ['Alpha']);
      expect(store.agentStateFor(id), AgentState.working);
      expect(store.commanderRunning, isTrue);
      expect(store.loading, isFalse);
      expect(store.error, isNull);
      // The feeds were subscribed with the acquired handle.
      expect(api.lastCall('changeFeed')!.args['handle'], 'handle-1');
      expect(api.lastCall('connectionFeed')!.args['handle'], 'handle-1');
    },
  );

  test(
    'a change-feed tick triggers a refetch and notifies listeners',
    () async {
      api.listSessionsResponse = [sessionInfo(id: id, title: 'Before')];
      final store = build();
      addTearDown(store.dispose);
      await store.connect();

      var notifications = 0;
      store.addListener(() => notifications++);
      final refetchesBefore = api.countOf('snapshot');

      // The server state moved: the next snapshot has a new title.
      api.listSessionsResponse = [sessionInfo(id: id, title: 'After')];
      api.emitChange();
      await pumpEventQueue();

      expect(api.countOf('snapshot'), greaterThan(refetchesBefore));
      expect(store.sessions.single.title, 'After');
      expect(notifications, greaterThan(0));
    },
  );

  test(
    'refresh() does not wait on the follow-up a mid-flight tick queued',
    () async {
      final store = build();
      addTearDown(store.dispose);
      await store.connect();

      // A poller tick lands while our fetch is in flight, so a follow-up refresh
      // is coalesced onto it — and that follow-up then parks on a slow server.
      final gate = Completer<void>();
      addTearDown(() {
        if (!gate.isCompleted) gate.complete();
      });
      var armed = false;
      api.onSnapshot = () {
        if (armed) return;
        armed = true;
        api.emitChange(); // queues a follow-up refresh
        api.snapshotGate = gate; // which will park
      };

      final before = api.countOf('snapshot');
      var done = false;
      unawaited(store.refresh().then((_) => done = true));
      await pumpEventQueue();

      // The caller's refresh completed on its own fetch. Chaining the queued
      // follow-up onto it instead re-arms the await on every tick — on a busy
      // server that never unwinds, stranding connect()/reconnect() and every UI
      // spinner waiting on them (e.g. the Edit server form's Save).
      expect(done, isTrue);
      // The follow-up did still fire — it's parked on the gate, not skipped.
      expect(api.countOf('snapshot'), before + 2);
    },
  );

  test('a connection-feed event updates connection and notifies', () async {
    final store = build();
    addTearDown(store.dispose);
    await store.connect();

    var notified = false;
    store.addListener(() => notified = true);

    api.emitConnection(
      const ConnectionStateDto(
        kind: ConnectionStateKind.degraded,
        reason: 'slow link',
      ),
    );
    await pumpEventQueue();

    expect(store.connection.kind, ConnectionStateKind.degraded);
    expect(store.connection.reason, 'slow link');
    expect(notified, isTrue);
  });

  test(
    'reconnect() disconnects the OLD handle before connecting the new one',
    () async {
      api.connectServerResponse = 'handle-1';
      final store = build();
      addTearDown(store.dispose);
      await store.connect();
      expect(store.handle, 'handle-1');

      api.connectServerResponse = 'handle-2';
      await store.reconnect(otherConfig);

      expect(store.handle, 'handle-2');
      expect(store.config, otherConfig);

      // The old handle was released, and that release happened BEFORE the second
      // connect — so no handle is ever left dangling in the cdylib registry.
      final disconnectIdx = api.calls.indexWhere(
        (c) => c.method == 'disconnectServer' && c.args['handle'] == 'handle-1',
      );
      final connectIdxs = [
        for (var i = 0; i < api.calls.length; i++)
          if (api.calls[i].method == 'connectServer') i,
      ];
      expect(disconnectIdx, isNonNegative);
      expect(connectIdxs.length, 2);
      expect(disconnectIdx, lessThan(connectIdxs[1]));
      // The new handle's feeds are live; the old one's are gone.
      expect(api.lastCall('changeFeed')!.args['handle'], 'handle-2');
    },
  );

  test(
    'a superseded reconnect does not roll its config back over a newer edit',
    () async {
      api.connectServerResponse = 'handle-1';
      final store = build();
      addTearDown(store.dispose);
      await store.connect();

      // Edit #1 (the real flow: FleetStore.updateServer applies the config,
      // persists, then reconnects). Its reconnect parks while releasing handle-1.
      final gate = Completer<void>();
      api.disconnectGate = gate;
      store.applyConfig(otherConfig);
      final first = store.reconnect(otherConfig);
      await pumpEventQueue();

      // updateServer completes at the persist commit point and leaves the
      // reconnect unawaited, so the Edit server form dismisses immediately: the
      // user can reopen it and save edit #2 while reconnect #1 is still parked
      // (easy when the old server is wedged and its disconnect sits on a 30s
      // timeout). Edit #2 runs to completion.
      api.disconnectGate = null;
      api.connectServerResponse = 'handle-3';
      store.applyConfig(thirdConfig);
      await store.reconnect(thirdConfig);

      // Reconnect #1 now resumes on a stale epoch. It must not assign its own
      // (older) config, nor supersede the live connection with a third connect.
      gate.complete();
      await first;
      await pumpEventQueue();

      // The persisted list holds edit #2, so the store must too — otherwise the UI
      // and the keychain disagree until the next launch.
      expect(store.config.baseUrl, thirdConfig.baseUrl);
      expect(store.config.token, thirdConfig.token);
      expect(store.config.name, thirdConfig.name);
      // Still live on edit #2's connection — a bail must not leave a dead store.
      expect(store.handle, 'handle-3');
      expect(api.countOf('connectServer'), 2);
      expect(api.countOf('changeFeed'), 2);
      // Reconnect #1 released handle-1 before bailing: no leak, no double-release.
      expect(api.countOf('disconnectServer'), 1);
    },
  );

  test('dispose() releases the handle', () async {
    api.connectServerResponse = 'handle-1';
    final store = build();
    await store.connect();

    store.dispose();
    await pumpEventQueue();

    expect(
      api.calls.any(
        (c) => c.method == 'disconnectServer' && c.args['handle'] == 'handle-1',
      ),
      isTrue,
    );
  });
}
