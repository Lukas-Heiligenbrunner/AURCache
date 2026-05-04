import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';

class BuildFlagSettings extends StatefulWidget {
  const BuildFlagSettings({
    super.key,
    required this.initialFlags,
    required this.onChanged,
  });

  final List<String> initialFlags;

  /// Called with the new full list whenever the user adds or removes a flag.
  /// Should persist + show a toast and return `true` on success — when it
  /// returns `false` the local list is rolled back to the previous state.
  final Future<bool> Function(List<String> flags) onChanged;

  @override
  State<BuildFlagSettings> createState() => _BuildFlagSettingsState();
}

class _BuildFlagSettingsState extends State<BuildFlagSettings> {
  List<String> _flags = [];
  final TextEditingController _controller = TextEditingController();

  @override
  void initState() {
    super.initState();
    _flags = widget.initialFlags.toList(growable: true);
  }

  @override
  void didUpdateWidget(covariant BuildFlagSettings oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (!_listEquals(oldWidget.initialFlags, widget.initialFlags)) {
      setState(() => _flags = widget.initialFlags.toList(growable: true));
    }
  }

  bool _listEquals(List<String> a, List<String> b) {
    if (a.length != b.length) return false;
    for (var i = 0; i < a.length; i++) {
      if (a[i] != b[i]) return false;
    }
    return true;
  }

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  Future<void> _commit(List<String> next) async {
    final previous = _flags;
    setState(() => _flags = next);
    final ok = await widget.onChanged(next);
    if (!ok && mounted) {
      setState(() => _flags = previous);
    }
  }

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        if (_flags.isEmpty)
          const Padding(
            padding: EdgeInsets.symmetric(vertical: 4),
            child: Text(
              "No build flags set.",
              style: TextStyle(color: Colors.grey, fontStyle: FontStyle.italic),
            ),
          )
        else
          Tags(
            itemBuilder: (idx) => ItemTags(
              index: idx,
              title: _flags[idx],
              active: true,
              activeColor: Colors.white38,
              pressEnabled: false,
              removeButton: ItemTagsRemoveButton(
                onRemoved: () {
                  final next = List<String>.from(_flags)..removeAt(idx);
                  unawaited(_commit(next));
                  return true;
                },
              ),
            ),
            itemCount: _flags.length,
          ),
        const SizedBox(height: 12),
        ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 320),
          child: TextField(
            controller: _controller,
            decoration: InputDecoration(
              labelText: "Add build flag",
              hintText: "--noconfirm",
              suffixIcon: IconButton(
                tooltip: 'Add',
                onPressed: _addFromController,
                icon: const Icon(Icons.add),
              ),
            ),
            onSubmitted: (_) => _addFromController(),
          ),
        ),
      ],
    );
  }

  void _addFromController() {
    final text = _controller.text.trim();
    if (text.isEmpty || _flags.contains(text)) return;
    final next = [..._flags, text];
    _controller.clear();
    unawaited(_commit(next));
  }
}
