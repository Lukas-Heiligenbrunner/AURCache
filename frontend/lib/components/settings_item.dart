import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_settings_ui/flutter_settings_ui.dart';

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
    required this.envOverwritten,
    required this.isDefault,
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
  final bool envOverwritten;
  final bool isDefault;
  final String? value;
  final bool isNullable;
  final SettingsResultCallback? onResult;

  final String? Function(String?)? validator;
  final TextInputType keyboardType;
  final List<TextInputFormatter>? inputFormatters;

  @override
  Widget build(BuildContext context) {
    return SettingsTile.navigation(
      enabled: !envOverwritten,
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
    if (envOverwritten) {
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
    if (isDefault) {
      return Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Text(displayValue),
          const SizedBox(width: 8),
          const Text(
            "(default)",
            style: TextStyle(color: Colors.grey, fontStyle: FontStyle.italic),
          ),
        ],
      );
    }
    return Text(displayValue);
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
                      onChanged: envOverwritten
                          ? null
                          : (v) {
                              setState(() => enabled = v);
                            },
                    ),

                  TextField(
                    controller: controller,
                    enabled: !envOverwritten && (!isNullable || enabled),
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
                if (!isDefault)
                  TextButton(
                    onPressed: () =>
                        Navigator.of(context).pop(const SettingsResult.reset()),
                    child: const Text("Reset to default"),
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
