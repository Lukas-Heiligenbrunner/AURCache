import 'package:aurcache/constants/platforms.dart';
import 'package:flutter/material.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';

class ArchTags extends StatelessWidget {
  const ArchTags({super.key, required this.selectedArchs});

  final List<String> selectedArchs;

  @override
  Widget build(BuildContext context) {
    return Tags(
      itemBuilder: (idx) => ItemTags(
        index: idx,
        title: Platforms[idx],
        active: selectedArchs.contains(Platforms[idx]),
        activeColor: Colors.green,
        onPressed: (i) {
          if (i.active!) {
            selectedArchs.add(i.title!);
          } else {
            selectedArchs.remove(i.title!);
          }
        },
      ),
      itemCount: Platforms.length,
    );
  }
}
