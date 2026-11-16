import 'package:flutter/material.dart';

import '../chrome/chrome_forms.dart';
import '../src/rust/api/mirrors.dart';
import '../state/commander_store.dart';
import '../state/commander_store_scope.dart';
import '../state/fleet_store.dart';
import '../theme/agent_glyphs.dart';
import '../theme/theme_controller.dart';
import '../theme/tokens.dart';
import '../util/error_text.dart';
import '../util/format.dart';
import '../util/session_filter.dart';
import '../widgets/session_chips.dart';
import '../widgets/workspace_menu.dart';
import 'create_session_page.dart';
import 'programs_page.dart';
import 'projects_page.dart';
import 'servers_page.dart';
import 'settings_page.dart';
import 'session_detail_page.dart';

/// Which slice of the sessions the list is showing: everything (grouped by
/// server → project) or the active ones in MRU order (most recently attached,
/// or created for one not yet attached).
enum _SessionView {
  recent('Recent'),
  all('All');

  const _SessionView(this.label);

  /// The slice's caption. LCARS uppercases it.
  final String label;
}

/// A one-tap "attention" filter layered on top of the search box: the sessions
/// that need input, the ones actively working, and the ones with an open PR
/// awaiting review. Mirrors the deck's quick-filter chip row.
enum _Quick { needsInput, working, review }

/// True when [store]'s session [s] is asking for a human decision — an agent
/// waiting for input, or a cascade paused mid-stack.
bool _isNeedsInput(CommanderStore store, SessionInfo s) =>
    s.status == SessionStatus.cascadePaused ||
    store.agentStateFor(s.id) == AgentState.waitingForInput;

/// True when [s]'s agent is actively working.
bool _isWorking(CommanderStore store, SessionInfo s) =>
    store.agentStateFor(s.id) == AgentState.working;

/// True when [s] carries an open PR — a candidate for review.
bool _isReview(SessionInfo s) =>
    s.prNumber != null && s.prState == PrState.open;

bool _matchesQuick(_Quick q, CommanderStore store, SessionInfo s) =>
    switch (q) {
      _Quick.needsInput => _isNeedsInput(store, s),
      _Quick.working => _isWorking(store, s),
      _Quick.review => _isReview(s),
    };

/// Whether a server's base URL points at the local machine (drives the
/// `local` / `remote` tag in the server node header). Compares the parsed
/// [Uri.host] so a name like `notlocalhost.example` isn't misread as local.
bool _isLocalServer(String baseUrl) {
  final host = Uri.tryParse(baseUrl)?.host.toLowerCase() ?? '';
  return host == 'localhost' ||
      host == '127.0.0.1' ||
      host == '::1' ||
      host == '[::1]';
}

/// The aggregated session list — layout-agnostic (no Scaffold, no route). It
/// describes itself to the chrome as a [ChromeViewRail]: a live search box
/// (fuzzy-filtering the list in place) as its filter, Recent/All as its slices,
/// and settings as its action — over a body of quick-filter chips and either the
/// servers' sessions grouped by project (All) or a flat, cross-server
/// most-recently-attached list (Recent). Enumerates the servers from the
/// [FleetStore]; in All mode each server section re-provides its own
/// [CommanderStoreScope] so per-server consumers (detail, cascade banner) keep
/// their single-store contract, and its header is suppressed when only one
/// server is configured.
class SessionListBody extends StatefulWidget {
  /// The id of the session shown in the detail pane, highlighted in the list.
  /// Null in the narrow (push) flow, where there is no persistent selection.
  final String? selectedId;

  /// Invoked when a session row is tapped, with the server that owns it.
  final void Function(CommanderStore store, SessionInfo session) onSelect;

  /// Whether the view frames itself with a [ChromeViewRail] — the branded "Fleet"
  /// header in Mission Control, the deck's elbow rail in LCARS. The phone
  /// [PhoneShell] turns this on; the wide layout's own chrome titles the fleet
  /// column and carries the nav, so it leaves this off and gets the controls
  /// alone.
  final bool showFleetHeader;

  const SessionListBody({
    super.key,
    this.selectedId,
    required this.onSelect,
    this.showFleetHeader = false,
  });

  @override
  State<SessionListBody> createState() => _SessionListBodyState();
}

class _SessionListBodyState extends State<SessionListBody> {
  final TextEditingController _search = TextEditingController();
  String _query = '';
  _SessionView _view = _SessionView.all;

