import 'dart:async';

import 'package:aurcache/api/builds.dart';
import 'package:aurcache/models/build.dart';
import 'package:aurcache/components/dashboard/your_packages.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';

import '../../api/API.dart';
import '../../constants/color_constants.dart';

class RecentBuilds extends StatefulWidget {
  const RecentBuilds({
    Key? key,
  }) : super(key: key);

  @override
  State<RecentBuilds> createState() => _RecentBuildsState();
}

class _RecentBuildsState extends State<RecentBuilds> {
  late Future<List<Build>> dataFuture;
  Timer? timer;

  @override
  void initState() {
    super.initState();
    dataFuture = API.listAllBuilds();

    timer = Timer.periodic(
        const Duration(seconds: 10),
        (Timer t) => setState(() {
              dataFuture = API.listAllBuilds();
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
            "Recent Builds",
            style: Theme.of(context).textTheme.subtitle1,
          ),
          SizedBox(
            width: double.infinity,
            child: FutureBuilder(
                future: dataFuture,
                builder: (context, snapshot) {
                  if (snapshot.hasData) {
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
                          label: Text("Status"),
                        ),
                      ],
                      rows: snapshot.data!
                          .map((e) => recentUserDataRow(e))
                          .toList(),
                    );
                  } else {
                    return const Text("no data");
                  }
                }),
          ),
        ],
      ),
    );
  }

  DataRow recentUserDataRow(Build build) {
    return DataRow(
      cells: [
        DataCell(Text(build.id.toString())),
        DataCell(Text(build.pkg_name)),
        DataCell(Text(build.version)),
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
