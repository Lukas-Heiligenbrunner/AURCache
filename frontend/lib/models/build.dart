import 'package:freezed_annotation/freezed_annotation.dart';
part 'build.g.dart';

@JsonSerializable()
class Build {
  final int id;
  final String pkg_name, platform;
  final int pkg_id;
  final String version;
  final int status;
  @JsonKey(fromJson: _fromJson)
  final DateTime? end_time;
  @JsonKey(fromJson: _fromJson)
  final DateTime start_time;

  Build({
    required this.id,
    required this.pkg_id,
    required this.pkg_name,
    required this.platform,
    required this.version,
    required this.start_time,
    required this.end_time,
    required this.status,
  });

  factory Build.fromJson(Map<String, dynamic> json) => _$BuildFromJson(json);
  Map<String, dynamic> toJson() => _$BuildToJson(this);

  static dynamic _fromJson(int? value) =>
      value != null ? DateTime.fromMillisecondsSinceEpoch(value * 1000) : null;
}
