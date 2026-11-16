import 'package:flutter/foundation.dart' show listEquals;
import 'package:flutter/material.dart';

import '../chrome/chrome.dart';
import '../src/rust/api/mirrors.dart';
import '../state/commander_store.dart';
import '../theme/tokens.dart';
import '../util/error_text.dart';

/// Reconciliation timeout while watching a clone. Server change notifications
/// wake the wait early; this deadline also supports servers without the feed.
const _changeWaitFallback = Duration(seconds: 1);

/// Prefix characters a client-derived directory name may contain. Deliberately
/// narrower than the server's `check_dir_name` (which only bars `/`, `\`, `.`,
/// `..` and a leading `-`): this name is *rendered* in the confirm sheet, and a
/// pasted `https://user:token@host` with no path would otherwise put the whole
/// `user:token@host` authority on screen as the "repo name". Excluding `@` and
/// `:` by construction is what stops that, rather than a redaction pass someone
/// has to remember. A URL we can't derive a safe name from simply gets no
/// prefill and the user types one.
final _safeDirName = RegExp(r'^[A-Za-z0-9._][A-Za-z0-9._-]*$');

/// Picks a repository from the server's selected code host (or takes a clone URL)
/// and clones it into the server's
/// projects directory, registering the result as a project.
///
/// The repo list is the *server's* — `gh` or `glab` runs where the checkout lands — so this
/// works from a phone with no hosting CLI or credentials. Everything the page
/// does with that list is local: the search field filters an already-fetched
/// list rather than issuing a request per keystroke, and pull-to-refresh (or the
/// retry button on the error banner) is the only refetch.
///
/// **The "Clone from URL" field is pinned above the list and never waits on it.**
/// Listing shells out to the selected provider CLI, which on a large account can take
/// longer than the client's request ceiling; the URL path involves no `gh` at
/// all, so a failed or slow listing must not take it down with it.
///
/// A repo already registered as a project is greyed with an **Added** badge and
/// is not clonable. That comparison runs both the project's stored `origin` and
/// the row's `clone_url` through [CommanderStore.canonicalRepoSlug] — the one
/// definition in `claude_commander_protocol::github` — because `gh repo clone`
/// honours the user's `git_protocol`, so a repo it cloned typically has an
/// `ssh://` origin while the API reports `https://`. Raw strings would miss most
/// matches. **A null slug is not an identity**: a project with no origin and a
/// source with no GitHub identity are not "the same unknown repo", and treating
/// them as equal badges every row.
///
/// Nothing here builds a user-facing string out of the URL the user typed. A
/// pasted clone URL can carry `user:token@` userinfo, so the progress banner
/// renders the server's already-redacted `CloneJobDto.sourceLabel`, failures
/// render the server's already-redacted `CloneStatusDto.message`, and the URL
/// field is cleared the moment a flow starts so the raw value does not sit
/// behind the sheet, the banner and any snackbar that follows.
class CloneRepoPage extends StatefulWidget {
  final CommanderStore store;

  /// The workspace the cloned (or registered-existing) project lands in — the
  /// app's active one. Null is Main.
  final String? workspace;

  const CloneRepoPage({super.key, required this.store, this.workspace});

  @override
  State<CloneRepoPage> createState() => _CloneRepoPageState();
}

/// What a single start-and-poll attempt concluded.
enum _Attempt {
  /// Terminal for this flow — succeeded, failed, or the user declined. Nothing
  /// left to offer.
  done,

  /// The destination was occupied and the user wants to pick another name, so
  /// the confirm sheet re-opens with the field focused.
  rename,
}

class _CloneRepoPageState extends State<CloneRepoPage> {
  final _url = TextEditingController();
  final _search = TextEditingController();

  CodeHost? _host;
  List<HostedRepository>? _repos;
  Object? _reposError;
  bool _loadingRepos = false;

  /// Canonical slug per row, keyed by `owner/name`. A value of null means the
  /// row's clone URL has no GitHub identity — never a match (see [_isAdded]).
  Map<String, String?> _repoSlugs = const {};

  /// Canonical slugs of the registered projects. Only non-null slugs are ever
  /// inserted, so membership can never be satisfied by a null.
  Set<String> _addedSlugs = const {};

  /// The project origins [_addedSlugs] was built from, sorted, so a change-feed
  /// tick that touched only sessions doesn't re-run the whole resolution.
  ///
  /// Compared as a list rather than reduced to a joined string: any separator
  /// would have to be a character no origin URL can contain, which is a
  /// needlessly sharp thing to have to be right about.
  List<String>? _addedOrigins;

