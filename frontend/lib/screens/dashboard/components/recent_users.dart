import 'package:flutter/material.dart';

import '../../../core/constants/color_constants.dart';

class RecentUsers extends StatelessWidget {
  const RecentUsers({
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
            "Your Packages",
            style: Theme.of(context).textTheme.subtitle1,
          ),
          SingleChildScrollView(
            //scrollDirection: Axis.horizontal,
            child: SizedBox(
              width: double.infinity,
              child: DataTable(
                horizontalMargin: 0,
                columnSpacing: defaultPadding,
                columns: [
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
                rows: List.generate(
                  7,
                  (index) => recentUserDataRow(context),
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

DataRow recentUserDataRow(BuildContext context) {
  return DataRow(
    cells: [
      DataCell(Text("1")),
      DataCell(Text("Resources")),
      DataCell(Text("2")),
      DataCell(Icon(Icons.watch_later_outlined, color: Color(0xFF9D8D00),)),
      DataCell(
        Row(
          children: [
            TextButton(
              child: Text('View', style: TextStyle(color: greenColor)),
              onPressed: () {},
            ),
            SizedBox(
              width: 6,
            ),
            TextButton(
              child: Text("Delete", style: TextStyle(color: Colors.redAccent)),
              onPressed: () {

              },
              // Delete
            ),
          ],
        ),
      ),
    ],
  );
}
