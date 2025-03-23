import 'package:aurcache/components/api/api_builder.dart';
import 'package:aurcache/models/simple_packge.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../components/dashboard/header.dart';
import '../constants/color_constants.dart';
import '../models/build.dart';
import '../utils/responsive.dart';
import '../components/dashboard/quick_info_banner.dart';
import '../components/dashboard/dashboard_tables.dart';
import '../components/dashboard/side_panel.dart';

class DashboardScreen extends StatefulWidget {
  const DashboardScreen({super.key});

  @override
  State<DashboardScreen> createState() => _DashboardScreenState();
}

class _DashboardScreenState extends State<DashboardScreen> {
  @override
  Widget build(BuildContext context) {
    return SafeArea(
      child: Builder(builder: (context) {
        final allScreen = Container(
          padding: const EdgeInsets.only(
              top: defaultPadding,
              left: defaultPadding / 2,
              right: defaultPadding / 2,
              bottom: defaultPadding / 2),
          child: Column(
            mainAxisAlignment: MainAxisAlignment.start,
            children: [
              Padding(
                padding: const EdgeInsets.only(left: defaultPadding),
                child: const Header(),
              ),
              const SizedBox(height: defaultPadding),
              QuickInfoBanner(),
              Responsive(
                  mobileChild: _buildMobileBody(),
                  desktopChild: _buildDesktopBody())
            ],
          ),
        );

        if (context.mobile) {
          return SingleChildScrollView(
            child: allScreen,
          );
        } else {
          return allScreen;
        }
      }),
    );
  }

  Widget _buildDesktopBody() {
    return Expanded(
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Expanded(
            flex: 3,
            child: DashboardTables(),
          ),
          Expanded(
            flex: 2,
            child: SidePanel(),
          ),
        ],
      ),
    );
  }

  Widget _buildMobileBody() {
    return Column(
      mainAxisAlignment: MainAxisAlignment.start,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        DashboardTables(),
        SidePanel(),
      ],
    );
  }
}
