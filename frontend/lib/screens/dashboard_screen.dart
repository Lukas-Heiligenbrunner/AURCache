import 'package:aurcache/api/statistics.dart';
import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/providers/stats_provider.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/API.dart';
import '../components/dashboard/header.dart';
import '../constants/color_constants.dart';
import '../utils/responsive.dart';
import '../models/stats.dart';
import '../components/dashboard/quick_info_banner.dart';
import '../components/dashboard/recent_builds.dart';
import '../components/dashboard/your_packages.dart';
import '../components/dashboard/side_panel.dart';

class DashboardScreen extends StatefulWidget {
  @override
  State<DashboardScreen> createState() => _DashboardScreenState();
}

class _DashboardScreenState extends State<DashboardScreen> {
  @override
  Widget build(BuildContext context) {
    return APIBuilder<StatsProvider, Stats, Object>(
      interval: const Duration(seconds: 10),
      onData: (stats) {
        return SafeArea(
          child: SingleChildScrollView(
            child: Container(
              padding: const EdgeInsets.all(defaultPadding),
              child: Column(
                children: [
                  const Header(),
                  const SizedBox(height: defaultPadding),
                  QuickInfoBanner(
                    stats: stats,
                  ),
                  const SizedBox(height: defaultPadding),
                  Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Expanded(
                        flex: 5,
                        child: Column(
                          children: [
                            const YourPackages(),
                            const SizedBox(height: defaultPadding),
                            const RecentBuilds(),
                            if (Responsive.isMobile(context))
                              const SizedBox(height: defaultPadding),
                            if (Responsive.isMobile(context))
                              SidePanel(
                                  nrbuilds: stats.total_builds,
                                  nrfailedbuilds: stats.failed_builds,
                                  nrActiveBuilds: stats.active_builds),
                          ],
                        ),
                      ),
                      if (!Responsive.isMobile(context))
                        const SizedBox(width: defaultPadding),
                      // On Mobile means if the screen is less than 850 we dont want to show it
                      if (!Responsive.isMobile(context))
                        Expanded(
                          flex: 2,
                          child: SidePanel(
                              nrbuilds: stats.total_builds,
                              nrfailedbuilds: stats.failed_builds,
                              nrActiveBuilds: stats.active_builds),
                        ),
                    ],
                  )
                ],
              ),
            ),
          ),
        );
      },
      onLoad: () {
        return Text("loading");
      },
    );
  }
}