  /// The active quick filter, or null for none. Tapping the active chip clears
  /// it, so it round-trips off.
  _Quick? _quick;

  @override
  void dispose() {
    _search.dispose();
    super.dispose();
  }

  void _setQuery(String value) => setState(() => _query = value.trim());

  void _toggleQuick(_Quick q) =>
      setState(() => _quick = _quick == q ? null : q);

  /// The active quick filter, or null when it no longer matches anything (its
  /// chip has dropped out of the row, so it can't be cleared by hand).
  _Quick? _liveQuick({
    required int needs,
    required int working,
    required int review,
  }) {
    final count = switch (_quick) {
      null => 0,
      _Quick.needsInput => needs,
      _Quick.working => working,
      _Quick.review => review,
    };
    return count > 0 ? _quick : null;
  }

  @override
  Widget build(BuildContext context) {
    final fleet = FleetScope.of(context)!;
    return ListenableBuilder(
      listenable: fleet,
      builder: (context, _) {
        final servers = fleet.servers;
        final multi = servers.length > 1;
        // Everything below is scoped to the active workspace: the counts, the
        // chips, and both views. Other workspaces surface only through the
        // title's switcher, with their waiting counts.
        final workspace = fleet.activeWorkspace;

        // A single cross-server pass powers the header counts and the chip row.
        var active = 0, total = 0, needs = 0, working = 0, review = 0;
        for (final store in servers) {
          for (final s in store.sessionsIn(workspace)) {
            total++;
            if (s.status.isActive) active++;
            if (_isNeedsInput(store, s)) needs++;
            if (_isWorking(store, s)) working++;
            if (_isReview(s)) review++;
          }
        }
        // A chip only renders while its count is non-zero, and tapping it is the
        // only way to clear the filter — so once the count reaches zero (the
        // agent stopped waiting, the PR merged) a still-applied filter is
        // unreachable, and every session stays hidden behind a bare "No
        // matches" with nothing on screen to explain why. Drop it instead. Safe
        // to assign during build: the value we render with is computed here, so
        // this needs no rebuild of its own.
        _quick = _liveQuick(needs: needs, working: working, review: review);

        // Everything under the controls, shared by both framings below: it ends
        // in an `Expanded`, so it is only ever spread into a `Column`.
        final tail = <Widget>[
          // The chips sat inside the controls column's own 16px inset; they keep
          // it here so the row lines up with the search box either way.
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16),
            child: _quickChips(needs: needs, working: working, review: review),
          ),
          // With several servers each group header carries its own connection
          // dot; a lone server has no group header (nor, in the phone shell,
          // an AppBar), so surface its connection state here when it isn't
          // healthy — otherwise a degraded/reconnecting sole server is silent.
          if (servers.length == 1 &&
              servers.single.connection.kind != ConnectionStateKind.connected)
            _ConnectionStrip(connection: servers.single.connection),
          Expanded(
            child: RefreshIndicator(
              onRefresh: fleet.refreshAll,
              child: _view == _SessionView.recent
                  ? _buildRecent(context, servers, workspace)
                  : _buildAll(servers, multi, workspace),
            ),
          ),
        ];

        if (!widget.showFleetHeader) {
          // The wide shell's fleet pane: its own chrome already titles the column
          // and carries the nav, so the view adds controls only.
          return Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Padding(
                padding: const EdgeInsets.fromLTRB(16, 6, 16, 0),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    ChromeField(_fieldSpec()),
                    const SizedBox(height: 9),
                    ChromeSegmented(_sliceSpec()),
                  ],
                ),
              ),
              ...tail,
            ],
          );
        }

        return ChromeViewRail(
          ChromeViewRailSpec(
            code: '47-A',
            title: 'Fleet',
            titleMenu: workspaceTitleMenu(fleet, theme: ThemeScope.of(context)),
            subtitle:
                '$active active · $total total · ${servers.length} '
                'server${servers.length == 1 ? '' : 's'}',
            filter: _fieldSpec(),
            slices: _sliceSpec(),
            body: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: tail,
            ),
          ),
        );
      },
    );
  }

  /// The live search box. The clear affordance keys off the controller's own text
  /// rather than [_query], so a whitespace-only query (which trims to empty) can
  /// still be cleared.
  ChromeFieldSpec _fieldSpec() => ChromeFieldSpec(
    controller: _search,
    icon: Icons.search,
    hint: 'Filter by name, branch, program…',
    onChanged: _setQuery,
    textInputAction: TextInputAction.search,
    onClear: _search.text.isEmpty
        ? null
        : () {
            _search.clear();
            _setQuery('');
          },
  );

  /// The Recent/All slices, noting how the active one is ordered.
  ChromeSegmentedSpec _sliceSpec() => ChromeSegmentedSpec(
    segments: [
      for (final view in _SessionView.values)
        ChromeSegment(
          label: view.label,
          selected: _view == view,
          onTap: () => setState(() => _view = view),
        ),
    ],
    note: _view == _SessionView.recent ? '↓ recency' : 'grouped',
  );

  /// The quick-filter chip row. Only chips with a non-zero count show, so a
  /// filter never dead-ends on an empty set. The needs-input chip always takes
  /// the attention tint (it's the one that wants attention).
  Widget _quickChips({
    required int needs,
    required int working,
    required int review,
  }) {
    final chips = <Widget>[
      if (needs > 0)
        _QuickChip(
          label: '? needs input · $needs',
          amber: true,
          selected: _quick == _Quick.needsInput,
          onTap: () => _toggleQuick(_Quick.needsInput),
        ),
      if (working > 0)
        _QuickChip(
          label: 'working · $working',
          selected: _quick == _Quick.working,
          onTap: () => _toggleQuick(_Quick.working),
        ),
      if (review > 0)
        _QuickChip(
          label: 'review · $review',
          selected: _quick == _Quick.review,
          onTap: () => _toggleQuick(_Quick.review),
        ),
    ];
    if (chips.isEmpty) return const SizedBox(height: 10);
    return Padding(
      padding: const EdgeInsets.only(top: 10, bottom: 2),
      child: SizedBox(
        height: 26,
        child: ListView.separated(
          scrollDirection: Axis.horizontal,
          itemCount: chips.length,
          separatorBuilder: (_, _) => const SizedBox(width: 6),
          itemBuilder: (_, i) => chips[i],
        ),
      ),
    );
  }

  Widget _buildAll(
    List<CommanderStore> servers,
    bool multi,
    String? workspace,
  ) {
    final rows = <(String, Widget Function(BuildContext))>[];
    for (final store in servers) {
      final prefix = store.config.id;
      if (store.snapshot == null) {
        rows.add((
          '$prefix/loading',
          (context) => _ServerSection(
            store: store,
            workspace: workspace,
            showHeader: multi,
          ),
        ));
        continue;
      }
      if (multi) {
        rows.add((
          '$prefix/header',
          (context) => _ServerHeader(
            store: store,
            count: store.sessionsIn(workspace).length,
          ),
        ));
      }
      if (store.cascadePaused != null) {
        rows.add((
          '$prefix/cascade',
          (context) =>
              CommanderStoreScope(store: store, child: CascadeBanner()),
        ));
      }
      var matches = 0;
      for (final group in store.sessionsByProjectIn(workspace)) {
        final sessions = matchingSessions(group.sessions, _query)
            .where(
              (session) =>
                  (_quick == null || _matchesQuick(_quick!, store, session)),
            )
            .toList();
        if (sessions.isEmpty) continue;
        matches += sessions.length;
        rows.add((
          '$prefix/project/${group.project.id}',
          (context) => ChromeEyebrow(
            '${group.project.name.toUpperCase()} · ${sessions.length}',
          ),
        ));
        for (final (i, session) in sessions.indexed) {
          rows.add((
            '$prefix/session/${session.id}',
            (context) => Padding(
              padding: const EdgeInsets.fromLTRB(14, 0, 12, 6),
              child: _groupedRow(
                context,
                store: store,
                session: session,
                selected: session.id == widget.selectedId,
                position: _rowPosition(i, sessions.length),
                onTap: () => widget.onSelect(store, session),
              ),
            ),
          ));
        }
      }
      if (matches == 0) {
        rows.add((
          '$prefix/empty',
          (context) => _InlineNote(
            icon: _query.isNotEmpty || _quick != null
                ? Icons.search_off
                : Icons.inbox_outlined,
            text: _query.isNotEmpty || _quick != null
                ? 'No matches'
                : 'No sessions',
          ),
        ));
      }
    }
    return ListView.builder(
      padding: const EdgeInsets.only(top: 6, bottom: 12),
      itemCount: rows.length,
      itemBuilder: (context, index) => KeyedSubtree(
        key: ValueKey(rows[index].$1),
        child: rows[index].$2(context),
      ),
    );
  }

  /// The Recent tab: every server's sessions flattened to (store, session)
  /// pairs, active-and-attached-only, newest-attach first (the TUI's MRU
  /// order). A live query filters that set by fuzzy score, ranking best matches
  /// first while keeping recency as the stable tie-break; an active quick filter
  /// narrows it further.
  ///
  /// Two deliberate differences from the TUI's pinned recents block, and they
  /// go together: this tab is uncapped (the TUI caps at `recent_sessions_limit`)
  /// *and* it excludes stopped sessions (the TUI shows any attached session
  /// regardless of status). The TUI's fixed cap already bounds how much stale
  /// history shows; an uncapped list has no such bound, so it filters to active
  /// sessions to avoid accumulating dead ones indefinitely.
  ///
  /// A third: the ordering key is [sessionRecency], not `lastAttachedAt`, so a
  /// never-attached session sorts by its creation time instead of dropping out.
  /// The TUI can drop those, because its recents block is pinned *above* the
  /// full tree; here Recent is one of two exclusive tabs, so dropping a session
  /// hides it outright — including the one the user has only just created.
  Widget _buildRecent(
    BuildContext context,
    List<CommanderStore> servers,
    String? workspace,
  ) {
    var pairs = <(CommanderStore, SessionInfo)>[
      for (final store in servers)
        for (final s in store.sessionsIn(workspace))
          if (s.status.isActive &&
              (_quick == null || _matchesQuick(_quick!, store, s)))
            (store, s),
    ];
    pairs = mostRecent(pairs, (p) => sessionRecency(p.$2));
    if (_query.isNotEmpty) {
      pairs = rankByScore(pairs, (p) => sessionFuzzyScore(p.$2, _query));
    }

    if (pairs.isEmpty) {
      return ListView(children: [_recentEmptyState(context, servers)]);
    }
    return ListView.builder(
      padding: const EdgeInsets.fromLTRB(12, 2, 12, 12),
      itemCount: pairs.length,
      itemBuilder: (context, i) {
        final (store, session) = pairs[i];
        return KeyedSubtree(
          key: ValueKey('${store.config.id}/${session.id}'),
          child: _recentRow(
            context,
            store: store,
            session: session,
            selected: session.id == widget.selectedId,
            position: _rowPosition(i, pairs.length),
            onTap: () => widget.onSelect(store, session),
          ),
        );
      },
    );
  }

  /// What to show when the flattened recent list is empty. A bare "No recent
  /// sessions" would hide a server that is merely still connecting or down, so
  /// mirror All mode: surface a spinner while any server is loading and an
  /// error+Retry when one has failed, before falling back to the empty note.
  Widget _recentEmptyState(BuildContext context, List<CommanderStore> servers) {
    // Loading/error take priority over the query notes, so typing while the
    // only server is still connecting shows the spinner (as All mode does),
    // not a misleading "No matches".
    final loading = servers.any((s) => s.snapshot == null && s.error == null);
    if (loading) {
      return const Padding(
        padding: EdgeInsets.symmetric(vertical: 24),
        child: Center(child: CircularProgressIndicator()),
      );
    }
    final failed = servers.where((s) => s.error != null).toList();
    if (failed.isNotEmpty) {
      final store = failed.first;
      return _InlineNote(
        icon: Icons.cloud_off,
        text: errorText(store.error!),
        action: ('Retry', store.retry),
        color: CommanderTokens.of(context).danger,
      );
    }
    if (_query.isNotEmpty || _quick != null) {
      return const _InlineNote(icon: Icons.search_off, text: 'No matches');
    }
    return const _InlineNote(icon: Icons.history, text: 'No recent sessions');
  }
}

