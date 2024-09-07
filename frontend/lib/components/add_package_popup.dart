import 'package:aurcache/constants/platforms.dart';
import 'package:flutter/material.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';

Future<bool> showPackageAddPopup(
  BuildContext context,
  String packageName,
  void Function(List<String>) successCallback,
) async {
  return (await showDialog<bool>(
    context: context,
    barrierDismissible: false,
    builder: (BuildContext context) {
      List<String> selectedArchs = ["x86_64"];

      return Stack(
        children: <Widget>[
          GestureDetector(
            onTap: () {
              Navigator.of(context).pop(false); // Dismiss dialog on outside tap
            },
            child: Container(
              color: Colors.black.withOpacity(0.5), // Adjust opacity for blur
            ),
          ),
          // Delete confirmation dialog
          AlertDialog(
            title: Text("Install package $packageName?"),
            content: SizedBox(
              height: 200,
              width: 800,
              child: Column(
                children: [
                  const Text(
                      "Select the target architectures you want this package build for:"),
                  const SizedBox(
                    height: 10,
                  ),
                  ArchTags(selectedArchs: selectedArchs),
                  const SizedBox(
                    height: 15,
                  ),
                  const Text(
                      "Remember: Supported platforms depend strongly on the AUR package and its PKGBUILD."),
                ],
              ),
            ),
            actions: <Widget>[
              TextButton(
                onPressed: () {
                  Navigator.of(context).pop(true);
                  successCallback(selectedArchs);
                },
                child: const Text('Install'),
              ),
              TextButton(
                onPressed: () {
                  Navigator.of(context).pop(false); // Dismiss dialog
                },
                child: const Text('Cancel'),
              ),
            ],
          ),
        ],
      );
    },
  ))!;
}

class ArchTags extends StatelessWidget {
  ArchTags({super.key, required this.selectedArchs});

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
