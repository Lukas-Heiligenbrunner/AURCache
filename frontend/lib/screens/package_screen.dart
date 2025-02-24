import 'package:aurcache/api/builds.dart';
import 'package:aurcache/api/packages.dart';
import 'package:aurcache/models/extended_package.dart';
import 'package:flutter/material.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';
import 'package:go_router/go_router.dart';
import 'package:url_launcher/url_launcher.dart';

import '../api/API.dart';
import '../components/api/api_builder.dart';
import '../components/builds_table.dart';
import '../components/confirm_popup.dart';
import '../constants/color_constants.dart';

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
      body: APIBuilder(
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
                    Row(
                      children: [
                        Container(
                          margin: const EdgeInsets.only(left: 15),
                          child: Text(
                            pkg.name,
                            style: const TextStyle(fontSize: 32),
                          ),
                        ),
                        IconButton(
                            onPressed: () async {
                              await launchUrl(
                                Uri.parse(pkg.aur_url),
                                webOnlyWindowName: '_blank',
                              );
                            },
                            icon: const Icon(Icons.link))
                      ],
                    ),
                    _buildTopActionButtons(pkg)
                  ],
                ),
                Row(
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        mainAxisAlignment: MainAxisAlignment.start,
                        children: [_buildMainBody(pkg)],
                      ),
                    ),
                    _buildSideBar(pkg),
                  ],
                ),
              ],
            ),
          );
        },
        api: () => API.getPackage(widget.pkgID),
      ),
    );
  }

  Widget _buildTopActionButtons(ExtendedPackage pkg) {
    return Row(
      children: [
        ElevatedButton(
          onPressed: () async {
            await showConfirmationDialog(
              context,
              "Force update Package",
              "Are you sure to force an Package rebuild?",
              () async {
                await API.updatePackage(force: true, id: pkg.id);
                if (mounted) {
                  // Provider.of<BuildsProvider>(context, listen: false)
                  //   .refresh(context);
                }
              },
              () {},
            );
          },
          child: const Text(
            "Force Update",
            style: TextStyle(color: Colors.yellowAccent),
          ),
        ),
        const SizedBox(
          width: 10,
        ),
        ElevatedButton(
          onPressed: () async {
            await showConfirmationDialog(
              context,
              "Delete Package",
              "Are you sure to delete this Package?",
              () async {
                final succ = await API.deletePackage(pkg.id);
                if (succ) {
                  context.pop();

                  /*Provider.of<PackagesProvider>(context, listen: false)
                      .refresh(context);
                  Provider.of<BuildsProvider>(context, listen: false)
                      .refresh(context);
                  Provider.of<StatsProvider>(context, listen: false)
                      .refresh(context);*/
                }
              },
              () {},
            );
          },
          child: const Text(
            "Delete",
            style: TextStyle(color: Colors.redAccent),
          ),
        ),
        const SizedBox(
          width: 10,
        ),
        ElevatedButton(
          onPressed: () {
            context.push("/package/${pkg.id}/settings");
          },
          child: const Text(
            "Settings",
            style: TextStyle(color: Colors.blueAccent),
          ),
        ),
      ],
    );
  }

  Widget _buildSideBar(ExtendedPackage pkg) {
    final lastUpdated =
        DateTime.fromMillisecondsSinceEpoch(pkg.last_updated * 1000);
    final firstSubmitted =
        DateTime.fromMillisecondsSinceEpoch(pkg.first_submitted * 1000);

    return SizedBox(
      width: 300,
      child: Container(
        color: secondaryColor,
        padding: const EdgeInsets.all(defaultPadding),
        margin: const EdgeInsets.all(10),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const SizedBox(
              height: 5,
            ),
            Text(
              "Details for ${pkg.name}:",
              style: const TextStyle(fontSize: 18),
              textAlign: TextAlign.start,
            ),
            _sideCard(
              title: "Latest AUR version",
              subtitle: pkg.latest_aur_version,
            ),
            _sideCard(
              title: "Last Updated",
              subtitle:
                  "${lastUpdated.year}-${lastUpdated.month.toString().padLeft(2, '0')}-${lastUpdated.day.toString().padLeft(2, '0')}",
            ),
            _sideCard(
              title: "First submitted",
              subtitle:
                  "${firstSubmitted.year}-${firstSubmitted.month.toString().padLeft(2, '0')}-${firstSubmitted.day.toString().padLeft(2, '0')}",
            ),
            _sideCard(
              title: "Licenses",
              subtitle: pkg.licenses ?? "-",
            ),
            _sideCard(
              title: "Maintainer",
              subtitle: pkg.maintainer ?? "-",
            ),
            _sideCard(
              title: "Flagged outdated",
              subtitle: pkg.aur_flagged_outdated ? "yes" : "no",
            ),
            const Divider(),
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
                title: pkg.selected_platforms[idx],
                active: true,
                activeColor: Colors.green,
                pressEnabled: false,
              ),
              itemCount: pkg.selected_platforms.length,
            ),
            const SizedBox(
              height: 15,
            ),
            const Divider(),
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
              ),
              itemCount: pkg.selected_build_flags.length,
            ),
          ],
        ),
      ),
    );
  }

  Widget _sideCard({required String title, required String subtitle}) {
    return Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
      const SizedBox(
        height: 5,
      ),
      Text(title, style: TextStyle(fontSize: 13, fontWeight: FontWeight.bold)),
      const SizedBox(
        height: 3,
      ),
      Text(subtitle),
      const SizedBox(
        height: 10,
      ),
    ]);
  }

  Widget _buildMainBody(ExtendedPackage pkg) {
    return Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
      if (pkg.description != null) ...[
        const SizedBox(
          height: 25,
        ),
        Padding(
          padding: const EdgeInsets.all(5.0),
          child: Text(pkg.description!),
        ),
        const SizedBox(
          height: 25,
        )
      ],
      Container(
        padding: const EdgeInsets.all(defaultPadding),
        decoration: const BoxDecoration(
          color: secondaryColor,
          borderRadius: BorderRadius.all(Radius.circular(10)),
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            Text(
              "Builds of ${pkg.name}",
              style: Theme.of(context).textTheme.titleMedium,
            ),
            APIBuilder(
              interval: const Duration(seconds: 5),
              onData: (data) {
                return BuildsTable(data: data);
              },
              onLoad: () => const Text("no data"),
              api: () => API.listAllBuilds(pkgID: pkg.id),
            ),
          ],
        ),
      )
    ]);
  }
}
