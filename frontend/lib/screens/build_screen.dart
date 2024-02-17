import 'package:aurcache/api/builds.dart';
import 'package:aurcache/api/packages.dart';
import 'package:aurcache/components/build_output.dart';
import 'package:aurcache/models/build.dart';
import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/providers/build_log_provider.dart';
import 'package:aurcache/utils/time_formatter.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:provider/provider.dart';

import '../api/API.dart';
import '../components/confirm_popup.dart';
import '../components/dashboard/chart_card.dart';
import '../components/dashboard/your_packages.dart';
import '../constants/color_constants.dart';
import '../providers/api/build_provider.dart';

class BuildScreen extends StatefulWidget {
  const BuildScreen({super.key, required this.buildID});

  final int buildID;

  @override
  State<BuildScreen> createState() => _BuildScreenState();
}

class _BuildScreenState extends State<BuildScreen> {
  bool scrollFollowActive = true;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: MultiProvider(
        providers: [
          ChangeNotifierProvider<BuildProvider>(create: (_) => BuildProvider()),
          ChangeNotifierProvider<BuildLogProvider>(
              create: (_) => BuildLogProvider()),
        ],
        builder: (context, child) {
          return APIBuilder<BuildProvider, Build, BuildDTO>(
              key: const Key("Build on seperate page"),
              dto: BuildDTO(buildID: widget.buildID),
              interval: const Duration(seconds: 10),
              onLoad: () => const Text("loading"),
              onData: (buildData) {
                return Row(
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  children: [
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        mainAxisAlignment: MainAxisAlignment.start,
                        children: [
                          _buildTopBar(buildData, context),
                          const SizedBox(
                            height: 15,
                          ),
                          _buildPage(buildData)
                        ],
                      ),
                    ),
                    _buildSideBar(buildData),
                  ],
                );
              });
        },
      ),
    );
  }

  Widget _buildTopBar(Build buildData, BuildContext context) {
    final start_time =
        DateTime.fromMillisecondsSinceEpoch((buildData.start_time ?? 0) * 1000);

    return Container(
      color: secondaryColor,
      child: Padding(
        padding: const EdgeInsets.only(top: 10, bottom: 10),
        child: Row(
          mainAxisAlignment: MainAxisAlignment.spaceBetween,
          children: [
            Row(
              mainAxisAlignment: MainAxisAlignment.start,
              children: [
                const SizedBox(
                  width: 10,
                ),
                IconButton(
                  icon: const Icon(
                    Icons.arrow_back,
                    size: 28,
                  ),
                  onPressed: () {
                    context.pop();
                  },
                ),
                const SizedBox(
                  width: 10,
                ),
                IconButton(
                  icon: Icon(
                    switchSuccessIcon(buildData.status),
                    color: switchSuccessColor(buildData.status),
                  ),
                  onPressed: () {
                    context.replace("/build/${buildData.id}");
                  },
                ),
                const SizedBox(
                  width: 10,
                ),
                Text(
                  buildData.pkg_name,
                  style: const TextStyle(fontWeight: FontWeight.bold),
                ),
                const SizedBox(
                  width: 10,
                ),
                Text("triggered ${start_time.readableDuration()}")
              ],
            ),
            Row(
              children: [
                IconButton(
                  onPressed: () {
                    setState(() {
                      scrollFollowActive = !scrollFollowActive;
                      Provider.of<BuildLogProvider>(context, listen: false)
                          .followLog = scrollFollowActive;
                    });
                  },
                  isSelected: scrollFollowActive,
                  icon: const Icon(Icons.read_more),
                  selectedIcon: const Icon(Icons.read_more),
                  tooltip: "Follow log",
                ),
                IconButton(
                  onPressed: () {
                    Provider.of<BuildLogProvider>(context, listen: false)
                        .go_to_top();
                  },
                  icon: const Icon(Icons.vertical_align_top_rounded),
                  tooltip: "Go to Top",
                ),
                IconButton(
                  onPressed: () {
                    Provider.of<BuildLogProvider>(context, listen: false)
                        .go_to_bottom();
                  },
                  icon: const Icon(Icons.vertical_align_bottom_rounded),
                  tooltip: "Go to Bottom",
                ),
                const SizedBox(
                  width: 15,
                ),
              ],
            )
          ],
        ),
      ),
    );
  }

  Widget _buildSideBar(Build buildData) {
    return SizedBox(
      width: 300,
      child: Container(
        color: secondaryColor,
        padding: const EdgeInsets.all(defaultPadding),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const SizedBox(
              height: 45,
            ),
            const Divider(),
            const SizedBox(
              height: 5,
            ),
            const Text(
              "Actions:",
              style: TextStyle(fontSize: 18),
              textAlign: TextAlign.start,
            ),
            const SizedBox(
              height: 20,
            ),
            Row(
              children: [
                ElevatedButton(
                  onPressed: () async {
                    final confirmResult = await showConfirmationDialog(
                        context,
                        "Delete Build",
                        "Are you sure to delete this Package?", () {
                      API.deleteBuild(widget.buildID);
                      context.pop();
                    }, null);
                  },
                  child: const Text(
                    "Delete",
                    style: TextStyle(color: Colors.redAccent),
                  ),
                ),
                const SizedBox(
                  width: 10,
                ),
                ElevatedButton(
                  onPressed: () async {
                    final buildid =
                        await API.updatePackage(id: buildData.pkg_id);
                    context.pushReplacement("/build/$buildid");
                  },
                  child: const Text(
                    "Retry",
                    style: TextStyle(color: Colors.orangeAccent),
                  ),
                ),
              ],
            ),
            const SizedBox(
              height: 15,
            ),
            const Divider(),
            const SizedBox(
              height: 5,
            ),
            const Text(
              "Build Information:",
              style: TextStyle(fontSize: 18),
              textAlign: TextAlign.start,
            ),
            const SizedBox(
              height: 20,
            ),
            SideCard(
              title: "Build Number",
              textRight: buildData.id.toString(),
            ),
            SideCard(
              title: "Finished",
              textRight: buildData.end_time.toString(),
            ),
            SideCard(
              title: "Queued",
              textRight: "7",
            ),
            SideCard(
              title: "Duration",
              textRight: (buildData.end_time != null
                      ? DateTime.fromMillisecondsSinceEpoch(
                          buildData.end_time! * 1000)
                      : DateTime.now())
                  .difference(DateTime.fromMillisecondsSinceEpoch(
                      buildData.start_time! * 1000))
                  .readableDuration(),
            ),
          ],
        ),
      ),
    );
  }

  Widget _buildPage(Build build) {
    switch (build.status) {
      case 3:
        return const Text("in Queue");
      case 0:
      case 1:
      case 2:
      default:
        return BuildOutput(build: build);
    }
  }
}
