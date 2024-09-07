import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';

import '../constants/color_constants.dart';
import '../models/build.dart';
import '../utils/package_color.dart';

class BuildsTable extends StatelessWidget {
  const BuildsTable({super.key, required this.data});
  final List<Build> data;

  @override
  Widget build(BuildContext context) {
    return DataTable(
      horizontalMargin: 0,
      columnSpacing: defaultPadding,
      columns: const [
        DataColumn(
          label: Text("Build ID"),
        ),
        DataColumn(
          label: Text("Package Name"),
        ),
        DataColumn(
          label: Text("Version"),
        ),
        DataColumn(
          label: Text("Platform"),
        ),
        DataColumn(
          label: Text("Status"),
        ),
      ],
      rows: data.map((e) => buildDataRow(context, e)).toList(),
    );
  }

  DataRow buildDataRow(BuildContext context, Build build) {
    return DataRow(
      cells: [
        DataCell(Text(build.id.toString())),
        DataCell(Text(build.pkg_name)),
        DataCell(Text(build.version)),
        DataCell(Text("x86_64")),
        DataCell(IconButton(
          icon: Icon(
            switchSuccessIcon(build.status),
            color: switchSuccessColor(build.status),
          ),
          onPressed: () {
            context.push("/build/${build.id}");
          },
        )),
      ],
    );
  }
}
