import 'dart:async';

import 'package:aurcache/api/packages.dart';
import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/providers/packages_provider.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:provider/provider.dart';

import '../../api/API.dart';
import '../../constants/color_constants.dart';
import '../../models/package.dart';
import '../../providers/builds_provider.dart';
import '../../providers/stats_provider.dart';
import '../confirm_popup.dart';

class YourPackages extends StatefulWidget {
  const YourPackages({
    Key? key,
  }) : super(key: key);

  @override
  State<YourPackages> createState() => _YourPackagesState();
}

class _YourPackagesState extends State<YourPackages> {
  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.all(defaultPadding),
      decoration: const BoxDecoration(
        color: secondaryColor,
        borderRadius: BorderRadius.all(Radius.circular(10)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            "Your Packages",
            style: Theme.of(context).textTheme.subtitle1,
          ),
          SingleChildScrollView(
            //scrollDirection: Axis.horizontal,
            child: SizedBox(
              width: double.infinity,
              child: APIBuilder<PackagesProvider, List<Package>, Object>(
                key: Key("Packages on dashboard"),
                interval: const Duration(seconds: 10),
                onData: (data) {
                  return DataTable(
                      horizontalMargin: 0,
                      columnSpacing: defaultPadding,
                      columns: const [
                        DataColumn(
                          label: Text("Package ID"),
                        ),
                        DataColumn(
                          label: Text("Package Name"),
                        ),
                        DataColumn(
                          label: Text("Number of versions"),
                        ),
                        DataColumn(
                          label: Text("Status"),
                        ),
                        DataColumn(
                          label: Text("Action"),
                        ),
                      ],
                      rows: data
                          .map((e) => buildDataRow(e))
                          .toList(growable: false));
                },
                onLoad: () => const Text("No data"),
              ),
            ),
          ),
        ],
      ),
    );
  }

  DataRow buildDataRow(Package package) {
    return DataRow(
      cells: [
        DataCell(Text(package.id.toString())),
        DataCell(Text(package.name)),
        DataCell(Text(package.count.toString())),
        DataCell(IconButton(
          icon: Icon(
            switchSuccessIcon(package.status),
            color: switchSuccessColor(package.status),
          ),
          onPressed: () {
            // todo open build info with logs
          },
        )),
        DataCell(
          Row(
            children: [
              TextButton(
                child: const Text('View', style: TextStyle(color: greenColor)),
                onPressed: () {
                  context.push("/package/${package.id}");
                },
              ),
              const SizedBox(
                width: 6,
              ),
              TextButton(
                child: const Text("Delete",
                    style: TextStyle(color: Colors.redAccent)),
                onPressed: () async {
                  final confirmResult =
                      await showDeleteConfirmationDialog(context);
                  if (!confirmResult) return;

                  final succ = await API.deletePackage(package.id);
                  if (succ) {
                    Provider.of<PackagesProvider>(context, listen: false)
                        .refresh(context);
                    Provider.of<BuildsProvider>(context, listen: false)
                        .refresh(context);
                    Provider.of<StatsProvider>(context, listen: false)
                        .refresh(context);
                  }
                },
              ),
            ],
          ),
        ),
      ],
    );
  }
}

IconData switchSuccessIcon(int status) {
  switch (status) {
    case 0:
      return Icons.watch_later_outlined;
    case 1:
      return Icons.check_circle_outline;
    case 2:
      return Icons.cancel_outlined;
    default:
      return Icons.question_mark_outlined;
  }
}

Color switchSuccessColor(int status) {
  switch (status) {
    case 0:
      return const Color(0xFF9D8D00);
    case 1:
      return const Color(0xFF0A6900);
    case 2:
      return const Color(0xff760707);
    default:
      return const Color(0xFF9D8D00);
  }
}
