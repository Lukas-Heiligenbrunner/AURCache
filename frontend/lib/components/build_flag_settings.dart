import 'package:aurcache/models/extended_package.dart';
import 'package:flutter/material.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';

class BuildFlagSettings extends StatefulWidget {
  const BuildFlagSettings(
      {super.key, required this.pkg, required this.changed});
  final ExtendedPackage pkg;
  final void Function(List<String>) changed;

  @override
  State<BuildFlagSettings> createState() => _BuildFlagSettingsState();
}

class _BuildFlagSettingsState extends State<BuildFlagSettings> {
  List<String> buildFlags = [];
  final TextEditingController _controller = TextEditingController();

  @override
  void initState() {
    super.initState();
    buildFlags = widget.pkg.selected_build_flags.toList(growable: true);
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
          "Build flags:",
          style: TextStyle(fontSize: 18),
          textAlign: TextAlign.start,
        ),
        const SizedBox(
          height: 20,
        ),
        Tags(
          itemBuilder: (idx) => ItemTags(
            index: idx,
            title: buildFlags[idx],
            active: true,
            activeColor: Colors.white38,
            pressEnabled: false,
            removeButton: ItemTagsRemoveButton(
              onRemoved: () {
                setState(() {
                  buildFlags.remove(buildFlags[idx]);
                });
                widget.changed(buildFlags);
                return true;
              },
            ),
          ),
          itemCount: buildFlags.length,
        ),
        const SizedBox(
          height: 15,
        ),
        const Text("Add new build flags:"),
        SizedBox(
          width: 200,
          child: TextField(
            controller: _controller,
            decoration: InputDecoration(
                label: const Text("--noconfirm"),
                suffixIcon: IconButton(
                    onPressed: () {
                      setState(() {
                        if (_controller.text.isNotEmpty &&
                            !buildFlags.contains(_controller.text)) {
                          buildFlags.add(_controller.text);
                          widget.changed(buildFlags);
                        }
                      });
                    },
                    icon: const Icon(Icons.add))),
          ),
        )
      ],
    );
  }
}
