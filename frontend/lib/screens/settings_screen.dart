import 'package:aurcache/components/activity_table.dart';
import 'package:aurcache/models/activity.dart';
import 'package:aurcache/providers/activity_log.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:flutter_settings_ui/flutter_settings_ui.dart';

import '../components/api/api_builder.dart';
import '../constants/color_constants.dart';

class SettingsScreen extends StatelessWidget {
  const SettingsScreen({super.key});
  @override
  Widget build(BuildContext context) {
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
      body: SettingsList(
        platform: context.desktop ? DevicePlatform.web : DevicePlatform.android,
        darkTheme: SettingsThemeData(
          settingsListBackground: Colors.transparent,
        ),

        shrinkWrap: false,
        contentPadding: context.desktop
            ? EdgeInsets.only(left: 32, right: 32, bottom: 32)
            : null,
        sections: [
          SettingsSection(
            title: Text('General'),
            tiles: [
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.update),
                title: Text('Version check interval'),
                description: Text(
                  'How often to check for new AUR/Git Versions?',
                ),
              ),
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.schedule),
                title: Text('Auto update schedule'),
                description: Text('When to trigger auto-updates?'),
              ),
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
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.speed),
                title: Text('CPU Limit'),
                description: Text('µCPUs to use for each build'),
              ),
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.memory),
                title: Text('Memory Limit'),
                description: Text(
                  'Maximum memory each build is allowed to use',
                ),
              ),
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.timer),
                title: Text('Job Timeout'),
                description: Text(
                  'Maxiumum amount of time a build is allowed to take',
                ),
              ),
            ],
          ),
          SettingsSection(
            title: Text('Advanced Settings'),
            tiles: [
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.image),
                title: Text('Builder Image'),
                description: Text(
                  'Use a custom builder image (change with care)',
                ),
              ),
            ],
          ),
          SettingsSection(
            title: Text('Authentication'),
            tiles: [
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.link),
                title: Text('Auth URI'),
              ),
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.credit_card_outlined),
                title: Text('Token URI'),
              ),
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.redo),
                title: Text('Redirect URI'),
                description: Text(
                  'Oauth redirect URI back to AURCache (https://yourdomain/api/auth)',
                ),
              ),
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.person),
                title: Text('Userinfo URI'),
              ),
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.badge),
                title: Text('Client ID'),
              ),
              SettingsTile.navigation(
                onPressed: (_) {},
                leading: Icon(Icons.key),
                title: Text('Client Secret'),
                value: Text(
                  "Already set by Environment variable!",
                  style: TextStyle(color: Colors.red),
                ),
                enabled: false,
              ),
            ],
          ),
        ],
      ),
    );
  }
}
