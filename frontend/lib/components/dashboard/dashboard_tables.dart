import 'package:aurcache/api/builds.dart';
import 'package:aurcache/api/packages.dart';
import 'package:aurcache/components/dashboard/tile_container.dart';
import 'package:aurcache/components/packages_table.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:toggle_switch/toggle_switch.dart';
import '../../api/API.dart';
import '../../constants/color_constants.dart';
import '../api/api_builder.dart';
import '../builds_table.dart';
import '../table_info.dart';

class DashboardTables extends StatefulWidget {
  const DashboardTables({super.key});

  @override
  State<DashboardTables> createState() => _DashboardTablesState();
}

class _DashboardTablesState extends State<DashboardTables> {
  int activePage = 0;

  @override
  Widget build(BuildContext context) {
    final toggle = MouseRegion(
      cursor: SystemMouseCursors.click,
      child: ToggleSwitch(
        initialLabelIndex: activePage,
        totalSwitches: 2,
        labels: ['Recent Packages', 'Recent Builds'],
        onToggle: (index) {
          setState(() {
            activePage = index!;
          });
        },
        radiusStyle: true,
        activeBgColor: [Color(0xff292E35)],
        inactiveBgColor: Color(0x292E354F),
        cornerRadius: 8,
        customWidths: [135, 115],
      ),
    );

    return Tilecontainer(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.only(left: 0, bottom: 24),
            child: Row(
              mainAxisAlignment: MainAxisAlignment.spaceBetween,
              children: [
                toggle,
                if (context.desktop)
                  OutlinedButton(
                    style: OutlinedButton.styleFrom(
                      backgroundColor: secondaryColor,
                      shape: RoundedRectangleBorder(
                          borderRadius: BorderRadius.circular(8)),
                      padding: EdgeInsets.symmetric(
                        horizontal: defaultPadding,
                        vertical: defaultPadding / (context.mobile ? 2 : 1),
                      ),
                    ),
                    onPressed: () {
                      if (activePage == 0) {
                        context.push("/packages");
                      } else {
                        context.push("/builds");
                      }
                    },
                    child: const Text(
                      "View All",
                      style: TextStyle(color: Colors.white54),
                    ),
                  ),
              ],
            ),
          ),
          Responsive(
              mobileChild: _buildActivePage(),
              desktopChild: Expanded(child: _buildActivePage()))
        ],
      ),
    );
  }

  Widget _buildActivePage() {
    if (activePage == 0) {
      return APIBuilder(
        refreshOnComeback: true,
        onData: (data) {
          if (data.isEmpty) {
            return const TableInfo(title: "You have no packages yet");
          } else {
            return PackagesTable(data: data);
          }
        },
        onLoad: () => const CircularProgressIndicator(),
        api: () => API.listPackages(limit: 10),
      );
    } else {
      return APIBuilder(
        onLoad: () => const CircularProgressIndicator(),
        refreshOnComeback: true,
        onData: (data) {
          if (data.isEmpty) {
            return const TableInfo(title: "You have no builds yet");
          } else {
            return BuildsTable(data: data);
          }
        },
        api: () => API.listAllBuilds(limit: 10),
      );
    }
  }
}
