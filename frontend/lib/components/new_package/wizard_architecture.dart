import 'package:flutter/material.dart';

import '../add_package_popup.dart';

class ArchitectureWizard extends StatelessWidget {
  ArchitectureWizard({super.key});
  final List<String> selectedArchs = ["x86_64"];

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        const Text(
          "Select the target architectures you want this package build for:",
        ),
        const SizedBox(height: 10),
        ArchTags(selectedArchs: selectedArchs),
        const SizedBox(height: 15),
        const Text(
          "Remember: Supported platforms depend strongly on the AUR package and its PKGBUILD.",
        ),
      ],
    );
  }
}
