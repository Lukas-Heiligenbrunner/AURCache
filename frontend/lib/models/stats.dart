import 'package:freezed_annotation/freezed_annotation.dart';
part 'stats.g.dart';

@JsonSerializable()
class Stats {
  final int total_builds,
      successful_builds,
      failed_builds,
      repo_size,
      total_packages;
  @JsonKey(fromJson: _fromJson)
  final Duration avg_build_time;
  final double total_build_trend, avg_build_time_trend;

  Stats(
      {required this.total_builds,
      required this.successful_builds,
      required this.failed_builds,
      required this.avg_build_time,
      required this.repo_size,
      required this.total_packages,
      required this.total_build_trend,
      required this.avg_build_time_trend});

  factory Stats.fromJson(Map<String, dynamic> json) => _$StatsFromJson(json);
  Map<String, dynamic> toJson() => _$StatsToJson(this);

  static Duration _fromJson(int value) => Duration(seconds: value);
}
