import 'package:aurcache/models/activity.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../constants/color_constants.dart';

class ActivityTable extends ConsumerWidget {
  const ActivityTable({super.key, required this.data});
  final List<Activity> data;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    return DataTable(
      horizontalMargin: 0,
      columnSpacing: defaultPadding,
      columns: const [
        DataColumn(
          label: Text("User"),
          columnWidth: IntrinsicColumnWidth(flex: 1),
        ),
        DataColumn(
          label: Text("Date"),
          columnWidth: IntrinsicColumnWidth(flex: 2),
        ),
        DataColumn(
          label: Text("Message"),
          columnWidth: IntrinsicColumnWidth(flex: 4),
        ),
      ],
      rows: data
          .map((e) => buildDataRow(e, context, ref))
          .toList(growable: false),
    );
  }

  DataRow buildDataRow(Activity activity, BuildContext context, WidgetRef ref) {
    return DataRow(
      cells: [
        DataCell(Text(activity.user ?? "You")),
        DataCell(Text(activity.timestamp.toString())),
        DataCell(Text(activity.text)),
      ],
    );
  }
}
