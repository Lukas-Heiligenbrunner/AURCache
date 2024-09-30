import 'package:aurcache/components/packages_table.dart';
import 'package:aurcache/providers/api/packages_provider.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../components/api/APIBuilder.dart';
import '../constants/color_constants.dart';
import '../models/simple_packge.dart';

class PackagesScreen extends StatelessWidget {
  const PackagesScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(),
      body: MultiProvider(
        providers: [
          ChangeNotifierProvider<PackagesProvider>(
              create: (_) => PackagesProvider()),
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
                    "All Packages",
                    style: Theme.of(context).textTheme.titleMedium,
                  ),
                  SizedBox(
                    width: double.infinity,
                    child: APIBuilder<PackagesProvider, List<SimplePackage>,
                            Object>(
                        key: const Key("Builds on seperate screen"),
                        interval: const Duration(seconds: 10),
                        onLoad: () => const Text("no data"),
                        onData: (data) {
                          return PackagesTable(data: data);
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
