import 'package:aurcache/components/builds_table.dart';
import 'package:aurcache/models/build.dart';
import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/providers/api/builds_provider.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import '../../constants/color_constants.dart';
import '../table_info.dart';

class RecentBuilds extends StatelessWidget {
  const RecentBuilds({super.key});

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
          APIBuilder<BuildsProvider, List<Build>, BuildsDTO>(
            key: const Key("Builds on dashboard"),
            dto: BuildsDTO(limit: 10),
            interval: const Duration(seconds: 10),
            onLoad: () => const Text("no data"),
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
          ),
        ],
      ),
    );
  }
}
