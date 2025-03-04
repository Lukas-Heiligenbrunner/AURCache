class Stats {
  final int total_builds,
      successful_builds,
      failed_builds,
      repo_size,
      total_packages;
  final Duration avg_build_time;
  final double total_build_trend, avg_build_time_trend;

  factory Stats.fromJson(Map<String, dynamic> json) {
    return Stats(
      total_builds: json["total_builds"] as int,
      successful_builds: json["successful_builds"] as int,
      failed_builds: json["failed_builds"] as int,
      avg_build_time: Duration(seconds: json["avg_build_time"]),
      repo_size: json["repo_size"] as int,
      total_packages: json["total_packages"] as int,
      total_build_trend: json["total_build_trend"] as double,
      avg_build_time_trend: json["avg_build_time_trend"] as double,
    );
  }

  Stats(
      {required this.total_builds,
      required this.successful_builds,
      required this.failed_builds,
      required this.avg_build_time,
      required this.repo_size,
      required this.total_packages,
      required this.total_build_trend,
      required this.avg_build_time_trend});
}
