import 'package:flutter/material.dart';

import '../../../core/constants/color_constants.dart';

class RecentBuilds extends StatelessWidget {
  const RecentBuilds({
    Key? key,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: EdgeInsets.all(defaultPadding),
      decoration: BoxDecoration(
        color: secondaryColor,
        borderRadius: const BorderRadius.all(Radius.circular(10)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            "Recent Builds",
            style: Theme.of(context).textTheme.subtitle1,
          ),
          SizedBox(
            width: double.infinity,
            child: DataTable(
              horizontalMargin: 0,
              columnSpacing: defaultPadding,
              columns: [
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
                  label: Text("Status"),
                ),
              ],
              rows: List.generate(
                7,
                (index) => recentUserDataRow(),
              ),
            ),
          ),
        ],
      ),
    );
  }

  DataRow recentUserDataRow() {
    return DataRow(
      cells: [
        DataCell(Text("1")),
        DataCell(Text("Resources")),
        DataCell(Text("v1.2.3")),
        DataCell(Icon(Icons.watch_later_outlined, color: Color(0xFF9D8D00),)),
      ],
    );
  }
}
