import 'package:flutter/material.dart';

import '../chrome/chrome.dart';
import '../chrome/chrome_forms.dart';
import '../src/rust/api/mirrors.dart';
import '../state/commander_store.dart';
import '../state/commander_store_scope.dart';
import '../theme/tokens.dart';
import '../util/error_text.dart';
import '../widgets/session_chips.dart';
import 'review_page.dart';
import 'edit_session_dialog.dart';
import 'terminal_page.dart';

/// The low-frequency management actions, tucked into the detail header's
/// overflow (⋮) menu rather than spending a button each.
enum _ManageAction { edit }

/// Detail view for a single session, layout-agnostic (no Scaffold, no route).
/// Live status and agent state come straight from the [CommanderStore] (refreshed
/// off the change feed — no local timer); the pane snapshot and diff stat, which
/// the snapshot doesn't carry, are fetched on demand and re-fetched
/// whenever the store ticks.
///
/// The narrow [SessionDetailPage] wraps this in a Scaffold and pushes
/// terminal/review routes; the wide shell places it in a detail pane whose
/// terminal/review views are tabs, so the open/dismiss actions are supplied as
/// callbacks rather than owned here.
class SessionDetailBody extends StatefulWidget {
  /// The session to show. The list already has this, so the body renders
  /// immediately and refines it with store-fed and fetched detail.
  final SessionInfo session;

  /// Open a live attach of the given [AttachKind] (agent pane or paired shell).
  /// Narrow: push a route; wide: switch the pane tab.
  final ValueChanged<AttachKind> onOpenTerminal;

  /// Open the review view (narrow: push a route; wide: switch the pane tab).
  final VoidCallback onOpenReview;

  /// Called after a successful delete (narrow: pop the route; wide: clear the
  /// selection).
  final VoidCallback onDeleted;

  /// Called from the gone-state's dismiss button (narrow: pop; wide: clear).
  final VoidCallback onDismiss;

  /// Whether to render the on-demand terminal-snapshot preview card. Phones
  /// hide it (the live terminal is one tap away and far more useful in a small
  /// viewport); the wide landscape layout keeps it. When false, the detail
  /// fetch also skips capturing pane lines, so the server does no tmux capture.
  final bool showPanePreview;

  const SessionDetailBody({
    super.key,
    required this.session,
    required this.onOpenTerminal,
    required this.onOpenReview,
    required this.onDeleted,
    required this.onDismiss,
    required this.showPanePreview,
  });

  @override
  State<SessionDetailBody> createState() => _SessionDetailBodyState();
}

class _SessionDetailBodyState extends State<SessionDetailBody> {
  CommanderStore? _store;
  SessionDetail? _detail;
  String? _error;

  /// Set once a detail fetch returns null (404 → session gone). Rendering
  /// switches to the gone-state and no further detail is fetched.
  bool _gone = false;

  /// Set while a lifecycle action is in flight, to disable the buttons and skip
  /// detail fetches so a refresh doesn't race the mutation.
  bool _busy = false;

  /// Guards against overlapping detail fetches when the store ticks rapidly.
  bool _fetching = false;

  bool _fetchedOnce = false;

  /// Set once we've asked the server to mark this session read, so opening the
  /// body only fires [CommanderStore.markRead] once (and never for an
  /// already-read session). Reset when the pane is reused for another session.
  bool _markReadRequested = false;

