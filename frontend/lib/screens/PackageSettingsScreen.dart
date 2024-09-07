import 'package:flutter/material.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';
import 'package:provider/provider.dart';

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

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text("Package Settings"),
        actions: [
          Padding(
            padding: const EdgeInsets.only(right: 10),
            child: TextButton(onPressed: () {}, child: const Text("Save")),
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
              return Padding(
                padding: const EdgeInsets.all(15.0),
                child: Row(
                  children: [
                    Expanded(
                      flex: 1,
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          const SizedBox(
                            height: 5,
                          ),
                          const Text(
                            "Selected build platforms:",
                            style: TextStyle(fontSize: 18),
                            textAlign: TextAlign.start,
                          ),
                          const SizedBox(
                            height: 20,
                          ),
                          Tags(
                            itemBuilder: (idx) => ItemTags(
                              index: idx,
                              title: Platforms[idx],
                              active: pkg.selected_platforms
                                  .contains(Platforms[idx]),
                              activeColor: Colors.green,
                              onPressed: (i) {
                                if (i.active!) {
                                  pkg.selected_platforms.add(i.title!);
                                } else {
                                  pkg.selected_platforms.remove(i.title!);
                                }
                              },
                            ),
                            itemCount: Platforms.length,
                          ),
                          const SizedBox(
                            height: 15,
                          ),
                        ],
                      ),
                    ),
                    Expanded(
                      flex: 1,
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          const SizedBox(
                            height: 5,
                          ),
                          const Text(
                            "Build flags:",
                            style: TextStyle(fontSize: 18),
                            textAlign: TextAlign.start,
                          ),
                          const SizedBox(
                            height: 20,
                          ),
                          Tags(
                            itemBuilder: (idx) => ItemTags(
                              index: idx,
                              title: pkg.selected_build_flags[idx],
                              active: true,
                              activeColor: Colors.white38,
                              pressEnabled: false,
                              removeButton: ItemTagsRemoveButton(
                                onRemoved: () {
                                  return true;
                                },
                              ),
                            ),
                            itemCount: pkg.selected_build_flags.length,
                          ),
                          SizedBox(
                            height: 15,
                          ),
                          Text("Add new build flags:"),
                          SizedBox(
                            width: 200,
                            child: TextField(
                              decoration: InputDecoration(
                                  label: Text("--noconfirm"),
                                  suffixIcon: IconButton(
                                      onPressed: () {}, icon: Icon(Icons.add))),
                            ),
                          )
                        ],
                      ),
                    )
                  ],
                ),
              );
            }),
      ),
    );
  }
}
