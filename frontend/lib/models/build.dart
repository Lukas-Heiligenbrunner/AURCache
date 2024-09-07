class Build {
  final int id;
  final String pkg_name, platform;
  final int pkg_id;
  final String version;
  final int status;
  final DateTime? end_time;
  final DateTime start_time;

  Build(
      {required this.id,
      required this.pkg_id,
      required this.pkg_name,
      required this.platform,
      required this.version,
      required this.start_time,
      required this.end_time,
      required this.status});

  factory Build.fromJson(Map<String, dynamic> json) {
    final startTime =
        DateTime.fromMillisecondsSinceEpoch(json["start_time"] * 1000);
    final endTime = json["end_time"] != null
        ? DateTime.fromMillisecondsSinceEpoch((json["end_time"] as int) * 1000)
        : null;

    return Build(
      id: json["id"] as int,
      pkg_id: json["pkg_id"] as int,
      status: json["status"] as int,
      start_time: startTime,
      end_time: endTime,
      pkg_name: json["pkg_name"] as String,
      version: json["version"] as String,
      platform: json["platform"] as String,
    );
  }
}
