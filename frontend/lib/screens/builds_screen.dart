import 'package:aurcache/components/builds_table.dart';
import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/providers/api/builds_provider.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';
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
      body: MultiProvider(
        providers: [
          ChangeNotifierProvider<BuildsProvider>(
              create: (_) => BuildsProvider()),
        ],
        child: Padding(
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
                    style: Theme.of(context).textTheme.subtitle1,
                  ),
                  SizedBox(
                    width: double.infinity,
                    child: APIBuilder<BuildsProvider, List<Build>, Object>(
                        key: const Key("Builds on seperate screen"),
                        interval: const Duration(seconds: 10),
                        onLoad: () => const Text("no data"),
                        onData: (data) {
                          return BuildsTable(data: data);
                        }),
                  )
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}
