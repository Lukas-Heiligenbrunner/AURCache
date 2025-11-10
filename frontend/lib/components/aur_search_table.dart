import 'package:aurcache/api/packages.dart';
import 'package:aurcache/models/aur_package.dart';
import 'package:aurcache/providers/builds.dart';
import 'package:aurcache/providers/packages.dart';
import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';
import 'package:toastification/toastification.dart';
import '../api/API.dart';
import '../constants/color_constants.dart';
import '../providers/activity_log.dart';
import '../providers/statistics.dart';
import 'add_package_popup.dart';

class AurSearchTable extends ConsumerWidget {
  const AurSearchTable({super.key, required this.data});
  final List<AurPackage> data;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    return DataTable(
      horizontalMargin: 0,
      columnSpacing: defaultPadding,
      columns: const [
        DataColumn(label: Text("Package Name")),
        DataColumn(label: Text("Version")),
        DataColumn(label: Text("Action")),
      ],
      rows: data
          .map((e) => buildDataRow(e, context, ref))
          .toList(growable: false),
    );
  }

  DataRow buildDataRow(
    AurPackage package,
    BuildContext context,
    WidgetRef ref,
  ) {
    return DataRow(
      cells: [
        DataCell(Text(package.name)),
        DataCell(Text(package.version.toString())),
        DataCell(
          TextButton(
            child: const Text("Install", style: TextStyle(color: greenColor)),
            onPressed: () async {
              final confirmResult = await showPackageAddPopup(
                context,
                package.name,
                (archs) async {
                  try {
                    await API.addPackage(
                      name: package.name,
                      selectedArchs: archs,
                    );
                  } on DioException catch (e) {
                    print(e);
                    toastification.show(
                      title: Text('Failed to add package!'),
                      autoCloseDuration: const Duration(seconds: 5),
                      type: ToastificationType.error,
                    );
                  }

                  // invalidate all dashboard providers
                  ref.invalidate(listActivitiesProvider);
                  ref.invalidate(listPackagesProvider);
                  ref.invalidate(listBuildsProvider);
                  ref.invalidate(listStatsProvider);
                  ref.invalidate(getGraphDataProvider);

                  if (context.mounted) {
                    context.go("/");
                  }
                },
              );
              if (!confirmResult) return;
            },
          ),
        ),
      ],
    );
  }
}
