import 'dart:convert';
import 'package:flutter/material.dart';
import '../src/rust/api/mirrors.dart';
import '../services/session_editor.dart';
export '../services/session_editor.dart' show SessionEdit;

/// Renders the shared Rust editor model and submits its changed-field plan.
class EditSessionDialog extends StatefulWidget {
  final SessionInfo session;
  final List<SessionInfo> peers;
  final String? mainBranch;
  final Future<CreateOptions> options;
  final SessionEditorBridge? bridge;
  const EditSessionDialog({
    super.key,
    required this.session,
    required this.peers,
    this.mainBranch,
    required this.options,
    this.bridge,
  });
  @override
  State<EditSessionDialog> createState() => _EditSessionDialogState();
}

class _EditSessionDialogState extends State<EditSessionDialog> {
  final _form = GlobalKey<FormState>();
  late Map<String, dynamic> _model;
  late final TextEditingController _name;
  late String _program;
  late String? _section;
  late String _base;
  late bool _keepAlive;
  bool _confirming = false;
  String? _error;

  SessionEditorBridge get _bridge =>
      widget.bridge ??
      debugSessionEditorBridge ??
      const NativeSessionEditorBridge();

  Map<String, dynamic> _readModel(CreateOptions? options) =>
      jsonDecode(
            _bridge.model(
              widget.session,
              widget.peers,
              widget.mainBranch,
              options,
            ),
          )
          as Map<String, dynamic>;
  @override
  void initState() {
    super.initState();
    _model = _readModel(null);
    final original = _model['original'] as Map<String, dynamic>;
    _name = TextEditingController(text: original['title'] as String);
    _program = original['program'] as String;
    _section = original['section'] as String?;
    _base = original['base'] as String;
    _keepAlive = original['keep_alive'] as bool;
    widget.options
        .then((options) {
          if (mounted) setState(() => _model = _readModel(options));
        })
        .catchError((Object error) {
          if (mounted) {
            setState(
              () => _error =
                  'Could not load program and section choices. Existing settings can still be saved.',
            );
          }
        });
  }

  @override
  void dispose() {
    _name.dispose();
    super.dispose();
  }

  Future<void> _save() async {
    if (_confirming || !_form.currentState!.validate()) return;
    final Map<String, dynamic> plan;
    try {
      plan =
          jsonDecode(
                _bridge.plan(
                  jsonEncode(_model),
                  jsonEncode({
                    'title': _name.text,
                    'program': _program,
                    'section': _section,
                    'base': _base,
                    'keep_alive': _keepAlive,
                  }),
                ),
              )
              as Map<String, dynamic>;
    } catch (error) {
      setState(() => _error = error.toString());
      return;
    }
    var restart = false;
    if (plan['requires_restart'] as bool) {
      setState(() => _confirming = true);
      final choice = await showDialog<bool>(
        context: context,
        builder: (context) => AlertDialog(
          title: const Text('Restart session?'),
          content: const Text(
            'Changing the program requires a fresh restart. Restarting stops the current agent and clears its conversation.\n\nSave for later to keep the current agent running. The new program starts fresh on the next restart.',
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context),
              child: const Text('Back'),
            ),
            TextButton(
              onPressed: () => Navigator.pop(context, false),
              child: const Text('No, save for next restart'),
            ),
            FilledButton(
              onPressed: () => Navigator.pop(context, true),
              child: const Text('Yes, restart and clear'),
            ),
          ],
        ),
      );
      if (!mounted) return;
      setState(() => _confirming = false);
      if (choice == null) return;
      restart = choice;
    }
    if (!mounted) return;
    final patch = plan['edit'] as Map<String, dynamic>;
    patch['restart'] = restart;
    Navigator.pop(context, SessionEdit(jsonEncode(patch)));
  }

  @override
  Widget build(BuildContext context) => AlertDialog(
    title: const Text('Edit session'),
    content: SizedBox(
      width: 460,
      child: Form(
        key: _form,
        child: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              TextFormField(
                controller: _name,
                autofocus: true,
                decoration: const InputDecoration(labelText: 'Session name'),
                validator: (v) =>
                    v == null || v.trim().isEmpty ? 'Required' : null,
              ),
              DropdownButtonFormField<String>(
                isExpanded: true,
                initialValue: _program,
                decoration: const InputDecoration(labelText: 'Program'),
                items: [
                  for (final p in _model['programs'] as List)
                    DropdownMenuItem(
                      value: p['command'] as String,
                      child: Text(
                        p['label'] as String,
                        overflow: TextOverflow.ellipsis,
                      ),
                    ),
                ],
                onChanged: (value) {
                  if (value != null) setState(() => _program = value);
                },
              ),
              DropdownButtonFormField<String>(
                isExpanded: true,
                initialValue: _section ?? '',
                decoration: const InputDecoration(labelText: 'Section'),
                items: [
                  const DropdownMenuItem(value: '', child: Text('Automatic')),
                  for (final s in _model['sections'] as List)
                    DropdownMenuItem(
                      value: s as String,
                      child: Text(s, overflow: TextOverflow.ellipsis),
                    ),
                ],
                onChanged: (value) =>
                    setState(() => _section = value == '' ? null : value),
              ),
              DropdownButtonFormField<String>(
                isExpanded: true,
                initialValue: _base,
                decoration: const InputDecoration(labelText: 'Stack base'),
                items: [
                  for (final b in _model['bases'] as List)
                    DropdownMenuItem(
                      value: b['id'] as String,
                      child: Text(
                        b['label'] as String,
                        overflow: TextOverflow.ellipsis,
                      ),
                    ),
                ],
                onChanged: (value) {
                  if (value != null) setState(() => _base = value);
                },
              ),
              const Padding(
                padding: EdgeInsets.only(top: 8),
                child: Text(
                  'Stack base changes metadata and the PR target. Git history is unchanged.',
                ),
              ),
              SwitchListTile(
                contentPadding: EdgeInsets.zero,
                title: const Text('Keep alive'),
                subtitle: const Text('Prevent automatic hibernation'),
                value: _keepAlive,
                onChanged: (value) => setState(() => _keepAlive = value),
              ),
              if (_error != null) Text(_error!),
            ],
          ),
        ),
      ),
    ),
    actions: [
      TextButton(
        onPressed: _confirming ? null : () => Navigator.pop(context),
        child: const Text('Cancel'),
      ),
      FilledButton(
        onPressed: _confirming ? null : _save,
        child: const Text('Save'),
      ),
    ],
  );
}
