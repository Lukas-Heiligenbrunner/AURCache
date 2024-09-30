import 'package:aurcache/models/extended_package.dart';
import 'package:flutter/material.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';

import '../constants/platforms.dart';

class PlatformSettings extends StatefulWidget {
  const PlatformSettings({super.key, required this.pkg, required this.changed});
  final ExtendedPackage pkg;
  final void Function(List<String>) changed;

  @override
  State<PlatformSettings> createState() => _PlatformSettingsState();
}

class _PlatformSettingsState extends State<PlatformSettings> {
  List<String> platforms = [];

  @override
  void initState() {
    super.initState();
    platforms = widget.pkg.selected_platforms.toList(growable: true);
  }

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const SizedBox(
          height: 5,
        ),
        const Text(
          "Selected build platforms:",
          style: TextStyle(fontSize: 18),
          textAlign: TextAlign.start,
        ),
        const SizedBox(
          height: 20,
        ),
        Tags(
          itemBuilder: (idx) => ItemTags(
            index: idx,
            title: Platforms[idx],
            active: platforms.contains(Platforms[idx]),
            activeColor: Colors.green,
            onPressed: (i) {
              if (i.active!) {
                setState(() {
                  platforms.add(i.title!);
                });
              } else {
                setState(() {
                  platforms.remove(i.title!);
                });
              }
              widget.changed(platforms);
            },
          ),
          itemCount: Platforms.length,
        ),
        const SizedBox(
          height: 15,
        ),
      ],
    );
  }
}