/// A tappable quick-filter pill. Selected fills with its active colour (the
/// attention tint for the needs-input chip, the primary accent otherwise); an
/// unselected needs-input chip keeps a faint attention tint, other unselected
/// chips are a neutral surface pill.
class _QuickChip extends StatelessWidget {
  final String label;
  final bool selected;
  final bool amber;
  final VoidCallback onTap;

  const _QuickChip({
    required this.label,
    required this.selected,
    required this.onTap,
    this.amber = false,
  });

  @override
  Widget build(BuildContext context) {
    final t = CommanderTokens.of(context);
    final active = amber ? t.attention : t.primary;
    final Color bg, borderColor, textColor;
    if (selected) {
      bg = active.withValues(alpha: 0.2);
      borderColor = active.withValues(alpha: 0.7);
      textColor = amber ? t.attentionOn : t.primarySoft;
    } else if (amber) {
      bg = t.attention.withValues(alpha: 0.14);
      borderColor = t.attention.withValues(alpha: 0.4);
      textColor = t.attentionOn;
    } else {
      bg = t.surface;
      borderColor = t.border;
      textColor = t.textMuted;
    }
    return Material(
      color: Colors.transparent,
      child: InkWell(
        onTap: onTap,
        borderRadius: BorderRadius.circular(20),
        child: Container(
          alignment: Alignment.center,
          padding: const EdgeInsets.symmetric(horizontal: 11),
          decoration: BoxDecoration(
            color: bg,
            borderRadius: BorderRadius.circular(20),
            border: Border.all(color: borderColor),
          ),
          child: Text(
            label,
            style: t.meta(size: 10, weight: FontWeight.w600, color: textColor),
          ),
        ),
      ),
    );
  }
}

