import 'package:aurcache/api/API.dart';
import 'package:aurcache/api/settings.dart';
import 'package:aurcache/components/settings_item.dart';
import 'package:aurcache/providers/settings.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_settings_ui/flutter_settings_ui.dart';

import '../components/api/api_builder.dart';
import '../models/settings.dart';

class SettingsScreen extends ConsumerWidget {
  const SettingsScreen({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    return Scaffold(
      appBar: AppBar(
        title: Row(
          mainAxisAlignment: MainAxisAlignment.spaceBetween,
          children: [
            const Text("Settings"),
            Padding(
              padding: const EdgeInsets.only(right: 32),
              child: Text(
                "Settings saved!",
                style: TextStyle(fontSize: 14, color: Colors.green.shade700),
              ),
            ),
          ],
        ),
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
        onLoad: () =>
            _renderSettingsList(context, ref, ApplicationSettings.dummy()),
        onData: (data) => _renderSettingsList(context, ref, data),
        provider: getSettingsProvider(),
      ),
    );
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
                  'How often to check for new AUR/Git Versions? (in Seconds)',
              icon: Icons.update,
              envOverwritten: settings.version_check_interval.env_forced,
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
              onChanged: (v) async {
                await API.patchSettings(
                  version_check_interval: int.tryParse(v!),
                );
                ref.invalidate(getSettingsProvider);
              },
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Auto update schedule',
              description: 'When to trigger auto-updates? (in Seconds)',
              icon: Icons.schedule,
              isNullable: true,
              envOverwritten: settings.auto_update_interval.env_forced,
              value: settings.auto_update_interval.value?.toString(),
              keyboardType: TextInputType.number,
              inputFormatters: [FilteringTextInputFormatter.digitsOnly],
              validator: (v) {
                if (v == null || v.isEmpty) return "";
                final n = int.tryParse(v);
                if (n == null) return "Must be a number";
                if (n < 1) return "Out of range";
                return null;
              },
              onChanged: (v) async {
                await API.patchSettings(
                  auto_update_interval: v == null ? null : int.tryParse(v),
                );
                ref.invalidate(getSettingsProvider);
              },
            ).asCustomSettingstile(),
          ],
        ),
        SettingsSection(
          title: Text('Builder'),
          tiles: [
            SettingsTile.navigation(
              onPressed: (_) {},
              leading: Icon(Icons.description),
              title: Text('Config Files'),
              trailing: Icon(Icons.chevron_right),
            ),
            SettingsItem(
              title: 'CPU Limit',
              description: 'µCPUs to use for each build',
              icon: Icons.speed,
              envOverwritten: settings.cpu_limit.env_forced,
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
              onChanged: (v) async {
                await API.patchSettings(cpu_limit: v);
                ref.invalidate(getSettingsProvider);
              },
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Memory Limit',
              description: 'Maximum memory each build is allowed to use',
              icon: Icons.memory,
              envOverwritten: settings.memory_limit.env_forced,
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
              onChanged: (v) async {
                await API.patchSettings(memory_limit: v);
                ref.invalidate(getSettingsProvider);
              },
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Job concurrency',
              description: 'Maxiumum build jobs allowed in parallel',
              icon: Icons.device_hub,
              envOverwritten: settings.max_concurrent_builds.env_forced,
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
              onChanged: (v) async {
                await API.patchSettings(
                  max_concurrent_builds: int.tryParse(v!),
                );
                ref.invalidate(getSettingsProvider);
              },
            ).asCustomSettingstile(),
            SettingsItem(
              title: 'Job Timeout',
              description:
                  'Maxiumum amount of time a build is allowed to take (in seconds)',
              icon: Icons.timer,
              envOverwritten: settings.job_timeout.env_forced,
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
              onChanged: (v) async {
                await API.patchSettings(job_timeout: v);
                ref.invalidate(getSettingsProvider);
              },
            ).asCustomSettingstile(),
          ],
        ),
        SettingsSection(
          title: Text('Advanced Settings'),
          tiles: [
            SettingsItem(
              value: settings.builder_image.value.toString(),
              title: 'Builder Image',
              icon: Icons.image,
              envOverwritten: settings.builder_image.env_forced,
              description: 'Use a custom builder image',
              onChanged: (v) async {
                print("stuff");
                print(v);
                await API.patchSettings(builder_image: v);
                ref.invalidate(getSettingsProvider);
              },
            ).asCustomSettingstile(),
          ],
        ),
      ],
    );
  }
}
