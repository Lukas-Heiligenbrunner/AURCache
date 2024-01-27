import 'package:aurcache/api/packages.dart';
import 'package:aurcache/models/aur_package.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import '../api/API.dart';
import '../constants/color_constants.dart';
import 'confirm_popup.dart';

class AurSearchTable extends StatelessWidget {
  const AurSearchTable({super.key, required this.data});
  final List<AurPackage> data;

  @override
  Widget build(BuildContext context) {
    return DataTable(
        horizontalMargin: 0,
        columnSpacing: defaultPadding,
        columns: const [
          DataColumn(
            label: Text("Package Name"),
          ),
          DataColumn(
            label: Text("Version"),
          ),
          DataColumn(
            label: Text("Action"),
          ),
        ],
        rows:
            data.map((e) => buildDataRow(e, context)).toList(growable: false));
  }

  DataRow buildDataRow(AurPackage package, BuildContext context) {
    return DataRow(
      cells: [
        DataCell(Text(package.name)),
        DataCell(Text(package.version.toString())),
        DataCell(
          TextButton(
            child: const Text("Install", style: TextStyle(color: greenColor)),
            onPressed: () async {
              final confirmResult = await showConfirmationDialog(
                  context,
                  "Install Package?",
                  "Are you sure to install Package: ${package.name}", () async {
                await API.addPackage(name: package.name);
                context.go("/");
              }, null);
              if (!confirmResult) return;
            },
          ),
        ),
      ],
    );
  }
}
