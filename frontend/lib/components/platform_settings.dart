import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';

import '../constants/platforms.dart';

class PlatformSettings extends StatefulWidget {
  const PlatformSettings({
    super.key,
    required this.initialPlatforms,
    required this.onChanged,
  });

  final List<String> initialPlatforms;

  /// Called whenever the user toggles a platform. Returns `true` on success;
  /// `false` rolls the local toggle back.
  final Future<bool> Function(List<String> platforms) onChanged;

  @override
  State<PlatformSettings> createState() => _PlatformSettingsState();
}

class _PlatformSettingsState extends State<PlatformSettings> {
  List<String> _selected = [];

  @override
  void initState() {
    super.initState();
    _selected = widget.initialPlatforms.toList(growable: true);
  }

  @override
  void didUpdateWidget(covariant PlatformSettings oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (!_listEquals(oldWidget.initialPlatforms, widget.initialPlatforms)) {
      setState(
        () => _selected = widget.initialPlatforms.toList(growable: true),
      );
    }
  }

  bool _listEquals(List<String> a, List<String> b) {
    if (a.length != b.length) return false;
    for (var i = 0; i < a.length; i++) {
      if (a[i] != b[i]) return false;
    }
    return true;
  }

  Future<void> _commit(List<String> next) async {
    final previous = _selected;
    setState(() => _selected = next);
    final ok = await widget.onChanged(next);
    if (!ok && mounted) {
      setState(() => _selected = previous);
    }
  }

  @override
  Widget build(BuildContext context) {
    return Tags(
      itemBuilder: (idx) {
        final platform = Platforms[idx];
        return ItemTags(
          index: idx,
          title: platform,
          active: _selected.contains(platform),
          activeColor: Colors.green,
          onPressed: (i) {
            final next = List<String>.from(_selected);
            if (i.active!) {
              if (!next.contains(platform)) next.add(platform);
            } else {
              next.remove(platform);
            }
            unawaited(_commit(next));
          },
        );
      },
      itemCount: Platforms.length,
    );
  }
}