  /// Bumped per resolution so a slow one that has been superseded discards its
  /// result instead of overwriting a newer set.
  int _addedEpoch = 0;

  /// The clone being watched, or null when no job is in flight. Drives the
  /// progress banner.
  CloneJobDto? _job;

  /// Whether a clone flow is running, from the moment a row is tapped until the
  /// flow ends.
  ///
  /// Not derived from [_job], which is only set once `startClone` *returns*: the
  /// sheet pops before that call is made, leaving the page interactive for a
  /// round trip. A second tap in that window would start a second clone.
  bool _flowActive = false;

  CommanderStore get _store => widget.store;

  CodeHost get _effectiveHost =>
      _host ??
      CodeHost(
        provider:
            _store.snapshot?.server.codeHost.provider ??
            CodeHostProvider.github,
        hostname: _store.snapshot?.server.codeHost.hostname,
      );

  bool get _busy => _flowActive;

  @override
  void initState() {
    super.initState();
    // The badge depends on the project list, which may still be loading when
    // this page opens (the snapshot lands asynchronously). Listening
    // means the badges fill in when it arrives rather than being permanently
    // absent for anyone who got here quickly.
    _store.addListener(_onStoreChanged);
    _fetch();
    _syncAddedSlugs();
  }

  @override
  void dispose() {
    _store.removeListener(_onStoreChanged);
    _url.dispose();
    _search.dispose();
    super.dispose();
  }

  void _onStoreChanged() => _syncAddedSlugs();

  // --- data ---------------------------------------------------------------

