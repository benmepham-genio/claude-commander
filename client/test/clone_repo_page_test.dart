import 'dart:async';

import 'package:claude_commander_client/pages/clone_repo_page.dart';
import 'package:claude_commander_client/src/rust/api/mirrors.dart';
import 'package:claude_commander_client/state/commander_store.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fake_commander_api.dart';
import 'support/fixtures.dart';

void main() {
  late FakeCommanderApi api;
  late CommanderStore store;

  setUp(() {
    api = FakeCommanderApi();
    store = CommanderStore(api: api, config: testConfig);
  });

  tearDown(() => store.dispose());

  Widget wrap() => MaterialApp(home: CloneRepoPage(store: store));

  /// Connect the store (so the page has a live handle + projects), then pump the
  /// page and let its repo fetch and slug resolution settle.
  Future<void> pump(WidgetTester tester) async {
    await store.connect();
    await tester.pumpWidget(wrap());
    await tester.pumpAndSettle();
  }

  group('local search filter', () {
    testWidgets('narrows the list without refetching', (tester) async {
      api.githubReposResponse = [
        githubRepo(owner: 'acme', name: 'widget'),
        githubRepo(owner: 'acme', name: 'gadget'),
        githubRepo(owner: 'other', name: 'sprocket'),
      ];
      await pump(tester);

      expect(find.text('acme/widget'), findsOneWidget);
      expect(find.text('acme/gadget'), findsOneWidget);
      expect(find.text('other/sprocket'), findsOneWidget);
      final fetchesBefore = api.countOf('repositories');

      await tester.enterText(
        find.byKey(const Key('repo-search-field')),
        'sproc',
      );
      await tester.pumpAndSettle();

      expect(find.text('acme/widget'), findsNothing);
      expect(find.text('acme/gadget'), findsNothing);
      expect(find.text('other/sprocket'), findsOneWidget);
      // The filter is local: typing must not hit the network per keystroke.
      expect(api.countOf('repositories'), fetchesBefore);
    });

    testWidgets('matching an owner keeps that owner\'s repos', (tester) async {
      api.githubReposResponse = [
        githubRepo(owner: 'acme', name: 'widget'),
        githubRepo(owner: 'other', name: 'sprocket'),
      ];
      await pump(tester);

      await tester.enterText(
        find.byKey(const Key('repo-search-field')),
        'ACME',
      );
      await tester.pumpAndSettle();

      expect(find.text('acme/widget'), findsOneWidget);
      expect(find.text('other/sprocket'), findsNothing);
    });
  });

  group('already-added badge', () {
    testWidgets('a GitLab ssh origin matches an https nested project', (
      tester,
    ) async {
      api.projectsResponse = [
        projectInfo(
          name: 'widget',
          originUrl: 'git@gitlab.example.com:group/sub/widget.git',
        ),
      ];
      api.repositoriesResponse = RepositoryListing(
        host: const CodeHost(
          provider: CodeHostProvider.gitlab,
          hostname: 'gitlab.example.com',
        ),
        repositories: [
          hostedRepo(
            namespace: 'group/sub',
            name: 'widget',
            hostname: 'gitlab.example.com',
          ),
        ],
      );
      await pump(tester);

      expect(find.text('Added'), findsOneWidget);
    });

    testWidgets('an ssh origin matches an https clone url', (tester) async {
      // The project was cloned by `gh` with git_protocol=ssh, so its origin is
      // the scp spelling; the API reports https. Raw string equality misses
      // this — only canonicalisation catches it.
      api.projectsResponse = [
        projectInfo(
          name: 'widget',
          repoPath: '/srv/projects/widget',
          originUrl: 'ssh://git@github.com/acme/widget.git',
        ),
      ];
      api.githubReposResponse = [
        githubRepo(
          owner: 'acme',
          name: 'widget',
          cloneUrl: 'https://github.com/acme/widget.git',
        ),
        githubRepo(owner: 'acme', name: 'gadget'),
      ];
      await pump(tester);

      expect(find.text('Added'), findsOneWidget);
      expect(
        find.descendant(
          of: find.byKey(const Key('repo-row-acme/widget')),
          matching: find.text('Added'),
        ),
        findsOneWidget,
      );
      // The unregistered sibling is not badged.
      expect(
        find.descendant(
          of: find.byKey(const Key('repo-row-acme/gadget')),
          matching: find.text('Added'),
        ),
        findsNothing,
      );
    });

    testWidgets('an added repo is not clonable', (tester) async {
      api.projectsResponse = [
        projectInfo(
          name: 'widget',
          originUrl: 'git@github.com:acme/widget.git',
        ),
      ];
      api.githubReposResponse = [githubRepo(owner: 'acme', name: 'widget')];
      await pump(tester);

      expect(find.text('Added'), findsOneWidget);
      await tester.tap(find.byKey(const Key('repo-row-acme/widget')));
      await tester.pumpAndSettle();

      // No confirm sheet, and nothing was started.
      expect(find.byKey(const Key('clone-dest-name-field')), findsNothing);
      expect(api.countOf('startClone'), 0);
    });

    testWidgets('two unslugged sources are not treated as a match', (
      tester,
    ) async {
      // `canonicalRepoSlug` answers null for a source with no GitHub identity —
      // a local path, a `file://` URL, or a project with no origin at all. Two
      // nulls must never compare equal, or every row gets a badge.
      api.projectsResponse = [
        projectInfo(name: 'local-only', repoPath: '/srv/projects/local-only'),
      ];
      api.githubReposResponse = [
        githubRepo(owner: 'acme', name: 'widget', cloneUrl: '/srv/elsewhere'),
      ];
      await pump(tester);

      expect(find.text('acme/widget'), findsOneWidget);
      expect(find.text('Added'), findsNothing);
    });

    testWidgets('an origin that is present but unslugged is not a match', (
      tester,
    ) async {
      // The sibling case to the test above, and the sharper one: there the
      // project has *no* origin at all, so it is dropped before canonicalisation
      // is even attempted. Here the origin is present and simply has no GitHub
      // identity ('relative/local/path' → null), so it reaches the
      // canonicalisation and comes back null — as does the repo's `clone_url`,
      // deliberately given the same shape. If a null were allowed to stand in as
      // an identity on either side of the comparison, these two would "match" and
      // the row would wear a false Added badge, blocking a clone the user can
      // legitimately make. Asserted on the badge *and* on the row still being
      // clonable, because that is the actual harm.
      api.projectsResponse = [
        projectInfo(
          name: 'mirror',
          repoPath: '/srv/projects/mirror',
          originUrl: 'relative/local/path',
        ),
      ];
      api.githubReposResponse = [
        githubRepo(
          owner: 'acme',
          name: 'widget',
          cloneUrl: 'relative/local/path',
        ),
      ];
      await pump(tester);

      expect(find.text('acme/widget'), findsOneWidget);
      expect(find.text('Added'), findsNothing);

      await tester.tap(find.byKey(const Key('repo-row-acme/widget')));
      await tester.pumpAndSettle();
      expect(find.byKey(const Key('clone-dest-name-field')), findsOneWidget);
    });
  });

  group('grouping', () {
    testWidgets('groups repos under an owner heading', (tester) async {
      api.githubReposResponse = [
        githubRepo(owner: 'acme', name: 'widget'),
        githubRepo(owner: 'acme', name: 'gadget'),
        githubRepo(owner: 'zeta', name: 'sprocket'),
      ];
      await pump(tester);

      expect(find.byKey(const Key('owner-heading-acme')), findsOneWidget);
      expect(find.byKey(const Key('owner-heading-zeta')), findsOneWidget);
    });

    testWidgets('keeps nested GitLab namespaces and visibility badges', (
      tester,
    ) async {
      api.repositoriesResponse = RepositoryListing(
        host: const CodeHost(
          provider: CodeHostProvider.gitlab,
          hostname: 'gitlab.example.com',
        ),
        repositories: [
          hostedRepo(
            namespace: 'group/sub',
            name: 'internal',
            visibility: RepositoryVisibility.internal,
            hostname: 'gitlab.example.com',
          ),
          hostedRepo(
            namespace: 'group/sub',
            name: 'private',
            visibility: RepositoryVisibility.private,
            archived: true,
            hostname: 'gitlab.example.com',
          ),
        ],
      );
      await pump(tester);

      expect(find.byKey(const Key('owner-heading-group/sub')), findsOneWidget);
      expect(find.text('group/sub/internal'), findsOneWidget);
      expect(find.text('internal'), findsOneWidget);
      expect(find.text('private · archived'), findsOneWidget);
    });
  });

  group('GitLab provider', () {
    testWidgets('empty listing still communicates provider and hostname', (
      tester,
    ) async {
      api.repositoriesResponse = const RepositoryListing(
        host: CodeHost(
          provider: CodeHostProvider.gitlab,
          hostname: 'gitlab.example.com',
        ),
        repositories: [],
      );
      await pump(tester);

      expect(find.text('No GitLab projects'), findsOneWidget);
      final field = tester.widget<TextField>(
        find.byKey(const Key('clone-url-field')),
      );
      expect(
        field.decoration?.hintText,
        'https://gitlab.example.com/group/project.git',
      );
    });

    testWidgets('listing failure gives glab guidance from server status', (
      tester,
    ) async {
      api.codeHostStatusResponse = const CodeHostStatus(
        provider: CodeHostProvider.gitlab,
        hostname: 'gitlab.example.com',
        cliAvailable: false,
      );
      api.githubReposError = StateError('GitLab CLI is unavailable');
      await pump(tester);

      expect(find.textContaining('`glab` installed'), findsOneWidget);
    });

    testWidgets('clone freezes the listing hostname into its source', (
      tester,
    ) async {
      api.repositoriesResponse = RepositoryListing(
        host: const CodeHost(
          provider: CodeHostProvider.gitlab,
          hostname: 'gitlab.example.com',
        ),
        repositories: [
          hostedRepo(
            namespace: 'group/sub',
            name: 'widget',
            hostname: 'gitlab.example.com',
          ),
        ],
      );
      api.cloneJobResponse = cloneJob(
        status: const CloneStatusDto(
          kind: CloneStatusKind.failed,
          message: 'stopped',
          isGitRepo: false,
        ),
      );
      await pump(tester);

      await tester.tap(find.byKey(const Key('repo-row-group/sub/widget')));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(const Key('clone-confirm-button')));
      await tester.pumpAndSettle();

      final request =
          api.lastCall('startClone')!.args['request'] as CloneRequestDto;
      expect(request.source.kind, CloneSourceKind.gitlab);
      expect(request.source.value, 'group/sub/widget');
      expect(request.source.hostname, 'gitlab.example.com');
    });
  });

  group('gh unavailable', () {
    testWidgets('shows an inline banner and keeps the url field usable', (
      tester,
    ) async {
      api.githubReposError = StateError('gh is not installed on the server');
      await pump(tester);

      expect(find.byKey(const Key('repo-list-error')), findsOneWidget);
      expect(find.textContaining('gh is not installed'), findsOneWidget);

      // The URL path does not depend on `gh`, so it must still work.
      final urlField = find.byKey(const Key('clone-url-field'));
      expect(urlField, findsOneWidget);
      await tester.enterText(urlField, 'https://github.com/acme/widget.git');
      await tester.pumpAndSettle();
      expect(
        tester.widget<TextField>(urlField).controller!.text,
        'https://github.com/acme/widget.git',
      );
    });

    testWidgets('pull-to-refresh retries the fetch', (tester) async {
      api.githubReposError = StateError('gh is not installed on the server');
      await pump(tester);
      expect(api.countOf('repositories'), 1);

      api.githubReposError = null;
      api.githubReposResponse = [githubRepo(owner: 'acme', name: 'widget')];
      await tester.tap(find.byKey(const Key('repo-list-retry')));
      await tester.pumpAndSettle();

      expect(api.countOf('repositories'), 2);
      expect(find.text('acme/widget'), findsOneWidget);
    });
  });

  group('confirm sheet', () {
    testWidgets('prefills the directory name from the repo name', (
      tester,
    ) async {
      api.githubReposResponse = [githubRepo(owner: 'acme', name: 'widget')];
      await pump(tester);

      await tester.tap(find.byKey(const Key('repo-row-acme/widget')));
      await tester.pumpAndSettle();

      final field = find.byKey(const Key('clone-dest-name-field'));
      expect(field, findsOneWidget);
      expect(tester.widget<TextField>(field).controller!.text, 'widget');
    });

    testWidgets('starting a clone sends the github slug and the edited name', (
      tester,
    ) async {
      api.githubReposResponse = [githubRepo(owner: 'acme', name: 'widget')];
      // Terminate the poll on the first tick so the test leaves no timer behind.
      api.cloneJobResponse = cloneJob(
        status: const CloneStatusDto(
          kind: CloneStatusKind.failed,
          message: 'remote hung up',
          isGitRepo: false,
        ),
      );
      await pump(tester);

      await tester.tap(find.byKey(const Key('repo-row-acme/widget')));
      await tester.pumpAndSettle();
      await tester.enterText(
        find.byKey(const Key('clone-dest-name-field')),
        'widget-2',
      );
      await tester.tap(find.byKey(const Key('clone-confirm-button')));
      await tester.pumpAndSettle();

      expect(api.countOf('startClone'), 1);
      final request =
          api.lastCall('startClone')!.args['request'] as CloneRequestDto;
      expect(request.source.kind, CloneSourceKind.github);
      expect(request.source.value, 'acme/widget');
      expect(request.destName, 'widget-2');
      // A failure is reported and the page stays put.
      expect(find.textContaining('remote hung up'), findsOneWidget);
      expect(find.byType(CloneRepoPage), findsOneWidget);
    });
  });

  group('busy guard', () {
    testWidgets('a second tap while startClone is in flight starts nothing', (
      tester,
    ) async {
      api.githubReposResponse = [githubRepo(owner: 'acme', name: 'widget')];
      api.cloneJobResponse = cloneJob(
        status: const CloneStatusDto(
          kind: CloneStatusKind.failed,
          message: 'stopped',
          isGitRepo: false,
        ),
      );
      // Park `startClone`, holding the page in the window AFTER the confirm
      // sheet has popped but BEFORE the job exists. A guard derived from the job
      // is not armed yet here, so this is the window a second tap slips through.
      final gate = Completer<void>();
      api.startCloneGate = gate;
      await pump(tester);

      await tester.tap(find.byKey(const Key('repo-row-acme/widget')));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(const Key('clone-confirm-button')));
      await tester.pumpAndSettle();

      // The sheet is gone and the page is interactive again, but no job yet.
      expect(find.byKey(const Key('clone-dest-name-field')), findsNothing);
      expect(api.countOf('startClone'), 1);

      await tester.tap(
        find.byKey(const Key('repo-row-acme/widget')),
        warnIfMissed: false,
      );
      await tester.pumpAndSettle();
      expect(api.countOf('startClone'), 1);
      // The URL path is closed off for the same reason.
      expect(
        tester
            .widget<TextField>(find.byKey(const Key('clone-url-field')))
            .enabled,
        isFalse,
      );

      gate.complete();
      await tester.pumpAndSettle();
      expect(api.countOf('startClone'), 1);
    });
  });

  group('url clone', () {
    testWidgets('never renders the credential from a pasted url', (
      tester,
    ) async {
      const secret = 'ghp_16C7e42F292c6912E7710c838347Ae178B4a';
      api.githubReposResponse = [];
      // The server redacts what it hands back; the page must render THAT and
      // never rebuild a label out of the text the user typed.
      api.startCloneResponse = cloneJob(
        sourceLabel: 'https://***@github.com/acme/widget.git',
      );
      api.cloneJobResponse = cloneJob(
        sourceLabel: 'https://***@github.com/acme/widget.git',
        status: const CloneStatusDto(
          kind: CloneStatusKind.failed,
          message: 'authentication failed',
          isGitRepo: false,
        ),
      );
      await pump(tester);

      await tester.enterText(
        find.byKey(const Key('clone-url-field')),
        'https://user:$secret@github.com/acme/widget.git',
      );
      await tester.tap(find.byKey(const Key('clone-url-submit')));
      await tester.pumpAndSettle();

      // The confirm sheet is up, and the URL field behind it has already been
      // cleared — the credential is not sitting on screen through the flow.
      final nameField = find.byKey(const Key('clone-dest-name-field'));
      expect(nameField, findsOneWidget);
      // The prefill comes from the last path segment, never the authority.
      expect(tester.widget<TextField>(nameField).controller!.text, 'widget');
      expect(
        find.textContaining(secret, findRichText: true),
        findsNothing,
        reason: 'a pasted credential must not survive into the confirm sheet',
      );

      await tester.tap(find.byKey(const Key('clone-confirm-button')));
      await tester.pumpAndSettle();

      expect(api.countOf('startClone'), 1);
      // The raw url IS what gets sent — it has to be, it is the clone source.
      final request =
          api.lastCall('startClone')!.args['request'] as CloneRequestDto;
      expect(request.source.value, contains(secret));
      // ...but nothing rendered is built from it. The banner and the failure
      // both come from the server's already-redacted strings.
      expect(
        find.textContaining(secret, findRichText: true),
        findsNothing,
        reason: 'a pasted credential must never reach the widget tree',
      );
    });

    testWidgets('an authority-only url gets no prefilled directory name', (
      tester,
    ) async {
      // No path, so the last `/`-segment IS the authority. Prefilling from it
      // would render `user:token@host` as the repo name.
      const secret = 'ghp_16C7e42F292c6912E7710c838347Ae178B4a';
      await pump(tester);

      await tester.enterText(
        find.byKey(const Key('clone-url-field')),
        'https://user:$secret@github.com',
      );
      await tester.tap(find.byKey(const Key('clone-url-submit')));
      await tester.pumpAndSettle();

      final nameField = find.byKey(const Key('clone-dest-name-field'));
      expect(tester.widget<TextField>(nameField).controller!.text, '');
      expect(find.textContaining(secret, findRichText: true), findsNothing);
    });
  });

  group('occupied destination', () {
    testWidgets('a git checkout offers Register existing', (tester) async {
      api.githubReposResponse = [githubRepo(owner: 'acme', name: 'widget')];
      api.cloneJobResponse = cloneJob(
        status: const CloneStatusDto(
          kind: CloneStatusKind.destinationExists,
          message: '',
          dest: '/srv/projects/widget',
          isGitRepo: true,
        ),
      );
      await pump(tester);

      await tester.tap(find.byKey(const Key('repo-row-acme/widget')));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(const Key('clone-confirm-button')));
      await tester.pumpAndSettle();

      expect(find.text('Register existing'), findsOneWidget);
      expect(find.textContaining('/srv/projects/widget'), findsWidgets);
      await tester.tap(find.text('Register existing'));
      await tester.pumpAndSettle();

      // The idempotent route, not `addProject`: the offer only appears because
      // the destination was occupied, so the checkout is frequently already a
      // project and `addProject` would register a second entry for it.
      expect(api.countOf('ensureProject'), 1);
      expect(
        api.lastCall('ensureProject')!.args['path'],
        '/srv/projects/widget',
      );
      expect(api.countOf('addProject'), 0);
    });

    testWidgets('Register existing reuses an already-registered project', (
      tester,
    ) async {
      // The dedupe is the server's: the page calls `ensureProject`, which answers
      // with the existing project's id, and never `addProject` — which does not
      // dedupe and would leave two entries for one checkout.
      api.projectsResponse = [
        projectInfo(name: 'widget', repoPath: '/srv/projects/widget'),
      ];
      api.githubReposResponse = [githubRepo(owner: 'acme', name: 'widget')];
      api.cloneJobResponse = cloneJob(
        status: const CloneStatusDto(
          kind: CloneStatusKind.destinationExists,
          message: '',
          dest: '/srv/projects/widget',
          isGitRepo: true,
        ),
      );
      await pump(tester);

      await tester.tap(find.byKey(const Key('repo-row-acme/widget')));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(const Key('clone-confirm-button')));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Register existing'));
      await tester.pumpAndSettle();

      expect(api.countOf('addProject'), 0);
      expect(api.countOf('ensureProject'), 1);
    });

    testWidgets('a non-repo directory returns to the sheet to rename', (
      tester,
    ) async {
      api.githubReposResponse = [githubRepo(owner: 'acme', name: 'widget')];
      api.cloneJobResponse = cloneJob(
        status: const CloneStatusDto(
          kind: CloneStatusKind.destinationExists,
          message: '',
          dest: '/srv/projects/widget',
          isGitRepo: false,
        ),
      );
      await pump(tester);

      await tester.tap(find.byKey(const Key('repo-row-acme/widget')));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(const Key('clone-confirm-button')));
      await tester.pumpAndSettle();

      // Back in the sheet, so the user can pick another name.
      expect(find.byKey(const Key('clone-dest-name-field')), findsOneWidget);
      expect(find.textContaining('already exists'), findsOneWidget);
      // Dismiss, or the flow loop keeps the sheet open past the test.
      await tester.tap(find.text('Cancel'));
      await tester.pumpAndSettle();
    });
  });
}
