import 'dart:async';

import 'package:aurcache/api/packages.dart';
import 'package:flutter/material.dart';

import '../../api/API.dart';
import '../../constants/color_constants.dart';
import '../../models/package.dart';

class YourPackages extends StatefulWidget {
  const YourPackages({
    Key? key,
  }) : super(key: key);

  @override
  State<YourPackages> createState() => _YourPackagesState();
}

class _YourPackagesState extends State<YourPackages> {
  late Future<List<Package>> dataFuture;
  Timer? timer;

  @override
  void initState() {
    super.initState();
    dataFuture = API.listPackages();

    timer = Timer.periodic(
        const Duration(seconds: 10),
        (Timer t) => setState(() {
              dataFuture = API.listPackages();
            }));
  }

  @override
  void dispose() {
    super.dispose();
    timer?.cancel();
  }

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.all(defaultPadding),
      decoration: const BoxDecoration(
        color: secondaryColor,
        borderRadius: BorderRadius.all(Radius.circular(10)),
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
              child: FutureBuilder(
                builder: (context, snapshot) {
                  if (snapshot.hasData) {
                    return DataTable(
                      horizontalMargin: 0,
                      columnSpacing: defaultPadding,
                      columns: const [
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
                      rows: snapshot.data!
                          .map((e) => buildDataRow(e))
                          .toList(growable: false),
                    );
                  } else {
                    return const Text("No data");
                  }
                },
                future: dataFuture,
              ),
            ),
          ),
        ],
      ),
    );
  }

  DataRow buildDataRow(Package package) {
    return DataRow(
      cells: [
        DataCell(Text(package.id.toString())),
        DataCell(Text(package.name)),
        DataCell(Text(package.count.toString())),
        DataCell(IconButton(
          icon: Icon(
            switchSuccessIcon(package.status),
            color: switchSuccessColor(package.status),
          ),
          onPressed: () {
            // todo open build info with logs
          },
        )),
        DataCell(
          Row(
            children: [
              TextButton(
                child: const Text('View', style: TextStyle(color: greenColor)),
                onPressed: () {},
              ),
              const SizedBox(
                width: 6,
              ),
              TextButton(
                child: const Text("Delete",
                    style: TextStyle(color: Colors.redAccent)),
                onPressed: () {},
              ),
            ],
          ),
        ),
      ],
    );
  }
}

IconData switchSuccessIcon(int status) {
  switch (status) {
    case 0:
      return Icons.watch_later_outlined;
    case 1:
      return Icons.check_circle_outline;
    case 2:
      return Icons.cancel_outlined;
    default:
      return Icons.question_mark_outlined;
  }
}

Color switchSuccessColor(int status) {
  switch (status) {
    case 0:
      return const Color(0xFF9D8D00);
    case 1:
      return const Color(0xFF0A6900);
    case 2:
      return const Color(0xff760707);
    default:
      return const Color(0xFF9D8D00);
  }
}
