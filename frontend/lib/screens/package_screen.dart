import 'package:aurcache/api/packages.dart';
import 'package:aurcache/models/extended_package.dart';
import 'package:aurcache/providers/builds.dart';
import 'package:aurcache/providers/packages.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';
import 'package:go_router/go_router.dart';
import 'package:url_launcher/url_launcher.dart';

import '../api/API.dart';
import '../components/api/api_builder.dart';
import '../components/builds_table.dart';
import '../components/confirm_popup.dart';
import '../constants/color_constants.dart';
import '../providers/activity_log.dart';
import '../providers/statistics.dart';

class PackageScreen extends ConsumerStatefulWidget {
  const PackageScreen({super.key, required this.pkgID});

  final int pkgID;

  @override
  ConsumerState<PackageScreen> createState() => _PackageScreenState();
}

class _PackageScreenState extends ConsumerState<PackageScreen> {
  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(),
      body: APIBuilder(
        interval: Duration(minutes: 1),
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
                        const SizedBox(width: 12),
                        Container(
                          padding: EdgeInsets.symmetric(horizontal: 8, vertical: 4),
                          decoration: BoxDecoration(
                            color: pkg.isCustom ? Color(0xFF2E7D32) : Color(0xFF1976D2),
                            borderRadius: BorderRadius.circular(12),
                          ),
                          child: Text(
                            pkg.packageTypeLabel,
                            style: TextStyle(
                              color: Colors.white,
                              fontSize: 12,
                              fontWeight: FontWeight.w500,
                            ),
                          ),
                        ),
                        if (!pkg.isCustom)
                          IconButton(
                            onPressed: () async {
                              await launchUrl(
                                Uri.parse(pkg.aur_url),
                                webOnlyWindowName: '_blank',
                              );
                            },
                            icon: const Icon(Icons.link),
                          ),
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
        provider: getPackageProvider(widget.pkgID),
      ),
    );
  }

  Widget _buildTopActionButtons(ExtendedPackage pkg) {
    return Row(
      children: [
        if (!pkg.isCustom)
          ElevatedButton(
            onPressed: () async {
              await showConfirmationDialog(
                context,
                "Force update Package",
                "Are you sure to force an Package rebuild?",
                () async {
                  await API.updatePackage(force: true, id: pkg.id);
                  // invalidate all dashboard providers
                  ref.invalidate(listActivitiesProvider);
                  ref.invalidate(listPackagesProvider);
                  ref.invalidate(listBuildsProvider);
                  ref.invalidate(listStatsProvider);
                  ref.invalidate(getGraphDataProvider);
                },
                () {},
              );
            },
            child: const Text(
              "Force Update",
              style: TextStyle(color: Colors.yellowAccent),
            ),
          ),
        if (pkg.isCustom)
          ElevatedButton(
            onPressed: () {
              _showCustomUpdateDialog(pkg);
            },
            child: const Text(
              "Update PKGBUILD",
              style: TextStyle(color: Colors.blueAccent),
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
                  // invalidate all dashboard providers
                  ref.invalidate(listActivitiesProvider);
                  ref.invalidate(listPackagesProvider);
                  ref.invalidate(listBuildsProvider);
                  ref.invalidate(listStatsProvider);
                  ref.invalidate(getGraphDataProvider);

                  if (mounted) {
                    context.pop();
                  }
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
    final lastUpdated = pkg.last_updated > 0 
        ? DateTime.fromMillisecondsSinceEpoch(pkg.last_updated * 1000)
        : null;
    final firstSubmitted = pkg.first_submitted > 0
        ? DateTime.fromMillisecondsSinceEpoch(pkg.first_submitted * 1000)
        : null;

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
              title: "Package Type",
              subtitle: pkg.packageTypeLabel,
            ),
            if (!pkg.isCustom) ...[
              _sideCard(
                title: "Latest AUR version",
                subtitle: pkg.latest_aur_version,
              ),
              if (lastUpdated != null)
                _sideCard(
                  title: "Last Updated",
                  subtitle:
                      "${lastUpdated.year}-${lastUpdated.month.toString().padLeft(2, '0')}-${lastUpdated.day.toString().padLeft(2, '0')}",
                ),
              if (firstSubmitted != null)
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
            ] else ...[
              _sideCard(
                title: "Current version",
                subtitle: pkg.latest_version ?? "-",
              ),
              _sideCard(
                title: "Type",
                subtitle: "Custom PKGBUILD",
              ),
            ],
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
              interval: const Duration(seconds: 30),
              onData: (data) {
                return BuildsTable(data: data);
              },
              onLoad: () => const Text("no data"),
              provider: listBuildsProvider(pkgID: pkg.id),
            ),
          ],
        ),
      )
    ]);
  }

  void _showCustomUpdateDialog(ExtendedPackage pkg) {
    final _versionController = TextEditingController();
    final _pkgbuildController = TextEditingController();
    bool _isLoading = false;

    showDialog(
      context: context,
      builder: (BuildContext context) {
        return StatefulBuilder(
          builder: (context, setState) {
            return AlertDialog(
              title: Text('Update ${pkg.name}'),
              content: SizedBox(
                width: 600,
                child: Column(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    TextField(
                      controller: _versionController,
                      decoration: const InputDecoration(
                        labelText: "New Version",
                        hintText: "e.g., 1.0.1",
                        border: OutlineInputBorder(),
                      ),
                    ),
                    const SizedBox(height: 16),
                    TextField(
                      controller: _pkgbuildController,
                      decoration: const InputDecoration(
                        labelText: "Updated PKGBUILD Content",
                        hintText: "Paste your updated PKGBUILD content here...",
                        border: OutlineInputBorder(),
                      ),
                      maxLines: 10,
                      style: TextStyle(fontFamily: 'monospace'),
                    ),
                  ],
                ),
              ),
              actions: [
                TextButton(
                  onPressed: _isLoading ? null : () => Navigator.of(context).pop(),
                  child: const Text('Cancel'),
                ),
                ElevatedButton(
                  onPressed: _isLoading
                      ? null
                      : () async {
                          if (_versionController.text.isEmpty ||
                              _pkgbuildController.text.isEmpty) {
                            return;
                          }

                          setState(() {
                            _isLoading = true;
                          });

                          try {
                            await API.updateCustomPackage(
                              id: pkg.id,
                              version: _versionController.text.trim(),
                              pkgbuildContent: _pkgbuildController.text,
                            );

                            // invalidate all dashboard providers
                            ref.invalidate(listActivitiesProvider);
                            ref.invalidate(listPackagesProvider);
                            ref.invalidate(listBuildsProvider);
                            ref.invalidate(listStatsProvider);
                            ref.invalidate(getGraphDataProvider);

                            Navigator.of(context).pop();
                          } catch (e) {
                            // Handle error
                          } finally {
                            setState(() {
                              _isLoading = false;
                            });
                          }
                        },
                  child: _isLoading
                      ? const SizedBox(
                          height: 20,
                          width: 20,
                          child: CircularProgressIndicator(strokeWidth: 2),
                        )
                      : const Text('Update'),
                ),
              ],
            );
          },
        );
      },
    );
  }
}
