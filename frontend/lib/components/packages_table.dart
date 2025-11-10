import 'package:aurcache/api/packages.dart';
import 'package:aurcache/providers/builds.dart';
import 'package:aurcache/providers/packages.dart';
import 'package:aurcache/providers/statistics.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';
import 'package:skeletonizer/skeletonizer.dart';
import 'package:toastification/toastification.dart';

import '../api/API.dart';
import '../constants/color_constants.dart';
import '../models/simple_packge.dart';
import '../utils/package_color.dart';

class PackagesTable extends ConsumerWidget {
  const PackagesTable({super.key, required this.data});
  final List<SimplePackage> data;

  static Widget loading() {
    final demoBuild = SimplePackage(
      id: 42,
      name: 'MyPackage',
      status: 0,
      latest_version: '1.0.0',
      latest_aur_version: '1.0.0',
      outofdate: false,
    );

    return Skeletonizer(
      child: PackagesTable(data: List.generate(20, (_) => demoBuild)),
    );
  }

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    return DataTable(
      horizontalMargin: 12,
      columnSpacing: defaultPadding,
      headingRowColor: WidgetStateProperty.resolveWith<Color?>((
        Set<WidgetState> states,
      ) {
        return Color(0xff131418);
      }),
      headingRowHeight: 50,
      columns: [
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("ID"))),
        DataColumn(label: Skeleton.keep(child: Text("Package Name"))),
        DataColumn(label: Skeleton.keep(child: Text("Version"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("Up-To-Date"))),
        DataColumn(label: Skeleton.keep(child: Text("Status"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("Action"))),
      ],
      rows: data
          .map((e) => buildDataRow(e, context, ref))
          .toList(growable: false),
    );
  }

  DataRow buildDataRow(
    SimplePackage package,
    BuildContext context,
    WidgetRef ref,
  ) {
    return DataRow(
      cells: [
        if (context.desktop) DataCell(Text(package.id.toString())),
        DataCell(
          Text(package.name),
          onTap: context.mobile
              ? () {
                  context.push("/package/${package.id}");
                }
              : null,
        ),
        DataCell(Text(package.latest_version.toString())),
        if (context.desktop)
          DataCell(
            IconButton(
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
                      } on DioException {
                        toastification.show(
                          title: Text('Failed to update package!'),
                          autoCloseDuration: const Duration(seconds: 5),
                          type: ToastificationType.error,
                        );
                      }

                      ref.invalidate(listPackagesProvider());
                      ref.invalidate(listBuildsProvider());
                      ref.invalidate(listStatsProvider);
                      ref.invalidate(getGraphDataProvider);
                    }
                  : null,
            ),
          ),
        DataCell(
          IconButton(
            icon: Icon(
              switchSuccessIcon(package.status),
              color: switchSuccessColor(package.status),
            ),
            onPressed: null,
          ),
        ),
        if (context.desktop)
          DataCell(
            OutlinedButton(
              style: OutlinedButton.styleFrom(
                backgroundColor: secondaryColor,
                side: BorderSide(color: primaryColor, width: 0),
                shape: RoundedRectangleBorder(
                  borderRadius: BorderRadius.circular(8),
                ),
                padding: EdgeInsets.symmetric(
                  horizontal: defaultPadding,
                  vertical: defaultPadding / 2,
                ),
              ),
              onPressed: () {
                context.push("/package/${package.id}");
              },
              child: const Text(
                "View",
                style: TextStyle(color: Colors.white54),
              ),
            ),
          ),
      ],
    );
  }
}
