import 'dart:async';

import 'package:aurcache/api/builds.dart';
import 'package:aurcache/models/build.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';

import '../api/API.dart';
import '../components/dashboard/your_packages.dart';

class BuildScreen extends StatefulWidget {
  const BuildScreen({super.key, required this.buildID});

  final int buildID;

  @override
  State<BuildScreen> createState() => _BuildScreenState();
}

class _BuildScreenState extends State<BuildScreen> {
  late Future<Build> buildData;
  late Future<String> initialOutput;

  String output = "";
  Timer? outputTimer, buildDataTimer;
  final scrollController = ScrollController();

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: FutureBuilder(
          future: buildData,
          builder: (context, snapshot) {
            if (snapshot.hasData) {
              final buildData = snapshot.data!;

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
                      const Text("triggered 2 months ago")
                    ],
                  ),
                  const SizedBox(
                    height: 15,
                  ),
                  Expanded(
                    flex: 1,
                    child: SingleChildScrollView(
                      controller: scrollController,
                      scrollDirection: Axis.vertical, //.horizontal
                      child: Padding(
                        padding: const EdgeInsets.only(left: 30, right: 15),
                        child: Text(
                          output,
                          style: const TextStyle(
                            fontSize: 16.0,
                            color: Colors.white,
                          ),
                        ),
                      ),
                    ),
                  ),
                ],
              );
            } else {
              return const Text("loading build");
            }
          }),
      appBar: AppBar(),
    );
  }

  @override
  void initState() {
    super.initState();

    initBuildDataLoader();
    initOutputLoader();
  }

  void initBuildDataLoader() {
    buildData = API.getBuild(widget.buildID);
    buildDataTimer = Timer.periodic(const Duration(seconds: 10), (t) {
      setState(() {
        buildData = API.getBuild(widget.buildID);
      });
    });
  }

  void initOutputLoader() {
    initialOutput = API.getOutput(buildID: widget.buildID);
    initialOutput.then((value) {
      setState(() {
        output = value;
      });
      _scrollToBottom();
    });

    buildData.then((value) {
      // poll new output only if not finished
      if (value.status == 0) {
        outputTimer =
            Timer.periodic(const Duration(seconds: 3), (Timer t) async {
          print("refreshing output");
          final value = await API.getOutput(
              buildID: widget.buildID, line: output.split("\n").length);
          setState(() {
            output += value;
          });

          _scrollToBottom();
        });
      }
    });
  }

  void _scrollToBottom() {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      // scroll to bottom
      final scrollPosition = scrollController.position;
      if (scrollPosition.viewportDimension < scrollPosition.maxScrollExtent) {
        scrollController.animateTo(
          scrollPosition.maxScrollExtent,
          duration: const Duration(milliseconds: 200),
          curve: Curves.easeOut,
        );
      }
    });
  }

  @override
  void dispose() {
    super.dispose();
    outputTimer?.cancel();
    buildDataTimer?.cancel();
  }
}
