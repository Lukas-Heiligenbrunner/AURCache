import 'package:aurcache/api/packages.dart';
import 'package:aurcache/components/build_flag_settings.dart';
import 'package:aurcache/components/platform_settings.dart';
import 'package:flutter/material.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';
import 'package:provider/provider.dart';

import '../api/API.dart';
import '../components/api/APIBuilder.dart';
import '../constants/platforms.dart';
import '../models/extended_package.dart';
import '../providers/api/package_provider.dart';

class Packagesettingsscreen extends StatefulWidget {
  const Packagesettingsscreen({super.key, required this.pkgID});

  final int pkgID;

  @override
  State<Packagesettingsscreen> createState() => _PackagesettingsscreenState();
}

class _PackagesettingsscreenState extends State<Packagesettingsscreen> {
  List<String> buildFlags = [];
  List<String> buildPlatforms = [];

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text("Package Settings"),
        actions: [
          Padding(
            padding: const EdgeInsets.only(right: 10),
            child: TextButton(
                onPressed: () async {
                  await API.patchPackage(
                      id: widget.pkgID,
                      build_flags: buildFlags,
                      platforms: buildPlatforms);
                  Navigator.pop(context);
                },
                child: const Text("Save")),
          )
        ],
      ),
      body: MultiProvider(
        providers: [
          ChangeNotifierProvider<PackageProvider>(
              create: (_) => PackageProvider()),
        ],
        child: APIBuilder<PackageProvider, ExtendedPackage, PackageDTO>(
            dto: PackageDTO(pkgID: widget.pkgID),
            onLoad: () => const Text("loading"),
            onData: (pkg) {
              buildFlags = pkg.selected_build_flags;
              buildPlatforms = pkg.selected_platforms;

              return Padding(
                padding: const EdgeInsets.all(15.0),
                child: Row(
                  children: [
                    Expanded(
                        flex: 1,
                        child: PlatformSettings(
                          pkg: pkg,
                          changed: (List<String> v) {
                            buildPlatforms = v;
                          },
                        )),
                    Expanded(
                        flex: 1,
                        child: BuildFlagSettings(
                          pkg: pkg,
                          changed: (List<String> v) {
                            buildFlags = v;
                          },
                        ))
                  ],
                ),
              );
            }),
      ),
    );
  }
}