/// One server's slice of the aggregated list: an optional header, a paused-
/// cascade banner, and its project-grouped session tiles — all under that
/// server's [CommanderStoreScope] so the banner and pushed routes resolve to it.
class _ServerSection extends StatelessWidget {
  final CommanderStore store;

  /// The active workspace (null = Main): only its projects are listed.
  final String? workspace;
  final bool showHeader;
  const _ServerSection({
    required this.store,
    required this.workspace,
    required this.showHeader,
  });

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: store,
      builder: (context, _) => CommanderStoreScope(
        store: store,
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          mainAxisSize: MainAxisSize.min,
          children: [
            if (showHeader)
              _ServerHeader(
                store: store,
                count: store.sessionsIn(workspace).length,
              ),
            ..._content(context),
          ],
        ),
      ),
    );
  }

  List<Widget> _content(BuildContext context) {
    if (store.snapshot == null) {
      // This server hasn't loaded yet (or failed) — show a compact per-server
      // state so a slow/down server never blanks the whole list.
      if (store.error != null) {
        return [
          _InlineNote(
            icon: Icons.cloud_off,
            text: errorText(store.error!),
            action: ('Retry', store.retry),
            color: CommanderTokens.of(context).danger,
          ),
        ];
      }
      return const [
        Padding(
          padding: EdgeInsets.symmetric(vertical: 24),
          child: Center(child: CircularProgressIndicator()),
        ),
      ];
    }
    return const [];
  }
}

