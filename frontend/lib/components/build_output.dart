import 'dart:async';

import 'package:aurcache/api/builds.dart';
import 'package:flutter/material.dart';

import '../api/API.dart';
import '../models/build.dart';

class BuildOutput extends StatefulWidget {
  const BuildOutput({super.key, required this.build});

  final Build build;

  @override
  State<BuildOutput> createState() => _BuildOutputState();
}

class _BuildOutputState extends State<BuildOutput> {
  late Future<String> initialOutput;

  String output = "";
  Timer? outputTimer;
  final scrollController = ScrollController();

  @override
  Widget build(BuildContext context) {
    return Expanded(
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
      _scrollToBottom();
    });

    // poll new output only if not finished
    if (widget.build.status == 0) {
      outputTimer = Timer.periodic(const Duration(seconds: 3), (Timer t) async {
        print("refreshing output");
        final value = await API.getOutput(
            buildID: widget.build.id, line: output.split("\n").length);
        setState(() {
          output += value;
        });

        _scrollToBottom();
      });
    }
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
  }
}