  Future<void> _fetch() async {
    if (_loadingRepos) return;
    setState(() {
      _loadingRepos = true;
      _reposError = null;
    });
    try {
      final listing = await _store.repositories();
      final repos = listing.repositories;
      final slugs = <String, String?>{};
      for (final repo in repos) {
        slugs[repo.fullName] = await _store.canonicalRepoSlug(repo.cloneUrl);
      }
      if (!mounted) return;
      setState(() {
        _host = listing.host;
        _repos = repos;
        _repoSlugs = slugs;
        _reposError = null;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() => _reposError = e);
    } finally {
      if (mounted) setState(() => _loadingRepos = false);
    }
  }

  /// Rebuild [_addedSlugs] from the registered projects, if their origins have
  /// changed since the last pass.
  Future<void> _syncAddedSlugs() async {
    final origins = [
      for (final p in _store.projects)
        if (p.originUrl != null) p.originUrl!,
    ];
    origins.sort();
    if (listEquals(origins, _addedOrigins)) return;
    _addedOrigins = origins;
    final epoch = ++_addedEpoch;
    final slugs = <String>{};
    for (final origin in origins) {
      final slug = await _store.canonicalRepoSlug(origin);
      // Never insert null. A set containing null is precisely how a project
      // with no origin comes to "match" a source with no GitHub identity.
      //
      // Redundant with [_isAdded]'s null check by design — either alone prevents
      // the false badge, which is why no test can fail on this line being
      // relaxed on its own (verified by mutation). What *is* pinned is the pair:
      // `an origin that is present but unslugged is not a match` fails as soon as
      // both are gone.
      if (slug != null) slugs.add(slug);
    }
    if (!mounted || epoch != _addedEpoch) return;
    setState(() => _addedSlugs = slugs);
  }

  /// Whether this row is already registered as a project.
  bool _isAdded(HostedRepository repo) {
    final slug = _repoSlugs[repo.fullName];
    // A null slug means "no GitHub identity", not "an identity that happens to
    // be unknown". It can never match anything, including another null.
    if (slug == null) return false;
    return _addedSlugs.contains(slug);
  }

  /// The rows matching the search box, filtered locally over the fetched list.
  List<HostedRepository> get _filtered {
    final repos = _repos ?? const <HostedRepository>[];
    final query = _search.text.trim().toLowerCase();
    if (query.isEmpty) return repos;
    return [
      for (final repo in repos)
        if (repo.fullName.toLowerCase().contains(query) ||
            (repo.description?.toLowerCase().contains(query) ?? false))
          repo,
    ];
  }

  /// [repos] grouped under their owner. Insertion order is preserved on both
  /// levels, so owners appear in the order the server listed their repos
  /// (`sort=pushed`) and the account being worked in stays near the top.
  List<(String, List<HostedRepository>)> _grouped(
    List<HostedRepository> repos,
  ) {
    final groups = <String, List<HostedRepository>>{};
    for (final repo in repos) {
      groups.putIfAbsent(repo.namespace, () => []).add(repo);
    }
    return [for (final e in groups.entries) (e.key, e.value)];
  }

  // --- clone flow ---------------------------------------------------------

  Future<void> _cloneRepo(HostedRepository repo) {
    final host = _effectiveHost;
    final kind = host.provider == CodeHostProvider.gitlab
        ? CloneSourceKind.gitlab
        : CloneSourceKind.github;
    return _cloneFlow(
      CloneSourceDto(
        kind: kind,
        value: repo.fullName,
        hostname: kind == CloneSourceKind.gitlab ? host.hostname : null,
      ),
      repo.name,
    );
  }

  Future<void> _cloneUrl() async {
    // Checked before the field is cleared, so a submit that is going to be
    // refused doesn't discard what the user typed.
    if (_flowActive) return;
    final url = _url.text.trim();
    if (url.isEmpty) return;
    // Cleared before anything else: the value is captured here, and a pasted
    // credential must not remain on screen behind the sheet and the banner.
    _url.clear();
    // The last path segment, but only if it is a plainly safe directory name —
    // see [_safeDirName]. An empty prefill means "let the server derive it".
    final segment = url.split('/').last;
    final derived = _safeDirName.hasMatch(segment)
        ? (segment.endsWith('.git')
              ? segment.substring(0, segment.length - 4)
              : segment)
        : '';
    await _cloneFlow(
      CloneSourceDto(kind: CloneSourceKind.url, value: url, hostname: null),
      derived,
    );
  }

  /// Confirm the destination name, start the clone, and keep re-offering the
  /// sheet as long as the destination is occupied by something that isn't a
  /// checkout the user wants to adopt.
  Future<void> _cloneFlow(CloneSourceDto source, String initialName) async {
    if (_flowActive) return;
    // Claimed before the first await, so a second tap landing in any window
    // inside this flow is refused rather than starting a parallel clone.
    setState(() => _flowActive = true);
    try {
      var name = initialName;
      while (mounted) {
        final chosen = await _showDestSheet(name);
        if (chosen == null || !mounted) return; // cancelled
        name = chosen;
        if (await _startAndPoll(source, name) == _Attempt.done) return;
      }
    } finally {
      if (mounted) setState(() => _flowActive = false);
    }
  }

  /// The confirm sheet. Pops with the directory name, or null on cancel.
  Future<String?> _showDestSheet(String initialName) {
    final t = CommanderTokens.of(context);
    return showModalBottomSheet<String>(
      context: context,
      isScrollControlled: true,
      backgroundColor: t.canvasRaised,
      builder: (_) => _DestNameSheet(initialName: initialName),
    );
  }

  /// Start the clone and poll it to a terminal status.
  Future<_Attempt> _startAndPoll(CloneSourceDto source, String destName) async {
    final CloneJobDto started;
    try {
      started = await _store.startClone(
        CloneRequestDto(
          source: source,
          // Empty means "no override" — the server derives the name. A
          // whitespace-only entry would be refused with a 400, so normalise it
          // to the same thing.
          destName: destName.trim().isEmpty ? null : destName.trim(),
          workspace: widget.workspace,
        ),
      );
    } catch (e) {
      // The server redacts its own rejection messages where they are built, so
      // this is safe to show; nothing is added from `source`.
      _snack('Could not start the clone: ${errorText(e, capitalize: false)}');
      return _Attempt.done;
    }
    if (!mounted) return _Attempt.done;
    setState(() => _job = started);

    var current = started;
    try {
      while (true) {
        switch (current.status.kind) {
          case CloneStatusKind.running:
            await _store.waitForChange(_changeWaitFallback);
            if (!mounted) return _Attempt.done;
            final CloneJobDto? polled;
            try {
              polled = await _store.cloneJob(started.id);
            } catch (e) {
              _snack(
                'Lost track of the clone: ${errorText(e, capitalize: false)}',
              );
              return _Attempt.done;
            }
            if (!mounted) return _Attempt.done;
            if (polled == null) {
              // A pruned job is a normal answer, not a broken connection — but
              // it does mean we can no longer say how it went.
              _snack('The server no longer has this clone job.');
              return _Attempt.done;
            }
            current = polled;
            setState(() => _job = current);
          case CloneStatusKind.succeeded:
            // Refetch before popping so the list behind this page already holds
            // the new project, rather than filling in a poll interval later.
            await _store.refresh();
            if (!mounted) return _Attempt.done;
            Navigator.of(context).pop(true);
            return _Attempt.done;
          case CloneStatusKind.failed:
            _snack('Clone failed: ${current.status.message}');
            return _Attempt.done;
          case CloneStatusKind.destinationExists:
            // Nothing is cloning any more, so drop the progress row before the
            // dialog goes up rather than leaving it spinning behind it. The
            // `finally` below did exactly that while this arm returned an
            // un-awaited future; awaiting one moves it after the dialog, so
            // clear it here and let the `finally` be a no-op.
            if (mounted) setState(() => _job = null);
            return await _handleDestinationExists(current.status);
        }
      }
    } finally {
      if (mounted) setState(() => _job = null);
    }
  }

  /// Nothing was cloned because the destination was occupied. A git checkout
  /// there is worth offering to register; anything else means picking a name.
  Future<_Attempt> _handleDestinationExists(CloneStatusDto status) async {
    // Server-built absolute path, safe to render (the destination is; the source
    // is not). The DTO types it as nullable because only this one status arm
    // populates it, so a null here means the server told us the destination was
    // occupied without saying where — nothing to register, only a rename to
    // offer.
    final dest = status.dest;
    if (dest == null) {
      _snack('That destination already exists — choose another name.');
      return _Attempt.rename;
    }
    if (!status.isGitRepo) {
      _snack('$dest already exists — choose another directory name.');
      return _Attempt.rename;
    }
    final register = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Already cloned'),
        content: Text(
          '$dest is already a git repository. Register it as a project '
          'instead of cloning again?',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(false),
            child: const Text('Choose another name'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(ctx).pop(true),
            child: const Text('Register existing'),
          ),
        ],
      ),
    );
    if (!mounted) return _Attempt.done;
    if (register != true) return _Attempt.rename;
    try {
      await _store.ensureProject(dest, workspace: widget.workspace);
      if (!mounted) return _Attempt.done;
      Navigator.of(context).pop(true);
    } catch (e) {
      _snack('Could not register $dest: ${errorText(e, capitalize: false)}');
    }
    return _Attempt.done;
  }

  void _snack(String message) {
    if (!mounted) return;
    ScaffoldMessenger.of(
      context,
    ).showSnackBar(SnackBar(content: Text(message)));
  }

  // --- rendering ----------------------------------------------------------

  @override
  Widget build(BuildContext context) {
    return ChromePage(
      title: 'Clone repository',
      code: '47-G',
      body: Column(
        children: [
          _header(),
          if (_job != null) _progress(_job!),
          Expanded(child: _listArea()),
        ],
      ),
    );
  }

  Widget _header() {
    final t = CommanderTokens.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 12, 12, 4),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Expanded(
                child: TextField(
                  key: const Key('clone-url-field'),
                  controller: _url,
                  enabled: !_busy,
                  style: t.meta(size: 13, color: t.text),
                  decoration: InputDecoration(
                    labelText: 'Clone from URL',
                    hintText: _effectiveHost.provider == CodeHostProvider.gitlab
                        ? 'https://${_effectiveHost.hostname ?? 'gitlab.com'}/group/project.git'
                        : 'https://github.com/owner/repo.git',
                    isDense: true,
                  ),
                  onSubmitted: (_) => _busy ? null : _cloneUrl(),
                ),
              ),
              const SizedBox(width: 8),
              Padding(
                padding: const EdgeInsets.only(top: 4),
                child: FilledButton(
                  key: const Key('clone-url-submit'),
                  onPressed: _busy ? null : _cloneUrl,
                  child: const Text('Clone'),
                ),
              ),
            ],
          ),
          const SizedBox(height: 12),
          TextField(
            key: const Key('repo-search-field'),
            controller: _search,
            style: t.meta(size: 13, color: t.text),
            decoration: const InputDecoration(
              labelText: 'Search repositories',
              prefixIcon: Icon(Icons.search, size: 18),
              isDense: true,
            ),
            // Local filter over the already-fetched list: no request per
            // keystroke.
            onChanged: (_) => setState(() {}),
          ),
        ],
      ),
    );
  }

  Widget _progress(CloneJobDto job) {
    final t = CommanderTokens.of(context);
    return Container(
      width: double.infinity,
      margin: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: t.surface,
        border: Border.all(color: t.borderSubtle),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Row(
        children: [
          const SizedBox(
            width: 16,
            height: 16,
            child: CircularProgressIndicator(strokeWidth: 2),
          ),
          const SizedBox(width: 12),
          Expanded(
            // `sourceLabel` and `dest` both come from the server, which redacted
            // the label where it built it. The raw URL is never used here.
            child: Text(
              'Cloning ${job.sourceLabel} → ${job.dest}',
              maxLines: 2,
              overflow: TextOverflow.ellipsis,
              style: t.meta(size: 12, color: t.textBright),
            ),
          ),
        ],
      ),
    );
  }

  Widget _listArea() {
    if (_reposError != null) return _errorBanner(_reposError!);
    if (_repos == null) {
      return const Center(child: CircularProgressIndicator());
    }
    final rows = _filtered;
    return RefreshIndicator(
      onRefresh: _fetch,
      child: rows.isEmpty
          ? ListView(
              children: [
                const SizedBox(height: 80),
                Center(
                  child: Text(
                    _search.text.trim().isEmpty
                        ? (_effectiveHost.provider == CodeHostProvider.gitlab
                              ? 'No GitLab projects'
                              : 'No repositories')
                        : 'No repositories match "${_search.text.trim()}"',
                  ),
                ),
              ],
            )
          : ListView(
              padding: const EdgeInsets.only(bottom: 88),
              children: [
                for (final (owner, repos) in _grouped(rows)) ...[
                  _ownerHeading(owner),
                  for (final repo in repos)
                    _RepoRow(
                      key: Key('repo-row-${repo.fullName}'),
                      repo: repo,
                      added: _isAdded(repo),
                      onTap: _busy ? null : () => _cloneRepo(repo),
                    ),
                ],
              ],
            ),
    );
  }

  Widget _ownerHeading(String owner) {
    final t = CommanderTokens.of(context);
    return Padding(
      key: Key('owner-heading-$owner'),
      padding: const EdgeInsets.fromLTRB(16, 16, 16, 6),
      child: Text(t.caseLabel(owner), style: t.eyebrow()),
    );
  }

  /// The listing failed. Rendered inline rather than as a full-page error so the
  /// URL field above stays usable — the URL path never touches `gh`.
  Widget _errorBanner(Object error) {
    final t = CommanderTokens.of(context);
    final message = errorText(error);
    // A timeout on this route is nearly always the *listing* overrunning, not a
    // dead server, so word it that way. The server bounds provider pagination
    // (`repo_list_timeout_secs`, 90s by default —
    // `crates/claude-commander-core/src/git/bounded.rs`) and answers with
    // with a provider-aware timeout error
    // (`GitError::RepoListTimedOut`), while the client deliberately gives this
    // one route a longer budget than the server's so the server wins the race
    // and its real reason arrives instead of a transport timeout
    // (`REPO_LIST_HTTP_TIMEOUT_SECS` > `DEFAULT_REPO_LIST_TIMEOUT_SECS`,
    // compile-asserted in `protocol/src/github.rs`). The one case that still
    // reads as "the server is down" is a user who raises
    // `repo_list_timeout_secs` past the client budget — matching on the message
    // rather than on a variant covers both, since either way the text says
    // "timed out".
    final slow =
        message.toLowerCase().contains('timed out') ||
        message.toLowerCase().contains('timeout');
    return ListView(
      key: const Key('repo-list-error'),
      padding: const EdgeInsets.all(16),
      children: [
        Container(
          padding: const EdgeInsets.all(14),
          decoration: BoxDecoration(
            color: t.surface,
            border: Border.all(color: t.danger),
            borderRadius: BorderRadius.circular(6),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                children: [
                  Icon(Icons.error_outline, size: 18, color: t.danger),
                  const SizedBox(width: 8),
                  Text(
                    'Could not list repositories',
                    style: t.meta(size: 12, color: t.textBright),
                  ),
                ],
              ),
              const SizedBox(height: 8),
              Text(message, style: t.meta(size: 12, color: t.textMuted)),
              const SizedBox(height: 8),
              Text(
                slow
                    ? 'Listing a large account can take longer than the '
                          'request limit. The clone-from-URL field above still '
                          'works.'
                    : 'The picker needs `${_effectiveHost.provider == CodeHostProvider.gitlab ? 'glab' : 'gh'}` installed and authenticated on '
                          'the server. The clone-from-URL field above still '
                          'works without it.',
                style: t.meta(size: 11, color: t.textFaint),
              ),
              const SizedBox(height: 12),
              Align(
                alignment: Alignment.centerLeft,
                child: FilledButton.icon(
                  key: const Key('repo-list-retry'),
                  onPressed: _loadingRepos ? null : _fetch,
                  icon: const Icon(Icons.refresh, size: 18),
                  label: const Text('Try again'),
                ),
              ),
            ],
          ),
        ),
      ],
    );
  }
}

