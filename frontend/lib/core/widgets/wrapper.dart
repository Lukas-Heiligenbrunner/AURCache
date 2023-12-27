import 'package:flutter/material.dart';

import '../constants/color_constants.dart';

class Wrapper extends StatelessWidget {
  final Widget? title;
  final Widget child;

  const Wrapper({Key? key, this.title, required this.child}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.all(defaultPadding),
      decoration: BoxDecoration(
        color: Palette.wrapperBg,
        borderRadius: BorderRadius.circular(defaultBorderRadius),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          if (title != null)
            Column(
              children: [
                title!,
                const SizedBox(height: defaultPadding),
              ],
            ),
          child
        ],
      ),
    );
  }
}
