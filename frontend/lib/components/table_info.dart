import 'package:flutter/material.dart';

class TableInfo extends StatelessWidget {
  const TableInfo({super.key, required this.title});
  final String title;

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.center,
      children: [
        const SizedBox(
          height: 5,
        ),
        const Divider(),
        const SizedBox(
          height: 15,
        ),
        const Icon(
          Icons.info_outline_rounded,
          size: 42,
        ),
        const SizedBox(
          height: 15,
        ),
        Text(title),
      ],
    );
  }
}
