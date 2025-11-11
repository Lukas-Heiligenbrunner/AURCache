import 'package:aurcache/components/activity_log.dart';
import 'package:aurcache/components/build_line_chart.dart';
import 'package:aurcache/components/dashboard/tile_container.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';

import '../../constants/color_constants.dart';

class SidePanel extends StatelessWidget {
  const SidePanel({super.key});

  @override
  Widget build(BuildContext context) {
    final activityWidget = Tilecontainer(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            mainAxisAlignment: MainAxisAlignment.spaceBetween,
            children: [
              Padding(
                padding: const EdgeInsets.only(left: 12),
                child: const Text(
                  "Recent Activity",
                  style: TextStyle(fontSize: 18, fontWeight: FontWeight.w500),
                ),
              ),
              if (context.desktop)
                OutlinedButton(
                  style: OutlinedButton.styleFrom(
                    backgroundColor: secondaryColor,
                    shape: RoundedRectangleBorder(
                      borderRadius: BorderRadius.circular(8),
                    ),
                    padding: EdgeInsets.symmetric(
                      horizontal: defaultPadding,
                      vertical: defaultPadding / (context.mobile ? 2 : 1),
                    ),
                  ),
                  onPressed: () {
                    context.push("/activities");
                  },
                  child: const Text(
                    "View All",
                    style: TextStyle(color: Colors.white54),
                  ),
                ),
            ],
          ),
          const SizedBox(height: defaultPadding),
          Responsive(
            mobileChild: ActivityLog(),
            desktopChild: Expanded(
              child: SingleChildScrollView(child: ActivityLog()),
            ),
          ),
        ],
      ),
    );

    return Column(
      mainAxisAlignment: MainAxisAlignment.spaceBetween,
      children: [
        Tilecontainer(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Padding(
                padding: const EdgeInsets.only(left: 12),
                child: const Text(
                  "Builds Per Month",
                  style: TextStyle(fontSize: 18, fontWeight: FontWeight.w500),
                ),
              ),
              Padding(
                padding: const EdgeInsets.only(left: 12, right: 12),
                child: BuildLineChart(),
              ),
            ],
          ),
        ),
        Responsive(
          mobileChild: activityWidget,
          desktopChild: Expanded(child: activityWidget),
        ),
      ],
    );
  }
}
