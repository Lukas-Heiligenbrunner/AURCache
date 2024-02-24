class Stats {
  final int total_builds,
      failed_builds,
      avg_queue_wait_time,
      repo_storage_size,
      enqueued_builds,
      total_packages;
  final Duration avg_build_time;

  factory Stats.fromJson(Map<String, dynamic> json) {
    return Stats(
      total_builds: json["total_builds"] as int,
      failed_builds: json["failed_builds"] as int,
      avg_queue_wait_time: json["avg_queue_wait_time"] as int,
      avg_build_time: Duration(seconds: json["avg_build_time"]),
      repo_storage_size: json["repo_storage_size"] as int,
      enqueued_builds: json["enqueued_builds"] as int,
      total_packages: json["total_packages"] as int,
    );
  }

  Stats(
      {required this.total_builds,
      required this.failed_builds,
      required this.avg_queue_wait_time,
      required this.avg_build_time,
      required this.repo_storage_size,
      required this.enqueued_builds,
      required this.total_packages});
}
