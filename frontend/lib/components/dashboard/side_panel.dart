import 'package:aurcache/components/dashboard/chart_card.dart';
import 'package:flutter/material.dart';

import '../../constants/color_constants.dart';
import 'builds_chart.dart';

class SidePanel extends StatelessWidget {
  const SidePanel({
    Key? key,
    required this.nrbuilds,
    required this.nrfailedbuilds,
    required this.nrActiveBuilds,
  }) : super(key: key);

  final int nrbuilds;
  final int nrfailedbuilds;
  final int nrActiveBuilds;

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: EdgeInsets.all(defaultPadding),
      decoration: BoxDecoration(
        color: secondaryColor,
        borderRadius: const BorderRadius.all(Radius.circular(10)),
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
          BuildsChart(
              nrbuilds: nrbuilds,
              nrfailedbuilds: nrfailedbuilds,
              nrActiveBuilds: nrActiveBuilds),
          SideCard(
            color: const Color(0xff0a7005),
            title: "Successful Builds",
            textRight:
                "${((nrbuilds - nrfailedbuilds) * 100 / nrbuilds).toStringAsFixed(2)}%",
            subtitle: (nrbuilds - nrfailedbuilds).toString(),
          ),
          SideCard(
            color: const Color(0xff760707),
            title: "Failed Builds",
            textRight:
                "${(nrfailedbuilds * 100 / nrbuilds).toStringAsFixed(2)}%",
            subtitle: nrfailedbuilds.toString(),
          ),
          SideCard(
            color: const Color(0xff9d8d00),
            title: "Active Builds",
            textRight:
                "${(nrActiveBuilds * 100 / nrbuilds).toStringAsFixed(2)}%",
            subtitle: nrActiveBuilds.toString(),
          ),
        ],
      ),
    );
  }
}
