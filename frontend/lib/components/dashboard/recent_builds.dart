import 'package:aurcache/api/builds.dart';
import 'package:aurcache/components/builds_table.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:provider/provider.dart';
import '../../api/API.dart';
import '../../constants/color_constants.dart';
import '../../models/build.dart';
import '../api/ApiBuilder.dart';
import '../table_info.dart';

class RecentBuilds extends StatelessWidget {
  const RecentBuilds({super.key});

  @override
  Widget build(BuildContext context) {
    final apiController =
        Provider.of<APIController<List<Build>>>(context, listen: false);

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
            style: Theme.of(context).textTheme.titleMedium,
          ),
          APIBuilder(
            onLoad: () => const Text("no data"),
            controller: apiController,
            onData: (t) {
              if (t.isEmpty) {
                return const TableInfo(title: "You have no builds yet");
              } else {
                return Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    SizedBox(
                        width: double.infinity, child: BuildsTable(data: t)),
                    ElevatedButton(
                      onPressed: () {
                        context.push("/builds");
                      },
                      child: Text(
                        "List all Builds",
                        style: TextStyle(color: Colors.white.withOpacity(0.8)),
                      ),
                    ),
                  ],
                );
              }
            },
            api: () => API.listAllBuilds(limit: 10),
          ),
        ],
      ),
    );
  }
}
