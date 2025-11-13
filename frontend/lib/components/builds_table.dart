import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:skeletonizer/skeletonizer.dart';

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
      headingRowColor: WidgetStateProperty.resolveWith<Color?>((
        Set<WidgetState> states,
      ) {
        return Color(0xff131418);
      }),
      headingRowHeight: 50,
      columns: [
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("Build ID"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("Date"))),
        DataColumn(label: Skeleton.keep(child: Text("Package Name"))),
        DataColumn(label: Skeleton.keep(child: Text("Version"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("Platform"))),
        DataColumn(label: Skeleton.keep(child: Text("Status"))),
      ],
      rows: data.map((e) => buildDataRow(context, e)).toList(),
    );
  }

  DataRow buildDataRow(BuildContext context, Build build) {
    return DataRow(
      cells: [
        if (context.desktop) DataCell(Text(build.id.toString())),
        if (context.desktop)
          DataCell(
            Text(
              '${build.start_time.day.toString().padLeft(2, '0')}.${build.start_time.month.toString().padLeft(2, '0')}.${build.start_time.year.toString()}',
            ),
          ),
        DataCell(
          Text(build.pkg_name),
          onTap: context.mobile
              ? () => context.push("/build/${build.id}")
              : null,
        ),
        DataCell(Text(build.version)),
        if (context.desktop) DataCell(Text(build.platform)),
        DataCell(
          IconButton(
            icon: Icon(
              switchSuccessIcon(build.status),
              color: switchSuccessColor(build.status),
            ),
            onPressed: () {
              context.push("/build/${build.id}");
            },
          ),
        ),
      ],
    );
  }
}
