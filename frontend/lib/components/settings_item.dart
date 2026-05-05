import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_settings_ui/flutter_settings_ui.dart';

import '../models/settings.dart';

enum SettingsAction { save, reset }

class SettingsResult {
  const SettingsResult.save(this.value) : action = SettingsAction.save;
  const SettingsResult.reset() : action = SettingsAction.reset, value = null;

  final SettingsAction action;
  final String? value;
}

typedef SettingsResultCallback = Future<void> Function(SettingsResult result);

class SettingsItem extends StatelessWidget {
  const SettingsItem({
    super.key,
    required this.title,
    this.description,
    required this.icon,
    required this.source,
    this.isPackageScope = false,
    required this.value,
    this.isNullable = false,
    this.onResult,
    this.validator,
    this.keyboardType = TextInputType.text,
    this.inputFormatters,
  });

  final String title;
  final String? description;
  final IconData icon;
  final SettingSource source;

  /// True when rendered on a per-package settings page. Changes the badge
  /// labels ("inherited" vs "default") and which states show a Reset button.
  final bool isPackageScope;
  final String? value;
  final bool isNullable;
  final SettingsResultCallback? onResult;

  final String? Function(String?)? validator;
  final TextInputType keyboardType;
  final List<TextInputFormatter>? inputFormatters;

  bool get _isEnvLocking => source == SettingSource.env && !isPackageScope;

  /// Whether this scope holds a stored override that "Reset" can clear. On
  /// the global page that means a global row exists; on a package page it
  /// means a package row exists. Inherited / default → no override to reset.
  bool get _hasStoredOverride => isPackageScope
      ? source == SettingSource.package
      : source == SettingSource.global;

  String? get _badgeLabel {
    if (_isEnvLocking) return null; // rendered via the red banner instead
    if (source == SettingSource.defaultSrc) return "(default)";
    if (isPackageScope && source == SettingSource.global) return "(inherited)";
    if (isPackageScope && source == SettingSource.env) {
      return "(inherited from env)";
    }
    return null;
  }

  @override
  Widget build(BuildContext context) {
    return SettingsTile.navigation(
      enabled: !_isEnvLocking,
      leading: Icon(icon),
      title: Text(title),
      description: description != null ? Text(description!) : null,
      value: _buildValue(),
      onPressed: (_) async {
        final result = await _showEditDialog(context);
        if (result != null) {
          await onResult?.call(result);
        }
      },
    );
  }

  Widget _buildValue() {
    if (_isEnvLocking) {
      return Row(
        children: [
          const Text(
            "Set by environment variable!",
            style: TextStyle(color: Colors.red),
          ),
          const SizedBox(width: 12),
          Text(value ?? "Disabled"),
        ],
      );
    }

    final displayValue = value == null || value!.isEmpty ? "Disabled" : value!;
    final badge = _badgeLabel;
    if (badge != null) {
      return Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Flexible(child: Text(displayValue, overflow: TextOverflow.ellipsis)),
          const SizedBox(width: 8),
          Text(
            badge,
            style: const TextStyle(
              color: Colors.grey,
              fontStyle: FontStyle.italic,
            ),
          ),
        ],
      );
    }
    return Text(displayValue, overflow: TextOverflow.ellipsis);
  }

  Future<SettingsResult?> _showEditDialog(BuildContext context) async {
    final controller = TextEditingController(text: value ?? "");
    bool enabled = value != null && value!.isNotEmpty;
    String? errorText;

    return showDialog<SettingsResult>(
      context: context,
      barrierDismissible: false,
      builder: (context) {
        return StatefulBuilder(
          builder: (context, setState) {
            void validate() {
              if (validator != null) {
                errorText = validator!(
                  isNullable && !enabled ? null : controller.text,
                );
              } else {
                errorText = null;
              }
            }

            validate();

            final resetLabel = isPackageScope
                ? "Reset to global"
                : "Reset to default";

            return AlertDialog(
              title: Text(title),
              content: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  if (description != null) ...[
                    Text(description!),
                    const SizedBox(height: 12),
                  ],

                  if (isNullable)
                    SwitchListTile(
                      contentPadding: EdgeInsets.zero,
                      title: const Text("Enabled"),
                      value: enabled,
                      onChanged: _isEnvLocking
                          ? null
                          : (v) {
                              setState(() => enabled = v);
                            },
                    ),

                  TextField(
                    controller: controller,
                    enabled: !_isEnvLocking && (!isNullable || enabled),
                    keyboardType: keyboardType,
                    inputFormatters: inputFormatters,
                    onChanged: (_) => setState(validate),
                    decoration: InputDecoration(
                      labelText: "Value",
                      border: const OutlineInputBorder(),
                      errorText: errorText,
                    ),
                  ),
                ],
              ),
              actions: [
                if (_hasStoredOverride)
                  TextButton(
                    onPressed: () =>
                        Navigator.of(context).pop(const SettingsResult.reset()),
                    child: Text(resetLabel),
                  ),
                TextButton(
                  onPressed: () => Navigator.of(context).pop(),
                  child: const Text("Cancel"),
                ),
                ElevatedButton(
                  onPressed: errorText == null || (!enabled)
                      ? () {
                          final raw = (!enabled || controller.text.isEmpty)
                              ? null
                              : controller.text;
                          Navigator.of(context).pop(SettingsResult.save(raw));
                        }
                      : null,
                  child: const Text("Save"),
                ),
              ],
            );
          },
        );
      },
    );
  }

  CustomSettingsTile asCustomSettingstile() {
    return CustomSettingsTile(child: this);
  }
}
