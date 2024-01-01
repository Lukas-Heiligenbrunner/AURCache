import 'package:aurcache/components/build_output.dart';
import 'package:aurcache/models/build.dart';
import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/providers/build_provider.dart';
import 'package:aurcache/utils/time_formatter.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';

import '../components/dashboard/your_packages.dart';

class BuildScreen extends StatefulWidget {
  const BuildScreen({super.key, required this.buildID});

  final int buildID;

  @override
  State<BuildScreen> createState() => _BuildScreenState();
}

class _BuildScreenState extends State<BuildScreen> {
  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: APIBuilder<BuildProvider, Build, BuildDTO>(
          dto: BuildDTO(buildID: widget.buildID),
          interval: const Duration(seconds: 10),
          onLoad: () => const Text("no data"),
          onData: (buildData) {
            final start_time = DateTime.fromMillisecondsSinceEpoch(
                (buildData.start_time ?? 0) * 1000);

            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisAlignment: MainAxisAlignment.start,
              children: [
                Row(
                  mainAxisAlignment: MainAxisAlignment.start,
                  children: [
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
                const SizedBox(
                  height: 15,
                ),
                BuildOutput(build: buildData)
              ],
            );
          }),
      appBar: AppBar(),
    );
  }
}
