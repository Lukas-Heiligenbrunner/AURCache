import 'package:flutter/material.dart';

class ZipWizard extends StatelessWidget {
  const ZipWizard({super.key});

  @override
  Widget build(BuildContext context) {
    return Center(
        child: Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(Icons.construction, size: 80, color: Colors.orangeAccent),
        SizedBox(height: 20),
        Text("This feature is under construction",
            style: TextStyle(fontSize: 24)),
      ],
    ));
  }
}
