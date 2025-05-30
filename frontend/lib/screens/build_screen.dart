import 'package:aurcache/api/builds.dart';
import 'package:aurcache/components/build_output.dart';
import 'package:aurcache/models/build.dart';
import 'package:aurcache/providers/build_log.dart';
import 'package:aurcache/providers/builds.dart';
import 'package:aurcache/providers/packages.dart';
import 'package:aurcache/utils/time_formatter.dart';
import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';
import 'package:toastification/toastification.dart';

import '../api/API.dart';
import '../components/api/api_builder.dart';
import '../components/confirm_popup.dart';
import '../components/dashboard/chart_card.dart';
import '../constants/color_constants.dart';
import '../utils/package_color.dart';

class BuildScreen extends ConsumerStatefulWidget {
  const BuildScreen({super.key, required this.buildID});

  final int buildID;

  @override
  ConsumerState<BuildScreen> createState() => _BuildScreenState();
}

class _BuildScreenState extends ConsumerState<BuildScreen> {
  bool scrollFollowActive = true;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: APIBuilder(
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
        },
        provider: getBuildProvider(widget.buildID),
      ),
    );
  }

  Widget _buildTopBar(Build buildData, BuildContext context) {
    final followLog = ref.watch(buildLogProvider);

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
                Text("triggered ${buildData.start_time.readableDuration()}")
              ],
            ),
            Row(
              children: [
                IconButton(
                  onPressed: () {
                    setState(() {
                      ref.read(buildLogProvider.notifier).follow_log =
                          !followLog;
                    });
                  },
                  isSelected: followLog,
                  icon: const Icon(Icons.read_more),
                  selectedIcon: const Icon(Icons.read_more),
                  tooltip: "Follow log",
                ),
                IconButton(
                  onPressed: () {
                    ref.read(buildLogProvider.notifier).go_to_top();
                  },
                  icon: const Icon(Icons.vertical_align_top_rounded),
                  tooltip: "Go to Top",
                ),
                IconButton(
                  onPressed: () {
                    ref.read(buildLogProvider.notifier).go_to_bottom();
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
              children: buildActions(buildData),
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
              textRight: "#${buildData.id}",
            ),
            SideCard(
              title: "Version",
              textRight: buildData.version,
            ),
            SideCard(
              title: "Finished",
              textRight: buildData.end_time == null
                  ? "Not yet"
                  : buildData.end_time!.readableDuration(),
            ),
            SideCard(
              title: "Duration",
              textRight: (buildData.end_time ?? DateTime.now())
                  .difference(buildData.start_time)
                  .readableDuration(),
            ),
          ],
        ),
      ),
    );
  }

  List<Widget> buildActions(Build build) {
    if (build.status == 0) {
      return [
        ElevatedButton(
          onPressed: () async {
            await showConfirmationDialog(
                context, "Cancel Build", "Are you sure to cancel this Build?",
                () {
              API.cancelBuild(widget.buildID);
              // refresh current build screen
              ref.invalidate(getBuildProvider(widget.buildID));
            }, null);
          },
          child: const Text(
            "Cancel",
            style: TextStyle(color: Colors.redAccent),
          ),
        ),
      ];
    } else {
      return [
        ElevatedButton(
          onPressed: () async {
            await showConfirmationDialog(
                context, "Delete Build", "Are you sure to delete this Build?",
                () async {
              await API.deleteBuild(widget.buildID);

              // invalidate package page provider
              ref.invalidate(getPackageProvider(build.pkg_id));

              if (mounted) {
                context.pop();
              }
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
            try {
              final buildid = await API.retryBuild(id: build.id);
              context.pushReplacement("/build/$buildid");
            } on DioException catch (e) {
              print(e);
              toastification.show(
                title: Text('Failed to retry build!'),
                autoCloseDuration: const Duration(seconds: 5),
                type: ToastificationType.error,
              );
            }
          },
          child: const Text(
            "Retry",
            style: TextStyle(color: Colors.orangeAccent),
          ),
        ),
      ];
    }
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
