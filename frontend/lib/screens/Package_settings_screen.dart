import 'package:aurcache/api/packages.dart';
import 'package:aurcache/components/build_flag_settings.dart';
import 'package:aurcache/components/platform_settings.dart';
import 'package:aurcache/providers/packages.dart';
import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';
import 'package:toastification/toastification.dart';

import '../api/API.dart';
import '../components/api/api_builder.dart';

class Packagesettingsscreen extends ConsumerStatefulWidget {
  const Packagesettingsscreen({super.key, required this.pkgID});

  final int pkgID;

  @override
  ConsumerState<Packagesettingsscreen> createState() =>
      _PackagesettingsscreenState();
}

class _PackagesettingsscreenState extends ConsumerState<Packagesettingsscreen> {
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
                  try {
                    await API.patchPackage(
                        id: widget.pkgID,
                        build_flags: buildFlags,
                        platforms: buildPlatforms);
                    // refresh provider (also of package page)
                    ref.invalidate(getPackageProvider(widget.pkgID));

                    if (mounted) {
                      context.pop();
                    }
                  } on DioException catch (e) {
                    print(e);
                    toastification.show(
                      title: Text('Failed to save package settings!'),
                      autoCloseDuration: const Duration(seconds: 5),
                      type: ToastificationType.error,
                    );
                  }
                },
                child: const Text("Save")),
          )
        ],
      ),
      body: APIBuilder(
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
        },
        provider: getPackageProvider(widget.pkgID),
      ),
    );
  }
}