/// One repo row. An [added] row is greyed, badged and inert — its project
/// already exists, so cloning it again would only fail on an occupied
/// destination.
class _RepoRow extends StatelessWidget {
  final HostedRepository repo;
  final bool added;
  final VoidCallback? onTap;

  const _RepoRow({
    super.key,
    required this.repo,
    required this.added,
    required this.onTap,
  });

  @override
  Widget build(BuildContext context) {
    final t = CommanderTokens.of(context);
    final muted = added ? t.textFaint : null;
    return ListTile(
      dense: true,
      enabled: !added,
      onTap: added ? null : onTap,
      leading: Icon(
        repo.visibility == RepositoryVisibility.private
            ? Icons.lock_outline
            : Icons.folder_outlined,
        size: 18,
        color:
            muted ??
            (repo.visibility == RepositoryVisibility.private
                ? t.attention
                : t.textMuted),
      ),
      title: Text(
        repo.fullName,
        maxLines: 1,
        overflow: TextOverflow.ellipsis,
        style: TextStyle(color: muted ?? t.textBright),
      ),
      subtitle: repo.description == null
          ? null
          : Text(
              repo.description!,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: t.meta(size: 11, color: muted ?? t.textMuted),
            ),
      trailing: added
          ? Container(
              padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
              decoration: BoxDecoration(
                color: t.surfaceSelected,
                border: Border.all(color: t.borderSubtle),
                borderRadius: BorderRadius.circular(10),
              ),
              child: Text('Added', style: t.meta(size: 10, color: t.textMuted)),
            )
          : Text(
              '${repo.visibility.name}${repo.archived ? ' · archived' : ''}',
              style: t.meta(size: 10, color: t.textFaint),
            ),
    );
  }
}

