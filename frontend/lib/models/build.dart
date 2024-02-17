class Build {
  final int id;
  final String pkg_name;
  final int pkg_id;
  final String version;
  final int status;
  final int? start_time, end_time;

  Build(
      {required this.id,
      required this.pkg_id,
      required this.pkg_name,
      required this.version,
      required this.start_time,
      required this.end_time,
      required this.status});

  factory Build.fromJson(Map<String, dynamic> json) {
    return Build(
      id: json["id"] as int,
      pkg_id: json["pkg_id"] as int,
      status: json["status"] as int,
      start_time: json["start_time"] as int?,
      end_time: json["end_time"] as int?,
      pkg_name: json["pkg_name"] as String,
      version: json["version"] as String,
    );
  }
}