/// Push a session's detail route, re-providing the owning server's scope so the
/// detail page's markRead/cascade/terminal/review calls hit the right server.
/// Shared by the redesigned [PhoneShell] and the wide shell's list body.
Future<void> openSessionDetail(
  BuildContext context,
  CommanderStore store,
  SessionInfo session,
) async {
  await Navigator.of(context).push<bool>(
    MaterialPageRoute(
      builder: (_) => CommanderStoreScope(
        store: store,
        child: SessionDetailPage(session: session),
      ),
    ),
  );
  // A lifecycle action bumps the change feed, so the list refreshes itself.
}

/// Resolve the server to act on for a per-server action (create/projects/
/// programs). Returns it directly when there is one server; otherwise prompts.
/// Null means "no server / user cancelled".
Future<CommanderStore?> pickServer(
  BuildContext context,
  FleetStore fleet, {
  String title = 'Choose a server',
}) async {
  final servers = fleet.servers;
  if (servers.isEmpty) return null;
  if (servers.length == 1) return servers.single;
  return showModalBottomSheet<CommanderStore>(
    context: context,
    builder: (context) => SafeArea(
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Padding(
            padding: const EdgeInsets.all(16),
            child: Text(title, style: Theme.of(context).textTheme.titleMedium),
          ),
          for (final store in servers)
            ListTile(
              leading: const Icon(Icons.dns_outlined),
              title: Text(store.config.name),
              subtitle: Text(
                store.config.baseUrl,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
              ),
              onTap: () => Navigator.of(context).pop(store),
            ),
        ],
      ),
    ),
  );
}

