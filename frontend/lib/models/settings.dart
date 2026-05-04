import 'package:freezed_annotation/freezed_annotation.dart';
part 'settings.g.dart';

@JsonSerializable(genericArgumentFactories: true)
class SettingsEntry<T> {
  final T value;
  final bool env_forced;
  @JsonKey(name: 'default')
  final bool defaultt;

  SettingsEntry({
    required this.value,
    required this.env_forced,
    required this.defaultt,
  });

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

@JsonSerializable()
class SingleSetting {
  final String value;
  final bool env_forced;
  @JsonKey(name: 'default')
  final bool defaultt;

  SingleSetting({
    required this.value,
    required this.env_forced,
    required this.defaultt,
  });

  factory SingleSetting.fromJson(Map<String, dynamic> json) =>
      _$SingleSettingFromJson(json);
  Map<String, dynamic> toJson() => _$SingleSettingToJson(this);
}
