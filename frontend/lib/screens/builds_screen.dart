import 'package:aurcache/api/builds.dart';
import 'package:aurcache/components/builds_table.dart';
import 'package:aurcache/components/table_info.dart';
import 'package:flutter/material.dart';
import '../api/API.dart';
import '../components/api/ApiBuilder.dart';
import '../constants/color_constants.dart';
import '../models/build.dart';

class BuildsScreen extends StatelessWidget {
  const BuildsScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text("All Builds"),
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
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  "All Builds",
                  style: Theme.of(context).textTheme.titleMedium,
                ),
                SizedBox(
                  width: double.infinity,
                  child: APIBuilder(
                      interval: const Duration(seconds: 10),
                      onLoad: () => const Text("no data"),
                      onData: (data) {
                        if (data.isEmpty) {
                          return const TableInfo(
                              title: "You have no builds yet");
                        } else {
                          return BuildsTable(data: data);
                        }
                      },
                      api: API.listAllBuilds),
                )
              ],
            ),
          ),
        ),
      ),
    );
  }
}
