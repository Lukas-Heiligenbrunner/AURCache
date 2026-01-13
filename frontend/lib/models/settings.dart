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
    T Function(dynamic? json) fromJsonT,
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

  // todo proper dummy values
  factory ApplicationSettings.dummy() => ApplicationSettings(
    cpu_limit: SettingsEntry(value: 500, env_forced: false, defaultt: false),
    memory_limit: SettingsEntry(value: 500, env_forced: false, defaultt: false),
    max_concurrent_builds: SettingsEntry(
      value: 500,
      env_forced: false,
      defaultt: false,
    ),
    version_check_interval: SettingsEntry(
      value: 500,
      env_forced: false,
      defaultt: false,
    ),
    auto_update_interval: SettingsEntry(
      value: "",
      env_forced: false,
      defaultt: false,
    ),
    job_timeout: SettingsEntry(value: 500, env_forced: false, defaultt: false),
    builder_image: SettingsEntry(
      value: "500",
      env_forced: false,
      defaultt: false,
    ),
  );

  factory ApplicationSettings.fromJson(Map<String, dynamic> json) =>
      _$ApplicationSettingsFromJson(json);
  Map<String, dynamic> toJson() => _$ApplicationSettingsToJson(this);
}

@JsonSerializable(genericArgumentFactories: true)
class PatchField<T> {
  final bool isPresent;
  final T? value;

  const PatchField(this.isPresent, this.value);

  const PatchField.absent() : this(false, null);

  const PatchField.present(T? value) : this(true, value);

  factory PatchField.fromJson(
    Object? json,
    T Function(Object? json) fromJsonT,
  ) {
    return PatchField.present(json == null ? null : fromJsonT(json));
  }

  Object? toJson(Object? Function(T value) toJsonT) {
    if (!isPresent) return null;
    return value == null ? null : toJsonT(value!);
  }
}

class PatchFieldConverter<T> implements JsonConverter<PatchField<T>, Object?> {
  const PatchFieldConverter();

  @override
  PatchField<T> fromJson(Object? json) {
    return PatchField.present(json as T?);
  }

  @override
  Object? toJson(PatchField<T> field) => field.value;
}

@JsonSerializable()
class ApplicationSettingsPatch {
  @PatchFieldConverter<int>()
  final PatchField<int> cpu_limit;

  @PatchFieldConverter<int>()
  final PatchField<int> memory_limit;

  @PatchFieldConverter<int>()
  final PatchField<int> max_concurrent_builds;

  @PatchFieldConverter<int>()
  final PatchField<int> version_check_interval;

  @PatchFieldConverter<int>()
  final PatchField<int> auto_update_interval;

  @PatchFieldConverter<int>()
  final PatchField<int> job_timeout;

  @PatchFieldConverter<String>()
  final PatchField<String> builder_image;

  const ApplicationSettingsPatch({
    this.cpu_limit = const PatchField.absent(),
    this.memory_limit = const PatchField.absent(),
    this.max_concurrent_builds = const PatchField.absent(),
    this.version_check_interval = const PatchField.absent(),
    this.auto_update_interval = const PatchField.absent(),
    this.job_timeout = const PatchField.absent(),
    this.builder_image = const PatchField.absent(),
  });

  factory ApplicationSettingsPatch.fromJson(Map<String, dynamic> json) =>
      _$ApplicationSettingsPatchFromJson(json);

  Map<String, dynamic> toJson() => _$ApplicationSettingsPatchToJson(this);
}
