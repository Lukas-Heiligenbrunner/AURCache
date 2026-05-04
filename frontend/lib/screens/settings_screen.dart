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

  Future<void> _apply(
    WidgetRef ref,
    String key,
    SettingsResult result, {
    String? Function(String?)? toStored,
  }) async {
    bool ok;
    if (result.action == SettingsAction.reset) {
      ok = await API.resetSetting(key);
    } else {
      final stored = toStored != null ? toStored(result.value) : result.value;
      ok = await API.patchSetting(key, stored ?? "");
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
          title: Text('General'),
          tiles: [
            SettingsItem(
              title: 'Version check interval',
              description:
                  'How often to check for new AUR/Git versions? (in seconds)',
              icon: Icons.update,
              envOverwritten: settings.version_check_interval.env_forced,
              isDefault: settings.version_check_interval.defaultt,
              value: settings.version_check_interval.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return null;
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
              envOverwritten: settings.auto_update_interval.env_forced,
              isDefault: settings.auto_update_interval.defaultt,
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
          title: Text('Builder'),
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
              description: 'µCPUs to use for each build',
              icon: Icons.speed,
              envOverwritten: settings.cpu_limit.env_forced,
              isDefault: settings.cpu_limit.defaultt,
              value: settings.cpu_limit.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return null;
                final n = int.tryParse(v);
                if (n == null) return "Must be a number";
                if (n < 1) return "Out of range";
                return null;
              },
              onResult: (r) => _apply(ref, 'cpu_limit', r),
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Memory Limit',
              description: 'Maximum memory each build is allowed to use',
              icon: Icons.memory,
              envOverwritten: settings.memory_limit.env_forced,
              isDefault: settings.memory_limit.defaultt,
              value: settings.memory_limit.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return null;
                final n = int.tryParse(v);
                if (n == null) return "Must be a number";
                if (n < 1) return "Out of range";
                return null;
              },
              onResult: (r) => _apply(ref, 'memory_limit', r),
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Job concurrency',
              description: 'Maximum build jobs allowed in parallel',
              icon: Icons.device_hub,
              envOverwritten: settings.max_concurrent_builds.env_forced,
              isDefault: settings.max_concurrent_builds.defaultt,
              value: settings.max_concurrent_builds.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return null;
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
              envOverwritten: settings.job_timeout.env_forced,
              isDefault: settings.job_timeout.defaultt,
              value: settings.job_timeout.value.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return null;
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
          title: Text('Advanced Settings'),
          tiles: [
            SettingsItem(
              value: settings.builder_image.value,
              title: 'Builder Image',
              icon: Icons.image,
              envOverwritten: settings.builder_image.env_forced,
              isDefault: settings.builder_image.defaultt,
              description: 'Use a custom builder image',
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
        title: Text('Settings saved!'),
        autoCloseDuration: const Duration(seconds: 5),
        type: ToastificationType.success,
      );
    } else {
      toastification.show(
        title: Text('Failed to save settings!'),
        autoCloseDuration: const Duration(seconds: 5),
        type: ToastificationType.error,
      );
    }
  }
}
