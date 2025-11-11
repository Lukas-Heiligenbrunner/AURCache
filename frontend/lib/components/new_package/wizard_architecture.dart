import 'package:flutter/material.dart';
import '../arch_tags.dart';

class ArchitectureWizard extends StatelessWidget {
  ArchitectureWizard({super.key, required this.selectedArchs});
  final List<String> selectedArchs;

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
