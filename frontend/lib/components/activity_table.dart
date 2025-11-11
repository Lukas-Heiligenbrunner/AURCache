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
          label: Text("Date"),
          columnWidth: IntrinsicColumnWidth(flex: 1),
        ),
        DataColumn(
          label: Text("User"),
          columnWidth: IntrinsicColumnWidth(flex: 2),
        ),
        DataColumn(
          label: Text("Message"),
          columnWidth: IntrinsicColumnWidth(flex: 5),
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
        DataCell(
          Text(
            '${activity.timestamp.day.toString().padLeft(2, '0')}.'
            '${activity.timestamp.month.toString().padLeft(2, '0')}.'
            '${activity.timestamp.year.toString()} '
            '${activity.timestamp.hour.toString().padLeft(2, '0')}:'
            '${activity.timestamp.minute.toString().padLeft(2, '0')}',
          ),
        ),
        DataCell(Text(activity.user ?? "You")),
        DataCell(Text(activity.text)),
      ],
    );
  }
}