  String get _id => widget.session.id;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final store = CommanderStoreScope.of(context);
    if (!identical(store, _store)) {
      _store?.removeListener(_onStoreChanged);
      _store = store;
      _store?.addListener(_onStoreChanged);
    }
    if (!_fetchedOnce) {
      _fetchedOnce = true;
      _fetchDetail();
      _maybeMarkRead();
    }
  }

  @override
  void didUpdateWidget(covariant SessionDetailBody old) {
    super.didUpdateWidget(old);
    // The wide pane reuses this State for a newly selected session; reset the
    // per-session fetch state and pull fresh detail.
    if (old.session.id != widget.session.id) {
      _detail = null;
      _error = null;
      _gone = false;
      _busy = false;
      _markReadRequested = false;
      _fetchDetail();
      _maybeMarkRead();
    }
  }

  /// Mark the session read when its detail is opened — but only once, and only
  /// when it is currently unread, so we don't spam the server on every rebuild.
  Future<void> _maybeMarkRead() async {
    final store = _store;
    if (store == null || _markReadRequested) return;
    if (!_info.unread) return;
    _markReadRequested = true;
    try {
      await store.markRead(_id);
    } catch (_) {
      // Best-effort: a failed mark-read just leaves the dot; not worth a toast.
    }
  }

  @override
  void dispose() {
    _store?.removeListener(_onStoreChanged);
    super.dispose();
  }

  void _onStoreChanged() {
    if (!mounted) return;
    // Rebuild to pick up the store's fresh session info / agent state, and pull
    // the on-demand detail (pane/diff) back in sync.
    setState(() {});
    _fetchDetail();
  }

  Future<void> _fetchDetail() async {
    final store = _store;
    if (store == null || _busy || _fetching || _gone) return;
    _fetching = true;
    try {
      // Only capture pane lines when the preview card will render them; a null
      // `lines` tells the server to skip the tmux capture entirely.
      final detail = await store.sessionDetail(
        _id,
        lines: widget.showPanePreview ? 200 : null,
      );
      if (!mounted) return;
      if (detail == null) {
        setState(() {
          _gone = true;
          _error = null;
        });
        return;
      }
      setState(() {
        _detail = detail;
        _error = null;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() => _error = errorText(e));
    } finally {
      _fetching = false;
    }
  }

  Future<void> _refresh() async {
    await _store?.refresh();
    await _fetchDetail();
  }

  /// Show a confirm dialog and resolve to true only if the user confirms.
  Future<bool> _confirm({
    required String title,
    required String message,
    required String confirmLabel,
    Color? confirmColor,
  }) async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(title),
        content: Text(message),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            style: confirmColor == null
                ? null
                : FilledButton.styleFrom(backgroundColor: confirmColor),
            onPressed: () => Navigator.of(ctx).pop(true),
            child: Text(confirmLabel),
          ),
        ],
      ),
    );
    return confirmed == true;
  }

  /// Run a lifecycle action with a confirm dialog, a busy guard, and a
  /// success/failure snackbar. `leaveOnSuccess` invokes [widget.onDeleted] (for
  /// delete).
  Future<void> _runAction({
    required String title,
    required String message,
    required String confirmLabel,
    required Color confirmColor,
    required Future<void> Function() action,
    required String successMessage,
    bool leaveOnSuccess = false,
  }) async {
    if (!await _confirm(
      title: title,
      message: message,
      confirmLabel: confirmLabel,
      confirmColor: confirmColor,
    )) {
      return;
    }

    setState(() => _busy = true);
    try {
      await action();
      if (!mounted) return;
      ScaffoldMessenger.of(
        context,
      ).showSnackBar(SnackBar(content: Text(successMessage)));
      if (leaveOnSuccess) {
        widget.onDeleted();
        return;
      }
      setState(() => _busy = false);
      await _fetchDetail();
    } catch (e) {
      if (!mounted) return;
      setState(() => _busy = false);
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('Failed: ${errorText(e, capitalize: false)}')),
      );
    }
  }

  void _kill() => _runAction(
    title: 'Kill session?',
    message:
        'Stops the running program. The worktree is kept and the '
        'conversation resumes on next attach.',
    confirmLabel: 'Kill',
    confirmColor: CommanderTokens.of(context).attention,
    successMessage: 'Session killed',
    action: () => _store!.killSession(_id),
  );

  void _restart() {
    final pending = _info.pendingProgram;
    _runAction(
      title: 'Restart session?',
      message: pending == null
          ? 'Restarts the program in this session.'
          : 'Restarts the session with "$pending". The conversation is '
                'cleared.',
      confirmLabel: 'Restart',
      confirmColor: CommanderTokens.of(context).working,
      successMessage: 'Session restarted',
      action: () => _store!.restartSession(_id),
    );
  }

  void _delete() => _runAction(
    title: 'Delete session?',
    message:
        'Removes the session, its branch, and its worktree. '
        'This cannot be undone.',
    confirmLabel: 'Delete',
    confirmColor: CommanderTokens.of(context).danger,
    successMessage: 'Session deleted',
    leaveOnSuccess: true,
    action: () => _store!.deleteSession(_id),
  );

  /// Run a stack operation (cascade / push-stack) with a confirm dialog, a busy
  /// guard, and a snackbar reporting the returned [OperationStatusDto] outcome
  /// (succeeded / paused / failed, plus its detail).
  Future<void> _runOperation({
    required String title,
    required String message,
    required String confirmLabel,
    required Future<OperationStatusDto> Function() action,
  }) async {
    if (!await _confirm(
      title: title,
      message: message,
      confirmLabel: confirmLabel,
    )) {
      return;
    }
    setState(() => _busy = true);
    try {
      final status = await action();
      if (!mounted) return;
      ScaffoldMessenger.of(
        context,
      ).showSnackBar(SnackBar(content: Text(describeOperation(status))));
      setState(() => _busy = false);
      await _fetchDetail();
    } catch (e) {
      if (!mounted) return;
      setState(() => _busy = false);
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('Failed: ${errorText(e, capitalize: false)}')),
      );
    }
  }

  void _cascadeMerge() => _runOperation(
    title: 'Cascade merge?',
    message:
        'Merges this session and everything stacked below it, in order. '
        'Pauses for a decision if a merge needs attention.',
    confirmLabel: 'Cascade',
    action: () => _store!.cascadeMerge(_id),
  );

  void _pushStack() => _runOperation(
    title: 'Push stack?',
    message: 'Pushes this session and its ancestors as a stack of branches.',
    confirmLabel: 'Push',
    action: () => _store!.pushStack(_id),
  );

  /// Run a mutation (no confirm dialog) with a busy guard + success/failure
  /// snackbar, then re-sync detail. Shared by session edits and lifecycle actions.
  Future<void> _mutate(
    Future<void> Function() action,
    String ok, {
    String? Function()? successMessage,
  }) async {
    setState(() => _busy = true);
    try {
      await action();
      if (!mounted) return;
      ScaffoldMessenger.of(
        context,
      ).showSnackBar(SnackBar(content: Text(successMessage?.call() ?? ok)));
      setState(() => _busy = false);
      await _fetchDetail();
    } catch (e) {
      if (!mounted) return;
      setState(() => _busy = false);
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('Failed: ${errorText(e, capitalize: false)}')),
      );
    }
  }

  Future<void> _edit(SessionInfo info) async {
    final store = _store!;
    final edit = await showDialog<SessionEdit>(
      context: context,
      builder: (_) => EditSessionDialog(
        session: info,
        peers: store.sessions,
        mainBranch: store.projects
            .where((p) => p.id == info.projectId)
            .firstOrNull
            ?.mainBranch,
        options: store.api.createOptions(handle: store.handle!),
      ),
    );
    if (edit == null || !mounted) return;
    String? warning;
    await _mutate(
      () async {
        warning = await store.editSession(_id, patchJson: edit.patchJson);
      },
      'Session updated',
      successMessage: () => warning,
    );
  }

  /// The freshest session info: the store's live copy, then the fetched detail,
  /// then the list's snapshot the body was opened with.
  SessionInfo get _info =>
      _store?.sessionById(_id) ?? _detail?.info ?? widget.session;

  AgentState get _agentState =>
      _store?.agentStateFor(_id) ?? _detail?.agentState ?? AgentState.unknown;

  @override
  Widget build(BuildContext context) {
    return _gone ? _goneView(context) : _liveBody(context, _info);
  }

  Widget _goneView(BuildContext context) {
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(
              Icons.link_off,
              size: 40,
              color: Theme.of(context).colorScheme.outline,
            ),
            const SizedBox(height: 12),
            Text(
              'Session no longer exists',
              style: Theme.of(context).textTheme.titleMedium,
              textAlign: TextAlign.center,
            ),
            const SizedBox(height: 8),
            Text(
              'It was deleted or stopped and removed on the server.',
              style: Theme.of(context).textTheme.bodySmall,
              textAlign: TextAlign.center,
            ),
            const SizedBox(height: 16),
            FilledButton.tonal(
              onPressed: widget.onDismiss,
              child: const Text('Back'),
            ),
          ],
        ),
      ),
    );
  }

  Widget _liveBody(BuildContext context, SessionInfo info) {
    final waiting =
        info.status == SessionStatus.running &&
        _agentState == AgentState.waitingForInput;
    return RefreshIndicator(
      onRefresh: _refresh,
      child: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          _header(context, info),
          if (waiting) ...[const SizedBox(height: 12), _waitingHint(context)],
          const SizedBox(height: 16),
          _agentHero(context),
          const SizedBox(height: 12),
          if (_error != null) _errorBanner(context, _error!),
          _detailSection(context),
          if (widget.showPanePreview) ...[
            const SizedBox(height: 16),
            _paneSection(context, info),
          ],
          const SizedBox(height: 24),
          _lifecycleBar(info),
        ],
      ),
    );
  }

  Widget _header(BuildContext context, SessionInfo info) {
    final t = CommanderTokens.of(context);
    final section = info.sectionOverride ?? info.currentSection;
    return Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              // Mono meta line: project · branch · program.
              Padding(
                padding: const EdgeInsets.only(top: 6),
                child: Text(
                  '${info.projectName} · ${info.branch} · ${info.program}',
                  style: t.meta(size: 12, color: t.textMuted),
                ),
              ),
              const SizedBox(height: 10),
              Wrap(
                spacing: 6,
                runSpacing: 6,
                children: [
                  statusChip(context, info.status),
                  if (info.status == SessionStatus.running)
                    agentStateChip(context, _agentState),
                  if (info.prNumber != null)
                    prChip(context, info.prNumber!, info.prState),
                  if (section != null) sectionChip(context, section),
                  if (info.keepAlive) keepAliveChip(context),
                ],
              ),
            ],
          ),
        ),
        _manageMenu(context, info),
      ],
    );
  }

  /// A slim attention-tinted banner shown only while the agent is blocked on a
  /// prompt, nudging the user into the Agent terminal (the only place the
  /// prompt can be answered). We don't have the prompt text on the client, so
  /// the copy is generic.
  ///
  /// The tint comes from [SessionTone.waiting] rather than being derived here,
  /// so the hint and the row of the session it describes can never drift apart:
  /// one amber-tinted box in Mission Control, a salmon-top-bordered panel in
  /// LCARS.
  Widget _waitingHint(BuildContext context) {
    final t = CommanderTokens.of(context);
    final tone = t.toneStyle(SessionTone.waiting);
    return ChromePanel(
      ChromePanelSpec(
        tone: SessionTone.waiting,
        padding: const EdgeInsets.symmetric(horizontal: 11, vertical: 9),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              '?',
              style: t.meta(
                size: 12,
                weight: FontWeight.w700,
                color: tone.accent,
              ),
            ),
            const SizedBox(width: 8),
            Expanded(
              child: Text(
                'Waiting for input — answer in the Agent terminal.',
                style: t.meta(size: 11.5, color: tone.onTint, height: 1.4),
              ),
            ),
          ],
        ),
      ),
    );
  }

  /// The overflow (⋮) menu of low-frequency management actions: rename, set
  /// section, and the keep-alive toggle (which stops the server hibernating an
  /// idle session). Disabled while a mutation is in flight.
  Widget _manageMenu(BuildContext context, SessionInfo info) {
    return PopupMenuButton<_ManageAction>(
      enabled: !_busy,
      tooltip: 'Manage session',
      icon: const Icon(Icons.more_vert),
      onSelected: (action) {
        switch (action) {
          case _ManageAction.edit:
            _edit(info);
        }
      },
      itemBuilder: (_) => [
        _menuItem(_ManageAction.edit, Icons.edit, 'Edit session'),
      ],
    );
  }

  PopupMenuItem<_ManageAction> _menuItem(
    _ManageAction value,
    IconData icon,
    String label,
  ) {
    return PopupMenuItem(
      value: value,
      child: Row(
        children: [
          Icon(icon, size: 20),
          const SizedBox(width: 12),
          Text(label),
        ],
      ),
    );
  }

  /// The single dominant action: a full-width accent button into the live agent
  /// terminal. Shell / review / lifecycle are all reachable below, so the agent
  /// pane — the primary interaction — gets the hero treatment. The Changes card
  /// below is the diff entry point; Shell lives in the lifecycle bar.
  ///
  /// Deliberately **not** a chrome form. No form element expresses a full-width
  /// prominent call to action: [ChromePanel] would flatten it to a bordered card
  /// and [ChromeButtonBar]'s Mission Control cell is a 38px icon over a caption.
  /// It needs neither — `filledButtonTheme` already resolves shape and colour
  /// from the tokens, so this renders as a violet 12px-radius button in Mission
  /// Control and a hard-cornered amber block in LCARS.
  Widget _agentHero(BuildContext context) {
    return SizedBox(
      width: double.infinity,
      child: FilledButton.icon(
        onPressed: () => widget.onOpenTerminal(AttachKind.agent),
        icon: const Icon(Icons.terminal, size: 18),
        label: const Text('Open Agent terminal'),
        style: FilledButton.styleFrom(
          padding: const EdgeInsets.symmetric(vertical: 15),
        ),
      ),
    );
  }

  /// The diffstat card, which doubles as the review entry point: tapping it
  /// opens the diff (the review view). Stays tappable even with no changes —
  /// the review view is then simply empty, and this keeps it reachable on the
  /// phone layout, which has no review tab.
  Widget _detailSection(BuildContext context) {
    final t = CommanderTokens.of(context);
    final diffStat = _detail?.diffStat;
    // The Semantics wrapper stays outside the panel: the chrome's tappable panel
    // carries a tap handler, but only LCARS marks it as a button, so the flag is
    // asserted here for both.
    return Semantics(
      button: true,
      child: ChromePanel(
        ChromePanelSpec(
          padding: const EdgeInsets.symmetric(horizontal: 13, vertical: 12),
          onTap: widget.onOpenReview,
          child: Row(
            children: [
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Row(
                      children: [
                        Text(
                          'Changes',
                          style: Theme.of(context).textTheme.titleSmall,
                        ),
                        const SizedBox(width: 7),
                        Icon(
                          Icons.rate_review_outlined,
                          size: 14,
                          color: t.textFaint,
                        ),
                        const SizedBox(width: 3),
                        Text(
                          'review',
                          style: t.meta(size: 11, color: t.textFaint),
                        ),
                      ],
                    ),
                    const SizedBox(height: 5),
                    Text(
                      diffStat == null || diffStat.isEmpty
                          ? 'No changes'
                          : diffStat,
                      style: t.meta(color: t.textMuted),
                    ),
                  ],
                ),
              ),
              Icon(Icons.chevron_right, color: t.textFaint),
            ],
          ),
        ),
      ),
    );
  }

  /// The on-demand terminal snapshot. Only the frame is themed: the captured
  /// output itself stays `t.mono` on `t.terminalFg` over `t.terminalBg` in every
  /// theme, because it is real agent output rather than chrome.
  Widget _paneSection(BuildContext context, SessionInfo info) {
    final t = CommanderTokens.of(context);
    final pane = _detail?.paneContent;
    return ChromePanel(
      ChromePanelSpec(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Text(
                  'Terminal snapshot',
                  style: Theme.of(context).textTheme.titleSmall,
                ),
                const Spacer(),
                TextButton.icon(
                  onPressed: () => widget.onOpenTerminal(AttachKind.agent),
                  icon: const Icon(Icons.terminal, size: 16),
                  label: const Text('Live'),
                ),
              ],
            ),
            const SizedBox(height: 8),
            Container(
              width: double.infinity,
              constraints: const BoxConstraints(maxHeight: 320),
              padding: const EdgeInsets.all(10),
              decoration: BoxDecoration(
                color: t.terminalBg,
                borderRadius: BorderRadius.circular(8),
                border: Border.all(color: t.borderSubtle),
              ),
              child: SingleChildScrollView(
                child: SelectableText(
                  (pane == null || pane.isEmpty) ? '(no output)' : pane,
                  style: t.meta(size: 12, color: t.terminalFg, height: 1.5),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }

  /// The session lifecycle controls: the common shell/kill/restart/cascade/push
  /// first, with the destructive delete last. Shell is pure navigation (always
  /// enabled); Kill needs a running session; the rest are gated only by the busy
  /// flag.
  ///
  /// The bar's shape is the chrome's — a labelled icon bar in Mission Control
  /// (which pushes the destructive action to the trailing edge itself, so the
  /// `Spacer` this used to place is no longer ours to position), one contiguous
  /// run of lettered blocks in LCARS.
  Widget _lifecycleBar(SessionInfo info) {
    final t = CommanderTokens.of(context);
    final running = info.status == SessionStatus.running;
    return ChromeButtonBar(
      ChromeButtonBarSpec(
        buttons: [
          ChromeBarButton(
            label: 'Shell',
            icon: Icons.code,
            onPressed: () => widget.onOpenTerminal(AttachKind.shell),
          ),
          ChromeBarButton(
            label: 'Kill',
            icon: Icons.stop,
            // Amber and teal respectively, as before the chrome layer: these two
            // read as distinct from the neutral Shell/Cascade/Push.
            accent: t.attentionOn,
            onPressed: _busy || !running ? null : _kill,
          ),
          ChromeBarButton(
            label: 'Restart',
            icon: Icons.restart_alt,
            accent: t.working,
            onPressed: _busy ? null : _restart,
          ),
          ChromeBarButton(
            label: 'Cascade',
            icon: Icons.merge_type,
            tooltip: 'Cascade merge',
            onPressed: _busy ? null : _cascadeMerge,
          ),
          ChromeBarButton(
            label: 'Push',
            icon: Icons.publish,
            tooltip: 'Push stack',
            onPressed: _busy ? null : _pushStack,
          ),
          ChromeBarButton(
            label: 'Delete',
            icon: Icons.delete_outline,
            kind: ChromeActionKind.destructive,
            onPressed: _busy ? null : _delete,
          ),
        ],
      ),
    );
  }

  Widget _errorBanner(BuildContext context, String error) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 12),
      child: Container(
        width: double.infinity,
        padding: const EdgeInsets.all(10),
        decoration: BoxDecoration(
          color: Theme.of(context).colorScheme.errorContainer,
          borderRadius: BorderRadius.circular(6),
        ),
        child: Row(
          children: [
            Icon(
              Icons.warning_amber,
              size: 18,
              color: Theme.of(context).colorScheme.onErrorContainer,
            ),
            const SizedBox(width: 8),
            Expanded(
              child: Text(
                error,
                maxLines: 3,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(
                  color: Theme.of(context).colorScheme.onErrorContainer,
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// The phone (stacked-navigation) detail screen: a [ChromePage] titled by the
/// session, wrapping a [SessionDetailBody] whose terminal/review actions push
/// routes and whose delete/dismiss pop back to the list.
class SessionDetailPage extends StatelessWidget {
  /// The session to show.
  final SessionInfo session;

  const SessionDetailPage({super.key, required this.session});

  void _openTerminal(
    BuildContext context,
    CommanderStore store,
    AttachKind kind,
  ) {
    Navigator.of(context).push(
      MaterialPageRoute(
        // Re-provide the owning scope: route builders don't inherit the pushing
        // widget's context, and TerminalPage registers its active attach with
        // the store (so reconnect/dispose can detach before releasing the
        // handle) via CommanderStoreScope.of.
        builder: (_) => CommanderStoreScope(
          store: store,
          child: TerminalPage(
            api: store.api,
            handle: store.handle!,
            session: session,
            kind: kind,
          ),
        ),
      ),
    );
  }

  void _openReview(BuildContext context, CommanderStore store) {
    Navigator.of(context).push(
      MaterialPageRoute(
        builder: (_) => CommanderStoreScope(
          store: store,
          child: ReviewPage(
            api: store.api,
            handle: store.handle!,
            session: session,
          ),
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final store = CommanderStoreScope.of(context)!;
    return ChromePage(
      code: '47-D',
      title: session.title,
      body: SessionDetailBody(
        session: session,
        onOpenTerminal: (kind) => _openTerminal(context, store, kind),
        onOpenReview: () => _openReview(context, store),
        onDeleted: () => Navigator.of(context).pop(true),
        onDismiss: () => Navigator.of(context).maybePop(),
        // Phone form factor: drop the static snapshot in favour of the live
        // terminal, which is a single tap away.
        showPanePreview: false,
      ),
    );
  }
}

/// A one-line human summary of a completed stack operation, for a snackbar:
/// e.g. "Cascade merge succeeded", or "Push stack failed" with its detail
/// appended. Shared by the detail actions and the paused-cascade banner.
String describeOperation(OperationStatusDto status) {
  final what = switch (status.kind) {
    OperationKind.cascade => 'Cascade merge',
    OperationKind.pushStack => 'Push stack',
  };
  final verb = switch (status.outcome.kind) {
    OperationOutcomeKind.succeeded => 'succeeded',
    OperationOutcomeKind.paused => 'paused',
    OperationOutcomeKind.failed => 'failed',
  };
  final detail = status.outcome.detail.trim();
  return detail.isEmpty ? '$what $verb' : '$what $verb: $detail';
}
