import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_settings_ui/flutter_settings_ui.dart';

class SettingsItem extends StatelessWidget {
  const SettingsItem({
    super.key,
    required this.title,
    this.description,
    required this.icon,
    required this.envOverwritten,
    required this.value,
    this.isNullable = false,
    this.onChanged,
    this.validator,
    this.keyboardType = TextInputType.text,
    this.inputFormatters,
  });

  final String title;
  final String? description;
  final IconData icon;
  final bool envOverwritten;
  final String? value;
  final bool isNullable;
  final ValueChanged<String?>? onChanged;

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
        onChanged?.call(result);
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
    return Text(value ?? "Disabled");
  }

  Future<String?> _showEditDialog(BuildContext context) async {
    final controller = TextEditingController(text: value ?? "");
    bool enabled = value != null;
    String? errorText;

    return showDialog<String?>(
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
                TextButton(
                  onPressed: () => Navigator.of(context).pop(null),
                  child: const Text("Cancel"),
                ),
                ElevatedButton(
                  onPressed: errorText == null || (!enabled)
                      ? (() {
                          Navigator.of(context).pop(
                            (!enabled || controller.text == "")
                                ? null
                                : controller.text,
                          );
                        })
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