/// Push the create-session route for a chosen server. Shared by both layouts.
/// The page refetches the snapshot itself before popping, so this route returns
/// to a list that already holds the new session.
Future<void> openCreateSession(BuildContext context, FleetStore fleet) async {
  final store = await pickServer(context, fleet, title: 'Create on…');
  if (store == null || store.handle == null || !context.mounted) return;
  await Navigator.of(context).push<String>(
    MaterialPageRoute(
      builder: (_) =>
          CreateSessionPage(store: store, workspace: fleet.activeWorkspace),
    ),
  );
}

/// Push the servers manager (add/edit/remove).
void openServers(BuildContext context, FleetStore fleet) {
  Navigator.of(
    context,
  ).push(MaterialPageRoute(builder: (_) => ServersPage(fleet: fleet)));
}

/// Push the program-list editor for a chosen server (`PUT /api/config/programs`).
Future<void> openPrograms(BuildContext context, FleetStore fleet) async {
  final store = await pickServer(context, fleet, title: 'Programs on…');
  final handle = store?.handle;
  if (store == null || handle == null || !context.mounted) return;
  Navigator.of(context).push(
    MaterialPageRoute(
      builder: (_) => ProgramsPage(api: store.api, handle: handle),
    ),
  );
}

/// Push the projects manager for a chosen server (add/remove/scan + branches).
Future<void> openProjects(BuildContext context, FleetStore fleet) async {
  final store = await pickServer(context, fleet, title: 'Projects on…');
  if (store == null || store.handle == null || !context.mounted) return;
  Navigator.of(context).push(
    MaterialPageRoute(
      builder: (_) =>
          ProjectsPage(store: store, workspace: fleet.activeWorkspace),
    ),
  );
}

/// Opens the [SettingsPage].
void openSettings(BuildContext context) => Navigator.of(
  context,
).push(MaterialPageRoute(builder: (_) => const SettingsPage()));

/// A slim in-body status strip for the lone-server case, shown while connecting
/// or degraded (a healthy connection needs no chrome). Rendered by
/// [SessionListBody] between the controls and the list, so a sole server's
/// connection state is visible in both the Recent and All views even without an
/// AppBar or a per-server group header.
class _ConnectionStrip extends StatelessWidget {
  final ConnectionStateDto connection;
  const _ConnectionStrip({required this.connection});

  @override
  Widget build(BuildContext context) {
    final t = CommanderTokens.of(context);
    final (label, color) = switch (connection.kind) {
      // The call site only renders this strip when kind != connected, so this
      // arm is for exhaustiveness — the strip never shows for a healthy server.
      ConnectionStateKind.connected => ('Connected', t.working),
      ConnectionStateKind.connecting => ('Connecting…', t.attention),
      ConnectionStateKind.degraded => (
        connection.reason.isEmpty
            ? 'Connection degraded'
            : 'Degraded: ${errorText(connection.reason, capitalize: false)}',
        t.danger,
      ),
    };
    return Container(
      width: double.infinity,
      margin: const EdgeInsets.fromLTRB(16, 8, 16, 0),
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 7),
      decoration: BoxDecoration(
        color: color.withValues(alpha: 0.12),
        borderRadius: BorderRadius.circular(9),
        border: Border.all(color: color.withValues(alpha: 0.4)),
      ),
      child: Row(
        children: [
          Container(
            width: 8,
            height: 8,
            decoration: BoxDecoration(color: color, shape: BoxShape.circle),
          ),
          const SizedBox(width: 9),
          Expanded(
            child: Text(
              label,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: t.meta(size: 10.5, weight: FontWeight.w600, color: color),
            ),
          ),
        ],
      ),
    );
  }
}

/// A prominent banner shown while a cascade is paused awaiting a decision. It
/// offers Resume (which continues the cascade and reports the next outcome) and
/// Abandon (which leaves the stack where it stopped). Owns its own busy guard so
/// a double-tap can't fire twice. Reads the server from the enclosing scope, so
/// it acts on the server whose group it is rendered in.
class CascadeBanner extends StatefulWidget {
  const CascadeBanner({super.key});

  @override
  State<CascadeBanner> createState() => _CascadeBannerState();
}

class _CascadeBannerState extends State<CascadeBanner> {
  bool _busy = false;

