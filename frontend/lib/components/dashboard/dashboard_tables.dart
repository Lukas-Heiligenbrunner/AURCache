import 'package:aurcache/components/dashboard/tile_container.dart';
import 'package:aurcache/components/packages_table.dart';
import 'package:aurcache/providers/builds.dart';
import 'package:aurcache/providers/packages.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:toggle_switch/toggle_switch.dart';
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
        onData: (data) {
          if (data.isEmpty) {
            return const TableInfo(title: "You have no packages yet");
          } else {
            return Responsive(
                mobileChild: PackagesTable(data: data),
                desktopChild: SingleChildScrollView(
                  child: PackagesTable(data: data),
                ));
          }
        },
        onLoad: () => PackagesTable.loading(),
        provider: listPackagesProvider(limit: 20),
      );
    } else {
      return APIBuilder(
        onLoad: () => BuildsTable.loading(),
        onData: (data) {
          if (data.isEmpty) {
            return const TableInfo(title: "You have no builds yet");
          } else {
            return Responsive(
                mobileChild: BuildsTable(data: data),
                desktopChild: SingleChildScrollView(
                  child: BuildsTable(data: data),
                ));
          }
        },
        provider: listBuildsProvider(limit: 20),
      );
    }
  }
}
