import 'package:flutter/material.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';

final archs = [
  "x86_64",
  "aarch64",
  "riscv64",
  "riscv32",
  "arm",
  "alpha",
  "armeb",
  "sparc",
  "sparc32plus",
  "sparc64",
  "ppc",
  "ppc64",
  "ppc64le",
  "m68k",
  "mips",
  "mipsel",
  "mipsn32",
  "mipsn32el",
  "mips64",
  "mips64el",
  "sh4",
  "sh4eb",
  "s390x",
  "aarch64_be",
  "hppa",
  "xtensa",
  "xtensaeb",
  "microblaze",
  "microblazeel",
  "or1k",
  "hexagon"
];

Future<bool> showPackageAddPopup(
  BuildContext context,
  String packageName,
  void Function() successCallback,
  void Function()? errorCallback,
) async {
  return (await showDialog<bool>(
    context: context,
    barrierDismissible: false,
    builder: (BuildContext context) {
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
                  Tags(
                    itemBuilder: (index) => ItemTags(
                      index: index,
                      title: archs[index],
                      active: index == 0,
                      activeColor: Colors.green,
                    ),
                    itemCount: archs.length,
                  ),
                  const SizedBox(
                    height: 15,
                  ),
                  const Text(
                      "Remember: Supported platforms depend strongly on the AUR package and ints PKGBUILD."),
                ],
              ),
            ),
            actions: <Widget>[
              TextButton(
                onPressed: () {
                  Navigator.of(context).pop(true);
                  successCallback();
                },
                child: const Text('Install'),
              ),
              TextButton(
                onPressed: () {
                  Navigator.of(context).pop(false); // Dismiss dialog
                  if (errorCallback != null) {
                    errorCallback();
                  }
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
