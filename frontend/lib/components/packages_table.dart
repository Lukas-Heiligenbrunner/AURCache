import 'package:aurcache/api/packages.dart';
import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:provider/provider.dart';
import 'package:toastification/toastification.dart';

import '../api/API.dart';
import '../constants/color_constants.dart';
import '../models/simple_packge.dart';
import '../providers/api/builds_provider.dart';
import '../providers/api/packages_provider.dart';
import '../providers/api/stats_provider.dart';
import '../utils/package_color.dart';
import 'confirm_popup.dart';

class PackagesTable extends StatelessWidget {
  const PackagesTable({super.key, required this.data});
  final List<SimplePackage> data;

  @override
  Widget build(BuildContext context) {
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
            label: Text("Version"),
          ),
          DataColumn(
            label: Text("Up-To-Date"),
          ),
          DataColumn(
            label: Text("Status"),
          ),
          DataColumn(
            label: Text("Action"),
          ),
        ],
        rows:
            data.map((e) => buildDataRow(e, context)).toList(growable: false));
  }

  DataRow buildDataRow(SimplePackage package, BuildContext context) {
    return DataRow(
      cells: [
        DataCell(Text(package.id.toString())),
        DataCell(Text(package.name)),
        DataCell(Text(package.latest_version.toString())),
        DataCell(IconButton(
          icon: Icon(
            package.outofdate ? Icons.update : Icons.verified,
            color: package.outofdate
                ? const Color(0xFF6B43A4)
                : const Color(0xFF0A6900),
          ),
          onPressed: package.outofdate
              ? () async {
                  try {
                    await API.updatePackage(id: package.id);
                  } on DioException catch (e) {
                    toastification.show(
                      title: Text('Failed to update package!'),
                      autoCloseDuration: const Duration(seconds: 5),
                      type: ToastificationType.error,
                    );
                  }

                  Provider.of<PackagesProvider>(context, listen: false)
                      .refresh(context);
                  Provider.of<BuildsProvider>(context, listen: false)
                      .refresh(context);
                  Provider.of<StatsProvider>(context, listen: false)
                      .refresh(context);
                }
              : null,
        )),
        DataCell(IconButton(
          icon: Icon(
            switchSuccessIcon(package.status),
            color: switchSuccessColor(package.status),
          ),
          onPressed: () {
            //context.push("/build/${package.latest_version_id}");
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
                  await showConfirmationDialog(context, "Delete Package",
                      "Are you sure to delete this Package?", () async {
                    final succ = await API.deletePackage(package.id);
                    if (succ) {
                      Provider.of<PackagesProvider>(context, listen: false)
                          .refresh(context);
                      Provider.of<BuildsProvider>(context, listen: false)
                          .refresh(context);
                      Provider.of<StatsProvider>(context, listen: false)
                          .refresh(context);
                    } else {
                      toastification.show(
                        title: Text('Failed to delete Package!'),
                        autoCloseDuration: const Duration(seconds: 5),
                        type: ToastificationType.error,
                      );
                    }
                  }, null);
                },
              ),
            ],
          ),
        ),
      ],
    );
  }
}
