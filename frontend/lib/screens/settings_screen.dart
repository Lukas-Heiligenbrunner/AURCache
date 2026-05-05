import 'package:aurcache/api/API.dart';
import 'package:aurcache/api/settings.dart';
import 'package:aurcache/components/settings_item.dart';
import 'package:aurcache/providers/settings.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_settings_ui/flutter_settings_ui.dart';
import 'package:go_router/go_router.dart';
import 'package:toastification/toastification.dart';

import '../components/api/api_builder.dart';
import '../models/settings.dart';

class SettingsScreen extends ConsumerWidget {
  const SettingsScreen({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    return Scaffold(
      appBar: AppBar(
        title: const Text("Settings"),
        leading: context.mobile
            ? IconButton(
                icon: const Icon(Icons.menu),
                onPressed: () {
                  Scaffold.of(context).openDrawer();
                },
              )
            : null,
        actions: [],
      ),
      body: APIBuilder(
        onLoad: () => const Center(child: CircularProgressIndicator()),
        onData: (data) => _renderSettingsList(context, ref, data),
        provider: getSettingsProvider(),
      ),
    );
  }

  Future<void> _apply(WidgetRef ref, String key, SettingsResult result) async {
    bool ok;
    if (result.action == SettingsAction.reset) {
      ok = await API.resetSetting(key);
    } else {
      ok = await API.patchSetting(key, result.value ?? "");
    }
    _showToast(ok);
    ref.invalidate(getSettingsProvider);
  }

  Widget _renderSettingsList(
    BuildContext context,
    WidgetRef ref,
    ApplicationSettings settings,
  ) {
    return SettingsList(
      platform: context.desktop ? DevicePlatform.web : DevicePlatform.android,
      darkTheme: SettingsThemeData(settingsListBackground: Colors.transparent),

      shrinkWrap: false,
      contentPadding: context.desktop
          ? EdgeInsets.only(left: 32, right: 32, bottom: 32)
          : null,
      sections: [
        SettingsSection(
          title: const Text('General'),
          tiles: [
            SettingsItem(
              title: 'Version check interval',
              description:
                  'How often to check for new AUR/Git versions? (in seconds)',
              icon: Icons.update,
              source: settings.version_check_interval.source,
              value: settings.version_check_interval.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return "Required";
                final n = int.tryParse(v);
                if (n == null) return "Must be a number";
                if (n < 10) return "Mind rate limits";
                return null;
              },
              onResult: (r) => _apply(ref, 'version_check_interval', r),
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Auto update schedule',
              description:
                  'Cron expression (with seconds) to trigger auto-updates. Disable to turn off.',
              icon: Icons.schedule,
              isNullable: true,
              source: settings.auto_update_interval.source,
              value: settings.auto_update_interval.value,
              validator: (v) {
                if (v == null) return null;
                if (v.trim().isEmpty) return "Cron expression required";
                return null;
              },
              onResult: (r) => _apply(ref, 'auto_update_interval', r),
            ).asCustomSettingstile(),
          ],
        ),
        SettingsSection(
          title: const Text('Builder'),
          tiles: [
            SettingsTile.navigation(
              onPressed: (_) => context.go('/config-files'),
              leading: const Icon(Icons.description),
              title: const Text('Config Files'),
              description: const Text(
                'Edit makepkg.conf and pacman.conf used by builds',
              ),
              trailing: const Icon(Icons.chevron_right),
            ),
            SettingsItem(
              title: 'CPU Limit',
              description: 'µCPUs to use for each build (0 = unlimited)',
              icon: Icons.speed,
              source: settings.cpu_limit.source,
              value: settings.cpu_limit.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return "Required";
                final n = int.tryParse(v);
                if (n == null) return "Must be a number";
                return null;
              },
              onResult: (r) => _apply(ref, 'cpu_limit', r),
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Memory Limit',
              description:
                  'Maximum memory each build is allowed to use (-1 = unlimited)',
              icon: Icons.memory,
              source: settings.memory_limit.source,
              value: settings.memory_limit.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [
                FilteringTextInputFormatter.allow(RegExp(r'-?\d*')),
              ],
              validator: (v) {
                if (v == null || v.isEmpty) return "Required";
                final n = int.tryParse(v);
                if (n == null) return "Must be a number";
                return null;
              },
              onResult: (r) => _apply(ref, 'memory_limit', r),
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Job concurrency',
              description: 'Maximum build jobs allowed in parallel',
              icon: Icons.device_hub,
              source: settings.max_concurrent_builds.source,
              value: settings.max_concurrent_builds.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return "Required";
                final n = int.tryParse(v);
                if (n == null) return "Must be a number";
                if (n < 1 || n > 2048) return "Out of range";
                return null;
              },
              onResult: (r) => _apply(ref, 'max_concurrent_builds', r),
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Job Timeout',
              description:
                  'Maximum amount of time a build is allowed to take (in seconds)',
              icon: Icons.timer,
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
              onResult: (r) => _apply(ref, 'job_timeout', r),
            ).asCustomSettingstile(),
          ],
        ),
        SettingsSection(
          title: const Text('Advanced Settings'),
          tiles: [
            SettingsItem(
              value: settings.builder_image.value,
              title: 'Builder Image',
              icon: Icons.image,
              source: settings.builder_image.source,
              description: 'Use a custom builder image',
              validator: (v) {
                if (v == null || v.isEmpty) return "Required";
                return null;
              },
              onResult: (r) => _apply(ref, 'builder_image', r),
            ).asCustomSettingstile(),
          ],
        ),
      ],
    );
  }

  void _showToast(bool success) {
    if (success) {
      toastification.show(
        title: const Text('Settings saved!'),
        autoCloseDuration: const Duration(seconds: 5),
        type: ToastificationType.success,
      );
    } else {
      toastification.show(
        title: const Text('Failed to save settings!'),
        autoCloseDuration: const Duration(seconds: 5),
        type: ToastificationType.error,
      );
    }
  }
}
