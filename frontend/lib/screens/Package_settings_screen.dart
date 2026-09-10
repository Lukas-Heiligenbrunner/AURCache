import 'package:aurcache/api/API.dart';
import 'package:aurcache/api/packages.dart';
import 'package:aurcache/api/settings.dart';
import 'package:aurcache/components/api/api_builder.dart';
import 'package:aurcache/components/build_flag_settings.dart';
import 'package:aurcache/components/platform_settings.dart';
import 'package:aurcache/components/settings_item.dart';
import 'package:aurcache/models/extended_package.dart';
import 'package:aurcache/models/settings.dart';
import 'package:aurcache/providers/packages.dart';
import 'package:aurcache/providers/settings.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_settings_ui/flutter_settings_ui.dart';
import 'package:go_router/go_router.dart';
import 'package:toastification/toastification.dart';

class Packagesettingsscreen extends ConsumerWidget {
  const Packagesettingsscreen({super.key, required this.pkgID});

  final int pkgID;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    return Scaffold(
      appBar: AppBar(
        leading: IconButton(
          icon: const Icon(Icons.arrow_back),
          onPressed: () => context.go('/package/$pkgID'),
        ),
        title: const Text('Package Settings'),
      ),
      body: APIBuilder<ExtendedPackage>(
        onLoad: () => const Center(child: CircularProgressIndicator()),
        onData: (pkg) => APIBuilder<ApplicationSettings>(
          onLoad: () => const Center(child: CircularProgressIndicator()),
          onData: (settings) => _Body(
            pkg: pkg,
            settings: settings,
            onPackageChanged: () => ref.invalidate(getPackageProvider(pkgID)),
            onSettingChanged: () =>
                ref.invalidate(getSettingsProvider(pkgid: pkgID)),
          ),
          provider: getSettingsProvider(pkgid: pkgID),
        ),
        provider: getPackageProvider(pkgID),
      ),
    );
  }
}

class _Body extends StatelessWidget {
  const _Body({
    required this.pkg,
    required this.settings,
    required this.onPackageChanged,
    required this.onSettingChanged,
  });

  final ExtendedPackage pkg;
  final ApplicationSettings settings;
  final VoidCallback onPackageChanged;
  final VoidCallback onSettingChanged;

  Future<bool> _patchPackage({
    List<String>? platforms,
    List<String>? buildFlags,
  }) async {
    final ok = await API.patchPackage(
      id: pkg.id,
      platforms: platforms,
      build_flags: buildFlags,
    );
    _toast(ok, 'Package settings saved!', 'Failed to save package settings!');
    if (ok) onPackageChanged();
    return ok;
  }

  Future<void> _applySetting(String key, SettingsResult result) async {
    final ok = result.action == SettingsAction.reset
        ? await API.resetSetting(key, pkgid: pkg.id)
        : await API.patchSetting(key, result.value ?? "", pkgid: pkg.id);
    _toast(ok, 'Setting saved!', 'Failed to save setting!');
    if (ok) onSettingChanged();
  }

  void _toast(bool ok, String success, String failure) {
    toastification.show(
      title: Text(ok ? success : failure),
      autoCloseDuration: const Duration(seconds: 4),
      type: ok ? ToastificationType.success : ToastificationType.error,
    );
  }

  @override
  Widget build(BuildContext context) {
    return SettingsList(
      platform: context.desktop ? DevicePlatform.web : DevicePlatform.android,
      darkTheme: SettingsThemeData(settingsListBackground: Colors.transparent),
      shrinkWrap: false,
      contentPadding: context.desktop
          ? const EdgeInsets.only(left: 32, right: 32, bottom: 32)
          : null,
      sections: [
        SettingsSection(
          title: const Text('Build Configuration'),
          tiles: [
            CustomSettingsTile(
              child: _SectionContainer(
                title: 'Selected build platforms',
                description:
                    'Builds will be triggered for each selected platform.',
                child: PlatformSettings(
                  initialPlatforms: pkg.selected_platforms,
                  onChanged: (next) => _patchPackage(platforms: next),
                ),
              ),
            ),
            CustomSettingsTile(
              child: _SectionContainer(
                title: 'Build flags',
                description:
                    'Extra flags passed to makepkg when building this package.',
                child: BuildFlagSettings(
                  initialFlags: pkg.selected_build_flags,
                  onChanged: (next) => _patchPackage(buildFlags: next),
                ),
              ),
            ),
          ],
        ),
        SettingsSection(
          title: const Text('Resource Limits'),
          tiles: [
            SettingsItem(
              title: 'CPU Limit',
              description: 'µCPUs to use for this package\'s builds.',
              icon: Icons.speed,
              isPackageScope: true,
              source: settings.cpu_limit.source,
              value: settings.cpu_limit.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return "Required";
                if (int.tryParse(v) == null) return "Must be a number";
                return null;
              },
              onResult: (r) => _applySetting('cpu_limit', r),
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Memory Limit',
              description:
                  'Maximum memory for this package\'s builds (-1 = unlimited).',
              icon: Icons.memory,
              isPackageScope: true,
              source: settings.memory_limit.source,
              value: settings.memory_limit.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [
                FilteringTextInputFormatter.allow(RegExp(r'-?\d*')),
              ],
              validator: (v) {
                if (v == null || v.isEmpty) return "Required";
                if (int.tryParse(v) == null) return "Must be a number";
                return null;
              },
              onResult: (r) => _applySetting('memory_limit', r),
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Job Timeout',
              description:
                  'Maximum build duration for this package (in seconds).',
              icon: Icons.timer,
              isPackageScope: true,
              source: settings.job_timeout.source,
              value: settings.job_timeout.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return "Required";
                final n = int.tryParse(v);
                if (n == null) return "Must be a number";
                if (n < 1) return "Out of range";
                return null;
              },
              onResult: (r) => _applySetting('job_timeout', r),
            ).asCustomSettingstile(),
          ],
        ),
        SettingsSection(
          title: const Text('Build Environment'),
          tiles: [
            SettingsTile.navigation(
              onPressed: (_) => context.go('/package/${pkg.id}/config-files'),
              leading: const Icon(Icons.description),
              title: const Text('Config Files'),
              description: const Text(
                'Override makepkg.conf and pacman.conf for this package',
              ),
              trailing: const Icon(Icons.chevron_right),
            ),
            SettingsItem(
              title: 'Builder Image',
              description: 'Use a custom builder image for this package.',
              icon: Icons.image,
              isPackageScope: true,
              source: settings.builder_image.source,
              value: settings.builder_image.value,
              validator: (v) {
                if (v == null || v.isEmpty) return "Required";
                return null;
              },
              onResult: (r) => _applySetting('builder_image', r),
            ).asCustomSettingstile(),
          ],
        ),
      ],
    );
  }
}

/// Wraps a free-form widget in the same indentation/title style the SettingsTile
/// gives, so platform/build-flag editors fit visually with the rest of the list.
class _SectionContainer extends StatelessWidget {
  const _SectionContainer({
    required this.title,
    required this.description,
    required this.child,
  });

  final String title;
  final String description;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(20, 14, 20, 14),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(title, style: theme.textTheme.titleSmall),
          const SizedBox(height: 4),
          Text(
            description,
            style: theme.textTheme.bodySmall?.copyWith(
              color: theme.textTheme.bodySmall?.color?.withValues(alpha: 0.75),
            ),
          ),
          const SizedBox(height: 12),
          child,
        ],
      ),
    );
  }
}
