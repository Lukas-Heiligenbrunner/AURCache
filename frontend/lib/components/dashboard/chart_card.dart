import 'package:flutter/material.dart';

import '../../constants/color_constants.dart';

class ChartCard extends StatelessWidget {
  const ChartCard({
    Key? key,
    required this.title,
    required this.color,
    required this.textRight,
    required this.subtitle,
  }) : super(key: key);

  final Color color;
  final String title, textRight, subtitle;

  @override
  Widget build(BuildContext context) {
    return Container(
      margin: const EdgeInsets.only(top: defaultPadding),
      padding: const EdgeInsets.all(defaultPadding),
      decoration: BoxDecoration(
        border: Border.all(width: 2, color: primaryColor.withOpacity(0.15)),
        borderRadius: const BorderRadius.all(
          Radius.circular(defaultPadding),
        ),
      ),
      child: Row(
        children: [
          SizedBox(
              height: 20,
              width: 20,
              child: Container(
                color: color,
              )),
          Expanded(
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: defaultPadding),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    title,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                  ),
                  Text(
                    subtitle,
                    style: Theme.of(context)
                        .textTheme
                        .bodySmall!
                        .copyWith(color: Colors.white70),
                  ),
                ],
              ),
            ),
          ),
          Text(textRight)
        ],
      ),
    );
  }
}
