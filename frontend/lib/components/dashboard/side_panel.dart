import 'package:aurcache/components/dashboard/chart_card.dart';
import 'package:flutter/material.dart';

import '../../constants/color_constants.dart';
import 'builds_chart.dart';

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
    final nrBuilds = nrSuccessfulBuilds + nrfailedbuilds + nrEnqueuedBuilds;

    return Container(
      padding: const EdgeInsets.all(defaultPadding),
      decoration: const BoxDecoration(
        color: secondaryColor,
        borderRadius: BorderRadius.all(Radius.circular(10)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const Text(
            "Package build success",
            style: TextStyle(
              fontSize: 18,
              fontWeight: FontWeight.w500,
            ),
          ),
          const SizedBox(height: defaultPadding),
          nrBuilds > 0
              ? BuildsChart(
                  nrBuilds: nrBuilds,
                  nrSuccessfulBuilds: nrSuccessfulBuilds,
                  nrfailedbuilds: nrfailedbuilds,
                  nrEnqueuedBuilds: nrEnqueuedBuilds)
              : const SizedBox(
                  width: double.infinity,
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.center,
                    mainAxisAlignment: MainAxisAlignment.center,
                    children: [
                      SizedBox(
                        height: 15,
                      ),
                      Icon(
                        Icons.info_outline_rounded,
                        size: 42,
                      ),
                      SizedBox(
                        height: 15,
                      ),
                      Text("Add Packages to view Graph"),
                      SizedBox(
                        height: 30,
                      )
                    ],
                  ),
                ),
          SideCard(
            color: const Color(0xff0a7005),
            title: "Successful Builds",
            textRight:
                "${(nrBuilds != 0 ? (nrSuccessfulBuilds * 100 / nrBuilds) : 0).toStringAsFixed(2)}%",
            subtitle: (nrSuccessfulBuilds).toString(),
          ),
          SideCard(
            color: const Color(0xff760707),
            title: "Failed Builds",
            textRight:
                "${(nrBuilds != 0 ? (nrfailedbuilds * 100 / nrBuilds) : 0).toStringAsFixed(2)}%",
            subtitle: nrfailedbuilds.toString(),
          ),
          SideCard(
            color: const Color(0xFF0044AA),
            title: "Enqueued Builds",
            textRight:
                "${(nrBuilds != 0 ? (nrEnqueuedBuilds * 100 / nrBuilds) : 0).toStringAsFixed(2)}%",
            subtitle: nrEnqueuedBuilds.toString(),
          ),
        ],
      ),
    );
  }
}
