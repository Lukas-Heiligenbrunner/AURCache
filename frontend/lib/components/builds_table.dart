import 'package:aurcache/utils/responsive.dart';
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
      horizontalMargin: 12,
      columnSpacing: defaultPadding,
      headingRowColor:
          WidgetStateProperty.resolveWith<Color?>((Set<WidgetState> states) {
        return Color(0xff131418);
      }),
      headingRowHeight: 50,
      columns: [
        if (context.desktop)
          DataColumn(
            label: Text("Build ID"),
          ),
        DataColumn(
          label: Text("Package Name"),
        ),
        DataColumn(
          label: Text("Version"),
        ),
        if (context.desktop)
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
        if (context.desktop) DataCell(Text(build.id.toString())),
        DataCell(Text(build.pkg_name),
            onTap: context.mobile
                ? () => context.push("/build/${build.id}")
                : null),
        DataCell(Text(build.version)),
        if (context.desktop) DataCell(Text(build.platform)),
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
