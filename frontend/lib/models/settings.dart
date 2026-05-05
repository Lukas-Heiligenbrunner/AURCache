import 'package:freezed_annotation/freezed_annotation.dart';
part 'settings.g.dart';

/// Where a setting's resolved value came from in the lookup hierarchy
/// (matches the backend `SettingSource` enum, JSON-encoded snake_case).
enum SettingSource {
  @JsonValue('env')
  env,

  /// Stored on the per-package settings row.
  @JsonValue('package')
  package,

  /// Stored on the global settings row.
  @JsonValue('global')
  global,

  /// No row stored — using the static built-in default.
  @JsonValue('default')
  defaultSrc,
}

@JsonSerializable(genericArgumentFactories: true)
class SettingsEntry<T> {
  final T value;
  final SettingSource source;

  SettingsEntry({required this.value, required this.source});

  bool get envForced => source == SettingSource.env;
  bool get isDefault => source == SettingSource.defaultSrc;
  bool get isInherited => source == SettingSource.global;
  bool get isPackageOverride => source == SettingSource.package;

  factory SettingsEntry.fromJson(
    Map<String, dynamic> json,
    T Function(dynamic json) fromJsonT,
  ) => _$SettingsEntryFromJson(json, fromJsonT);
  Map<String, dynamic> toJson(dynamic Function(T value) toJsonT) =>
      _$SettingsEntryToJson(this, toJsonT);
}

@JsonSerializable()
class ApplicationSettings {
  final SettingsEntry<int> cpu_limit;
  final SettingsEntry<int> memory_limit;
  final SettingsEntry<int> max_concurrent_builds;
  final SettingsEntry<int> version_check_interval;
  final SettingsEntry<String?> auto_update_interval;
  final SettingsEntry<int> job_timeout;
  final SettingsEntry<String> builder_image;

  ApplicationSettings({
    required this.cpu_limit,
    required this.memory_limit,
    required this.max_concurrent_builds,
    required this.version_check_interval,
    required this.auto_update_interval,
    required this.job_timeout,
    required this.builder_image,
  });

  factory ApplicationSettings.fromJson(Map<String, dynamic> json) =>
      _$ApplicationSettingsFromJson(json);
  Map<String, dynamic> toJson() => _$ApplicationSettingsToJson(this);
}

/// Single-setting fetch (per-key endpoint). Used for large blobs like
/// makepkg.conf / pacman.conf that don't sit in the dashboard payload.
@JsonSerializable()
class SingleSetting {
  final String value;
  final SettingSource source;

  SingleSetting({required this.value, required this.source});

  bool get envForced => source == SettingSource.env;
  bool get isDefault => source == SettingSource.defaultSrc;
  bool get isInherited => source == SettingSource.global;
  bool get isPackageOverride => source == SettingSource.package;

  factory SingleSetting.fromJson(Map<String, dynamic> json) =>
      _$SingleSettingFromJson(json);
  Map<String, dynamic> toJson() => _$SingleSettingToJson(this);
}
