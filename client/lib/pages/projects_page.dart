import 'package:flutter/material.dart';

import '../chrome/chrome.dart';
import '../src/rust/api/mirrors.dart';
import '../state/commander_store.dart';
import '../theme/tokens.dart';
import '../util/error_text.dart';
import 'clone_repo_page.dart';

/// Manages the server's registered projects (git repos). Lists each project's
/// name + repo path; adds one by its server-side path (`addProject`), removes one
/// (`removeProject`), scans a server-side directory for repos (`scanDirectory`),
/// clones one from the selected code host ([CloneRepoPage]), and browses a project's branches on
/// demand (`listBranches`).
///
/// Project paths are typed, not picked — the paths live on the server, not the
/// device, mirroring the TUI. The one exception is cloning, which is how a
/// project arrives on a server that doesn't have it yet: the repo list and the
/// checkout are both the server's, so a phone can add a project it has no copy
/// of. The list is rendered reactively from the [CommanderStore] (the change feed
/// refreshes it after a mutation).
///
/// A project added, scanned or cloned here lands in [workspace] — the app's
/// active one — so it appears in the list the user came from. Every project is
/// listed regardless of workspace, each tagged with the one it is in; moving
/// them between workspaces is the Workspaces page's job.
class ProjectsPage extends StatefulWidget {
  final CommanderStore store;

  /// Where new projects go (null = Main).
  final String? workspace;

  const ProjectsPage({super.key, required this.store, this.workspace});

  @override
  State<ProjectsPage> createState() => _ProjectsPageState();
}

/// The three ways to add a project, as offered by the **+** sheet.
enum _AddSource { clone, path, scan }

class _ProjectsPageState extends State<ProjectsPage> {
  bool _busy = false;

  CommanderStore get _store => widget.store;

  /// Prompt for a server-side path; returns the trimmed value or null on cancel.
  Future<String?> _promptPath({required String title, required String label}) =>
      showDialog<String>(
        context: context,
        builder: (_) => _PathPromptDialog(title: title, label: label),
      );

  /// The **+** sheet: the three ways a project can arrive. Cloning is listed
  /// first because it is the only one that doesn't require the repo to already
  /// be on the server's disk — the case a phone can't otherwise satisfy.
  Future<void> _addSheet() async {
    final t = CommanderTokens.of(context);
    final choice = await showModalBottomSheet<_AddSource>(
      context: context,
      backgroundColor: t.canvasRaised,
      builder: (sheetContext) => SafeArea(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            _sheetTile(
              sheetContext,
              Icons.cloud_download_outlined,
              'Clone repository',
              'Pick a repo, or paste a clone URL',
              _AddSource.clone,
            ),
            _sheetTile(
              sheetContext,
              Icons.folder_open,
              'Add existing path',
              'A repo already on the server',
              _AddSource.path,
            ),
            _sheetTile(
              sheetContext,
              Icons.travel_explore,
              'Scan directory',
              'Register every repo it finds',
              _AddSource.scan,
            ),
          ],
        ),
      ),
    );
    if (choice == null || !mounted) return;
    switch (choice) {
      case _AddSource.clone:
        await _clone();
      case _AddSource.path:
        await _addProject();
      case _AddSource.scan:
        await _scan();
    }
  }

  Widget _sheetTile(
    BuildContext sheetContext,
    IconData icon,
    String label,
    String subtitle,
    _AddSource value,
  ) {
    final t = CommanderTokens.of(context);
    return ListTile(
      leading: Icon(icon, color: t.primary),
      title: Text(label),
      subtitle: Text(subtitle, style: t.meta(size: 11, color: t.textMuted)),
      onTap: () => Navigator.of(sheetContext).pop(value),
    );
  }

  /// Push the repo picker. It refreshes the store itself before popping, so the
  /// list here already holds a newly cloned project when we come back.
  Future<void> _clone() async {
    await Navigator.of(context).push(
      MaterialPageRoute<bool>(
        builder: (_) =>
            CloneRepoPage(store: _store, workspace: widget.workspace),
      ),
    );
  }

  Future<void> _addProject() async {
    final path = await _promptPath(
      title: 'Add project',
      label: 'Server-side repo path',
    );
    if (path == null || path.isEmpty) return;
    await _run(() async {
      await _store.addProject(path, workspace: widget.workspace);
      await _store.refresh();
      _snack('Project added');
    });
  }

  Future<void> _scan() async {
    final path = await _promptPath(
      title: 'Scan directory',
      label: 'Server-side directory path',
    );
    if (path == null || path.isEmpty) return;
    await _run(() async {
      final result = await _store.scanDirectory(
        path,
        workspace: widget.workspace,
      );
      await _store.refresh();
      _snack('Added ${result.added}, skipped ${result.skipped}');
    });
  }

  Future<void> _remove(ProjectInfoDto project) async {
    final t = CommanderTokens.of(context);
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Remove project?'),
        content: Text(
          'Deregisters "${project.name}" from the server. '
          'The repo on disk is not touched.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            style: FilledButton.styleFrom(
              backgroundColor: t.danger,
              foregroundColor: t.canvas,
            ),
            onPressed: () => Navigator.of(ctx).pop(true),
            child: const Text('Remove'),
          ),
        ],
      ),
    );
    if (confirmed != true) return;
    await _run(() async {
      await _store.removeProject(project.id.field0.uuid);
      _snack('Project removed');
    });
  }

  /// Run a mutation with a busy guard and a failure snackbar.
  Future<void> _run(Future<void> Function() action) async {
    if (_busy) return;
    setState(() => _busy = true);
    try {
      await action();
    } catch (e) {
      _snack('Failed: ${errorText(e, capitalize: false)}');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  void _snack(String message) {
    if (!mounted) return;
    ScaffoldMessenger.of(
      context,
    ).showSnackBar(SnackBar(content: Text(message)));
  }

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: _store,
      builder: (context, _) {
        final projects = _store.projects;
        return ChromePage(
          title: 'Projects',
          code: '47-P',
          primaryAction: ChromeButtonAction(
            icon: Icons.add,
            label: 'Add project',
            onPressed: _busy ? null : _addSheet,
          ),
          body: projects.isEmpty
              ? _emptyState()
              : ListView(
                  padding: const EdgeInsets.only(bottom: 88),
                  children: [
                    for (final p in projects)
                      _ProjectTile(
                        key: ValueKey(p.id.field0.uuid),
                        project: p,
                        store: _store,
                        onRemove: _busy ? null : () => _remove(p),
                      ),
                  ],
                ),
        );
      },
    );
  }

  Widget _emptyState() {
    return ListView(
      children: const [
        SizedBox(height: 120),
        Center(child: Icon(Icons.folder_off_outlined, size: 48)),
        SizedBox(height: 12),
        Center(child: Text('No projects — tap + to add one')),
      ],
    );
  }
}

