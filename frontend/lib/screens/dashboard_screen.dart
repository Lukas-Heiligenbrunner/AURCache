import 'package:aurcache/api/statistics.dart';
import 'package:aurcache/components/api/ApiBuilder.dart';
import 'package:aurcache/models/simple_packge.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../api/API.dart';
import '../components/dashboard/header.dart';
import '../constants/color_constants.dart';
import '../models/build.dart';
import '../utils/responsive.dart';
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
    return APIBuilder(
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
                  MultiProvider(
                    providers: [
                      ChangeNotifierProvider(
                        create: (context) =>
                            APIController<List<SimplePackage>>(),
                      ),
                      ChangeNotifierProvider(
                        create: (context) => APIController<List<Build>>(),
                      ),
                    ],
                    child: Row(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Expanded(
                          flex: 5,
                          child: Column(
                            children: [
                              YourPackages(),
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
                    ),
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
      api: API.listStats,
    );
  }
}