  Future<void> _run(Future<void> Function(CommanderStore store) action) async {
    final store = CommanderStoreScope.of(context);
    if (store == null || _busy) return;
    setState(() => _busy = true);
    try {
      await action(store);
    } catch (e) {
      if (!mounted) return;
      ScaffoldMessenger.of(
        context,
      ).showSnackBar(SnackBar(content: Text('Failed: $e')));
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  Future<void> _resume() => _run((store) async {
    final status = await store.cascadeResume();
    if (!mounted) return;
    ScaffoldMessenger.of(
      context,
    ).showSnackBar(SnackBar(content: Text(describeOperation(status))));
  });

  Future<void> _abandon() => _run((store) async {
    await store.cascadeAbandon();
    if (!mounted) return;
    ScaffoldMessenger.of(
      context,
    ).showSnackBar(const SnackBar(content: Text('Cascade abandoned')));
  });

  @override
  Widget build(BuildContext context) {
    final t = CommanderTokens.of(context);
    return Container(
      margin: const EdgeInsets.fromLTRB(12, 12, 12, 4),
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: t.held.withValues(alpha: 0.09),
        borderRadius: BorderRadius.circular(12),
        border: Border.all(color: t.held.withValues(alpha: 0.4)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Icon(Icons.pause_circle_outline, color: t.held),
              const SizedBox(width: 8),
              Expanded(
                child: Text(
                  'Cascade paused — awaiting a decision',
                  style: Theme.of(
                    context,
                  ).textTheme.titleSmall?.copyWith(color: t.attentionOn),
                ),
              ),
            ],
          ),
          const SizedBox(height: 10),
          Wrap(
            spacing: 8,
            children: [
              FilledButton.icon(
                onPressed: _busy ? null : _resume,
                icon: const Icon(Icons.play_arrow),
                label: const Text('Resume'),
              ),
              OutlinedButton.icon(
                onPressed: _busy ? null : _abandon,
                icon: const Icon(Icons.close),
                label: const Text('Abandon'),
              ),
            ],
          ),
        ],
      ),
    );
  }
}

/// A server-group header (the deck's "server node"): a live connection dot, the
/// server name, and a `N · local/remote` count. A degraded server is greyed +
/// dimmed with an "unreachable" note in place of the count (mirrors the TUI), so
/// a down server reads as inert but never vanishes from the list.
class _ServerHeader extends StatelessWidget {
  final CommanderStore store;

  /// The server's session count in the active workspace.
  final int count;
  const _ServerHeader({required this.store, required this.count});

