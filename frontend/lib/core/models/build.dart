class Build {
  final int id;
  final String pkg_name;
  final String version;
  final int status;

  Build(
      {required this.id,required this.pkg_name, required this.version,
        required this.status});

  factory Build.fromJson(Map<String, dynamic> json) {
    return Build(
      id: json["id"] as int,
      status: json["status"] as int,
      pkg_name: json["pkg_name"] as String,
      version: json["version"] as String,
    );
  }
}