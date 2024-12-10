import 'package:aurcache/components/build_line_chart.dart';
import 'package:aurcache/components/activity_log.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';

import '../../constants/color_constants.dart';

class SidePanel extends StatelessWidget {
  const SidePanel({
    Key? key,
    required this.nrSuccessfulBuilds,
    required this.nrfailedbuilds,
    required this.nrEnqueuedBuilds,
  }) : super(key: key);

  final int nrSuccessfulBuilds;
  final int nrfailedbuilds;
  final int nrEnqueuedBuilds;

  @override
  Widget build(BuildContext context) {
    final activityWidget = Container(
      padding: const EdgeInsets.all(defaultPadding),
      decoration: const BoxDecoration(
        color: secondaryColor,
        borderRadius: BorderRadius.all(Radius.circular(10)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const Text(
            "Recent Activity",
            style: TextStyle(
              fontSize: 18,
              fontWeight: FontWeight.w500,
            ),
          ),
          const SizedBox(height: defaultPadding),
          ActivityLog()
        ],
      ),
    );

    return Column(
      mainAxisAlignment: MainAxisAlignment.spaceBetween,
      children: [
        Container(
          padding: const EdgeInsets.all(defaultPadding),
          decoration: const BoxDecoration(
            color: secondaryColor,
            borderRadius: BorderRadius.all(Radius.circular(10)),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              const Text(
                "Builds Per Month",
                style: TextStyle(
                  fontSize: 18,
                  fontWeight: FontWeight.w500,
                ),
              ),
              const SizedBox(height: defaultPadding),
              BuildLineChart()
            ],
          ),
        ),
        SizedBox(
          height: defaultPadding,
        ),
        Responsive(
            mobileChild: activityWidget,
            desktopChild: Expanded(child: activityWidget))
      ],
    );
  }
}
