import 'package:aurcache/components/packages_table.dart';
import 'package:aurcache/models/simple_packge.dart';
import 'package:flutter/material.dart';

import '../components/api/api_builder.dart';
import '../constants/color_constants.dart';
import '../providers/packages.dart';

class PackagesScreen extends StatelessWidget {
  const PackagesScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(),
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
                  "All Packages",
                  style: Theme.of(context).textTheme.titleMedium,
                ),
                SizedBox(
                  width: double.infinity,
                  child: APIBuilder(
                      interval: const Duration(seconds: 10),
                      onLoad: () => const Text("no data"),
                      onData: (List<SimplePackage> data) {
                        return PackagesTable(data: data);
                      },
                      provider: listPackagesProvider()),
                )
              ],
            ),
          ),
        ),
      ),
    );
  }
}
