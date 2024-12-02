import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/providers/api/stats_provider.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../components/dashboard/header.dart';
import '../constants/color_constants.dart';
import '../providers/api/builds_provider.dart';
import '../providers/api/packages_provider.dart';
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
    return MultiProvider(
      providers: [
        ChangeNotifierProvider<StatsProvider>(create: (_) => StatsProvider()),
        ChangeNotifierProvider<PackagesProvider>(
            create: (_) => PackagesProvider()),
        ChangeNotifierProvider<BuildsProvider>(create: (_) => BuildsProvider()),
      ],
      child: APIBuilder<StatsProvider, Stats, Object>(
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
                              if (context.mobile)
                                const SizedBox(height: defaultPadding),
                              if (context.mobile)
                                SidePanel(
                                    nrSuccessfulBuilds: stats.successful_builds,
                                    nrfailedbuilds: stats.failed_builds,
                                    nrEnqueuedBuilds: stats.enqueued_builds),
                            ],
                          ),
                        ),
                        if (!context.mobile)
                          const SizedBox(width: defaultPadding),
                        // On Mobile means if the screen is less than 850 we dont want to show it
                        if (!context.mobile)
                          Expanded(
                            flex: 2,
                            child: SidePanel(
                                nrSuccessfulBuilds: stats.successful_builds,
                                nrfailedbuilds: stats.failed_builds,
                                nrEnqueuedBuilds: stats.enqueued_builds),
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
      ),
    );
  }
}