  @override
  Widget build(BuildContext context) {
    final t = CommanderTokens.of(context);
    final conn = store.connection;
    final (dotColor, note, noteColor, degraded) = switch (conn.kind) {
      ConnectionStateKind.connected => (t.working, null, null, false),
      ConnectionStateKind.connecting => (
        t.attention,
        'connecting…',
        t.attentionOn,
        false,
      ),
      ConnectionStateKind.degraded => (
        t.idle,
        conn.reason.isEmpty
            ? 'unreachable'
            : errorText(conn.reason, capitalize: false, maxLength: 40),
        t.danger,
        true,
      ),
    };
    final tag = _isLocalServer(store.config.baseUrl) ? 'local' : 'remote';
    return Opacity(
      opacity: degraded ? 0.6 : 1,
      child: Container(
        padding: const EdgeInsets.fromLTRB(4, 10, 8, 8),
        decoration: BoxDecoration(
          border: Border(bottom: BorderSide(color: t.divider)),
        ),
        child: Row(
          children: [
            Container(
              width: 8,
              height: 8,
              decoration: BoxDecoration(
                color: dotColor,
                shape: BoxShape.circle,
                boxShadow: degraded
                    ? null
                    : [BoxShadow(color: dotColor, blurRadius: 7)],
              ),
            ),
            const SizedBox(width: 9),
            // The name owns the majority of the row and the note is capped at
            // the rest: a degraded server's reason is the longest thing that can
            // land in that slot, and with the note unbounded it crushed the name
            // to a stub ("192…") on a phone. Both ellipsize, so neither can win
            // the whole row.
            Expanded(
              flex: 3,
              child: Text(
                store.config.name,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(
                  fontSize: 13,
                  fontWeight: FontWeight.w600,
                  color: t.text,
                ),
              ),
            ),
            const SizedBox(width: 8),
            Flexible(
              flex: 2,
              child: Text(
                note ?? '$count · $tag',
                maxLines: 1,
                textAlign: TextAlign.end,
                overflow: TextOverflow.ellipsis,
                style: t.meta(size: 10, color: noteColor ?? t.textMuted),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// A compact inline note (loading-failed / empty) rendered inside a server
/// section, with an optional action button.
class _InlineNote extends StatelessWidget {
  final IconData icon;
  final String text;
  final (String, Future<void> Function())? action;
  final Color? color;
  const _InlineNote({
    required this.icon,
    required this.text,
    this.action,
    this.color,
  });

  @override
  Widget build(BuildContext context) {
    final t = CommanderTokens.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 24, vertical: 16),
      child: Column(
        children: [
          Icon(icon, color: color ?? t.textFaint),
          const SizedBox(height: 8),
          Text(
            text,
            textAlign: TextAlign.center,
            // Belt and braces on top of `errorText`: whatever lands here, the
            // note stays a note rather than pushing the list off screen.
            maxLines: 3,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(color: t.textMuted),
          ),
          if (action != null) ...[
            const SizedBox(height: 8),
            FilledButton.icon(
              onPressed: action!.$2,
              icon: const Icon(Icons.refresh),
              label: Text(action!.$1),
            ),
          ],
        ],
      ),
    );
  }
}

/// Where a row sits in a run of [count] rows. LCARS rounds a run's outer
/// corners so it reads as one bracketed cluster; Mission Control ignores it.
ChromeRowPosition _rowPosition(int index, int count) {
  if (count == 1) return ChromeRowPosition.only;
  if (index == 0) return ChromeRowPosition.first;
  if (index == count - 1) return ChromeRowPosition.last;
  return ChromeRowPosition.middle;
}

/// A dense MRU row for the Recent tab: the state glyph, the title, a
/// `state · project · server` metadata line, and a trailing PR badge + relative
/// age.
///
/// The subtitle is load-bearing beyond its content: it is what selects Mission
/// Control's divider-ruled row shape over its carded one, so this row always
/// passes one.
Widget _recentRow(
  BuildContext context, {
  required CommanderStore store,
  required SessionInfo session,
  required bool selected,
  required ChromeRowPosition position,
  required VoidCallback onTap,
}) {
  final t = CommanderTokens.of(context);
  final descriptor = sessionDescriptor(
    session,
    store.agentStateFor(session.id),
  );
  final age = relativeAge(session.lastAttachedAt ?? session.createdAt);
  final pr = session.prNumber;
  return ChromeListRow(
    ChromeListRowSpec(
      title: session.title,
      subtitle:
          '${descriptor.label} · ${session.projectName} · ${store.config.name}',
      tone: descriptor.tone,
      glyph: SessionGlyph(descriptor),
      number: lcarsRowNumber(session.id),
      selected: selected,
      position: position,
      onTap: onTap,
      // This row wants a PR badge *and* the age, but `trailingWidget`
      // supersedes `trailing` rather than sitting beside it — so when there is
      // a PR the pair is composed here instead. The gap and the age's style are
      // the ones the chrome's own trailing slot uses, which is what keeps a PR
      // row and a PR-less one identical apart from the badge. Dropping the age
      // instead would have been a visible change to Mission Control.
      trailing: pr == null ? age : null,
      trailingWidget: pr == null
          ? null
          : Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                prChip(context, pr, session.prState),
                const SizedBox(width: 8),
                Text(age, style: t.meta(size: 10, color: t.textFaint)),
              ],
            ),
    ),
  );
}

/// A session row for the grouped All view: the state glyph, the title, and a
/// trailing PR badge (or the state word when there's no PR).
///
/// Deliberately **no subtitle**: that is what selects Mission Control's carded
/// row shape. See [_recentRow].
Widget _groupedRow(
  BuildContext context, {
  required CommanderStore store,
  required SessionInfo session,
  required bool selected,
  required ChromeRowPosition position,
  required VoidCallback onTap,
}) {
  final descriptor = sessionDescriptor(
    session,
    store.agentStateFor(session.id),
  );
  final pr = session.prNumber;
  return ChromeListRow(
    ChromeListRowSpec(
      title: session.title,
      tone: descriptor.tone,
      glyph: SessionGlyph(descriptor, width: 12),
      number: lcarsRowNumber(session.id),
      selected: selected,
      position: position,
      onTap: onTap,
      trailing: pr == null ? descriptor.label : null,
      trailingWidget: pr == null ? null : prChip(context, pr, session.prState),
    ),
  );
}
