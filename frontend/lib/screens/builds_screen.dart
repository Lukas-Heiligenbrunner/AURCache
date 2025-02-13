import 'package:aurcache/api/builds.dart';
import 'package:aurcache/components/builds_table.dart';
import 'package:aurcache/components/table_info.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import '../api/API.dart';
import '../components/api/api_builder.dart';
import '../constants/color_constants.dart';

class BuildsScreen extends StatelessWidget {
  const BuildsScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text("All Builds"),
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
