import 'dart:async';

import 'package:aurcache/api/builds.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:provider/provider.dart';

import '../api/API.dart';
import '../models/build.dart';
import '../providers/build_log.dart';

class BuildOutput extends ConsumerStatefulWidget {
  const BuildOutput({super.key, required this.build});

  final Build build;

  @override
  ConsumerState<BuildOutput> createState() => _BuildOutputState();
}

class _BuildOutputState extends ConsumerState<BuildOutput> {
  late Future<String> initialOutput;

  String output = "";
  Timer? outputTimer;

  @override
  Widget build(BuildContext context) {
    final sc = ref.read(buildLogProvider.notifier).scrollController;

    return Expanded(
      flex: 1,
      child: SingleChildScrollView(
        controller: sc,
        scrollDirection: Axis.vertical, //.horizontal
        child: Padding(
          padding: const EdgeInsets.only(left: 30, right: 15),
          child: SelectionArea(
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
    );
  }

  @override
  void initState() {
    super.initState();
    initOutputLoader();
  }

  void initOutputLoader() {
    initialOutput = API.getOutput(buildID: widget.build.id);
    initialOutput.then((value) {
      setState(() {
        output = value;
      });
      ref.read(buildLogProvider.notifier).go_to_bottom();
    });

    // poll new output only if not finished
    if (widget.build.status == 0) {
      outputTimer = Timer.periodic(const Duration(seconds: 3), (Timer t) async {
        print("refreshing output");
        final value = await API.getOutput(
            buildID: widget.build.id, line: output.split("\n").length);
        if (value.isNotEmpty) {
          setState(() {
            output += "\n$value";
          });
        }

        if (ref.read(buildLogProvider).value!) {
          ref.read(buildLogProvider.notifier).go_to_bottom();
        }
      });
    }
  }

  @override
  void dispose() {
    super.dispose();
    outputTimer?.cancel();
  }
}
