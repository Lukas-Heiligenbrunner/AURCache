import 'package:aurcache/components/builds_table.dart';
import 'package:aurcache/models/build.dart';
import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/providers/builds_provider.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import '../../constants/color_constants.dart';

class RecentBuilds extends StatefulWidget {
  const RecentBuilds({
    Key? key,
  }) : super(key: key);

  @override
  State<RecentBuilds> createState() => _RecentBuildsState();
}

class _RecentBuildsState extends State<RecentBuilds> {
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
            child: APIBuilder<BuildsProvider, List<Build>, BuildsDTO>(
              key: const Key("Builds on dashboard"),
              dto: BuildsDTO(limit: 10),
              interval: const Duration(seconds: 10),
              onLoad: () => const Text("no data"),
              onData: (t) {
                return BuildsTable(data: t);
              },
            ),
          ),
          ElevatedButton(
            onPressed: () {
              context.push("/builds");
            },
            child: Text(
              "List all Builds",
              style: TextStyle(color: Colors.white.withOpacity(0.8)),
            ),
          )
        ],
      ),
    );
  }
}