/// The confirm sheet: an editable destination directory name.
///
/// The **name** is what this collects, not a full path: the projects directory it
/// lands in is server-side config that no route exposes to a client, so showing
/// an absolute path here would mean guessing one. The real destination is shown
/// as soon as the server reports it — in the progress banner, and in the
/// occupied-destination dialog.
///
/// Owns its controller and disposes it with its route, like `_PathPromptDialog`
/// on the projects page. Autofocuses so a re-open after an occupied destination
/// lands the caret in the field.
class _DestNameSheet extends StatefulWidget {
  final String initialName;

  const _DestNameSheet({required this.initialName});

  @override
  State<_DestNameSheet> createState() => _DestNameSheetState();
}

class _DestNameSheetState extends State<_DestNameSheet> {
  late final TextEditingController _name = TextEditingController(
    text: widget.initialName,
  );

  @override
  void dispose() {
    _name.dispose();
    super.dispose();
  }

  void _submit() => Navigator.of(context).pop(_name.text.trim());

  @override
  Widget build(BuildContext context) {
    final t = CommanderTokens.of(context);
    return SafeArea(
      child: Padding(
        padding: EdgeInsets.fromLTRB(
          16,
          16,
          16,
          16 + MediaQuery.viewInsetsOf(context).bottom,
        ),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Clone destination', style: t.display(size: 17)),
            const SizedBox(height: 4),
            Text(
              'Clones into the server\'s projects directory under this name.',
              style: t.meta(size: 11, color: t.textMuted),
            ),
            const SizedBox(height: 16),
            TextField(
              key: const Key('clone-dest-name-field'),
              controller: _name,
              autofocus: true,
              style: t.meta(size: 13, color: t.text),
              decoration: const InputDecoration(
                labelText: 'Directory name',
                isDense: true,
              ),
              onSubmitted: (_) => _submit(),
            ),
            const SizedBox(height: 20),
            Row(
              mainAxisAlignment: MainAxisAlignment.end,
              children: [
                TextButton(
                  onPressed: () => Navigator.of(context).pop(),
                  child: const Text('Cancel'),
                ),
                const SizedBox(width: 8),
                FilledButton.icon(
                  key: const Key('clone-confirm-button'),
                  onPressed: _submit,
                  icon: const Icon(Icons.download_outlined, size: 18),
                  label: const Text('Clone'),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }
}
