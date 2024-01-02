import 'package:aurcache/api/packages.dart';
import 'package:aurcache/components/api/APIBuilder.dart';
import 'package:aurcache/providers/builds_provider.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:provider/provider.dart';

import '../api/API.dart';
import '../components/builds_table.dart';
import '../components/confirm_popup.dart';
import '../constants/color_constants.dart';
import '../models/build.dart';
import '../models/package.dart';
import '../providers/package_provider.dart';
import '../providers/packages_provider.dart';
import '../providers/stats_provider.dart';

class PackageScreen extends StatefulWidget {
  const PackageScreen({super.key, required this.pkgID});

  final int pkgID;

  @override
  State<PackageScreen> createState() => _PackageScreenState();
}

class _PackageScreenState extends State<PackageScreen> {
  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(),
      body: APIBuilder<PackageProvider, Package, PackageDTO>(
          dto: PackageDTO(pkgID: widget.pkgID),
          onLoad: () => const Text("loading"),
          onData: (pkg) {
            return Padding(
              padding: const EdgeInsets.all(defaultPadding),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Row(
                    mainAxisAlignment: MainAxisAlignment.spaceBetween,
                    crossAxisAlignment: CrossAxisAlignment.center,
                    children: [
                      Container(
                        margin: const EdgeInsets.only(left: 15),
                        child: Text(
                          pkg.name,
                          style: const TextStyle(fontSize: 32),
                        ),
                      ),
                      Container(
                        margin: const EdgeInsets.only(right: 15),
                        child: ElevatedButton(
                          onPressed: () async {
                            final confirmResult =
                                await showDeleteConfirmationDialog(context);
                            if (!confirmResult) return;

                            final succ = await API.deletePackage(pkg.id);
                            if (succ) {
                              context.pop();

                              Provider.of<PackagesProvider>(context,
                                      listen: false)
                                  .refresh(context);
                              Provider.of<BuildsProvider>(context,
                                      listen: false)
                                  .refresh(context);
                              Provider.of<StatsProvider>(context, listen: false)
                                  .refresh(context);
                            }
                          },
                          child: const Text(
                            "Delete",
                            style: TextStyle(color: Colors.redAccent),
                          ),
                        ),
                      )
                    ],
                  ),
                  const SizedBox(
                    height: 25,
                  ),
                  Container(
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
                            "Builds of ${pkg.name}",
                            style: Theme.of(context).textTheme.subtitle1,
                          ),
                          SizedBox(
                            width: double.infinity,
                            child: APIBuilder<BuildsProvider, List<Build>,
                                BuildsDTO>(
                              key: const Key("Builds on Package info"),
                              dto: BuildsDTO(pkgID: pkg.id),
                              interval: const Duration(seconds: 5),
                              onData: (data) {
                                return BuildsTable(data: data);
                              },
                              onLoad: () => const Text("no data"),
                            ),
                          ),
                        ],
                      ),
                    ),
                  )
                ],
              ),
            );
          }),
    );
  }
}
