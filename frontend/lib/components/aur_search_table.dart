import 'package:aurcache/api/packages.dart';
import 'package:aurcache/models/aur_package.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import '../api/API.dart';
import '../constants/color_constants.dart';
import 'add_package_popup.dart';
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
              final confirmResult = await showPackageAddPopup(
                  context, package.name, (archs) async {
                await API.addPackage(name: package.name, selectedArchs: archs);
                context.go("/");
              });
              if (!confirmResult) return;
            },
          ),
        ),
      ],
    );
  }
}