/// One project row: name + repo path, a remove action, and a lazily-loaded
/// branch list revealed on expand (`listBranches`, local branches only).
class _ProjectTile extends StatefulWidget {
  final ProjectInfoDto project;
  final CommanderStore store;
  final VoidCallback? onRemove;

  const _ProjectTile({
    super.key,
    required this.project,
    required this.store,
    required this.onRemove,
  });

  @override
  State<_ProjectTile> createState() => _ProjectTileState();
}

class _ProjectTileState extends State<_ProjectTile> {
  List<BranchInfo>? _branches;
  Object? _error;
  bool _loading = false;

  Future<void> _loadBranches() async {
    if (_loading || _branches != null) return;
    setState(() => _loading = true);
    try {
      final branches = await widget.store.listBranches(
        widget.project.id.field0.uuid,
      );
      if (!mounted) return;
      setState(() {
        _branches = branches;
        _error = null;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() => _error = e);
    } finally {
      if (mounted) setState(() => _loading = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final t = CommanderTokens.of(context);
    return Card(
      margin: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
      child: ExpansionTile(
        title: Text(
          widget.project.name,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
        ),
        subtitle: Text(
          [widget.project.repoPath, ?widget.project.workspace].join(' · '),
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: t.meta(size: 11, color: t.textMuted),
        ),
        trailing: IconButton(
          onPressed: widget.onRemove,
          icon: const Icon(Icons.delete_outline),
          tooltip: 'Remove',
        ),
        onExpansionChanged: (expanded) {
          if (expanded) _loadBranches();
        },
        children: [_branchesView(context)],
      ),
    );
  }

  Widget _branchesView(BuildContext context) {
    if (_loading) {
      return const Padding(
        padding: EdgeInsets.all(16),
        child: Center(child: CircularProgressIndicator()),
      );
    }
    if (_error != null) {
      return Padding(
        padding: const EdgeInsets.all(16),
        child: Text(
          'Failed to load branches: '
          '${errorText(_error!, capitalize: false)}',
          maxLines: 3,
          overflow: TextOverflow.ellipsis,
        ),
      );
    }
    final branches = _branches;
    if (branches == null) return const SizedBox.shrink();
    if (branches.isEmpty) {
      return const Padding(
        padding: EdgeInsets.all(16),
        child: Text('No branches'),
      );
    }
    final t = CommanderTokens.of(context);
    return Column(
      children: [
        for (final b in branches)
          ListTile(
            dense: true,
            leading: Icon(
              b.isRemote ? Icons.cloud_outlined : Icons.call_split,
              size: 18,
              color: b.isRemote ? t.textFaint : t.working,
            ),
            title: Text(b.name, style: t.meta(size: 12, color: t.textBright)),
          ),
      ],
    );
  }
}

/// A path-prompt dialog that owns its controller and disposes it with its route.
/// Pops with the trimmed text on confirm, or null on cancel.
class _PathPromptDialog extends StatefulWidget {
  final String title;
  final String label;

  const _PathPromptDialog({required this.title, required this.label});

  @override
  State<_PathPromptDialog> createState() => _PathPromptDialogState();
}

class _PathPromptDialogState extends State<_PathPromptDialog> {
  final _controller = TextEditingController();

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  void _submit() => Navigator.of(context).pop(_controller.text.trim());

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: Text(widget.title),
      content: TextField(
        controller: _controller,
        autofocus: true,
        decoration: InputDecoration(labelText: widget.label),
        onSubmitted: (_) => _submit(),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(onPressed: _submit, child: const Text('OK')),
      ],
    );
  }
}
