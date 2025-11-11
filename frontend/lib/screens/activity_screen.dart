import 'package:aurcache/components/activity_table.dart';
import 'package:aurcache/models/activity.dart';
import 'package:aurcache/providers/activity_log.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';

import '../components/api/api_builder.dart';
import '../constants/color_constants.dart';

class ActivityScreen extends StatelessWidget {
  const ActivityScreen({super.key});
  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text("Activity Log"),
        leading: context.mobile
            ? IconButton(
                icon: const Icon(Icons.menu),
                onPressed: () {
                  Scaffold.of(context).openDrawer();
                },
              )
            : null,
      ),
      body: Padding(
        padding: const EdgeInsets.all(defaultPadding),
        child: Container(
          padding: const EdgeInsets.all(defaultPadding),
          decoration: const BoxDecoration(
            color: secondaryColor,
            borderRadius: BorderRadius.all(Radius.circular(10)),
          ),
          child: SingleChildScrollView(
            child: SizedBox(
              width: double.infinity,
              child: APIBuilder(
                onLoad: () => Center(
                  child: Column(
                    children: [
                      const SizedBox(height: 15),
                      const Text("loading"),
                    ],
                  ),
                ),
                onData: (List<Activity> data) => ActivityTable(data: data),
                provider: listActivitiesProvider(),
              ),
            ),
          ),
        ),
      ),
    );
  }
}
