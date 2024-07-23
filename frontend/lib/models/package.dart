class Package {
  final int id;
  final String name;
  final bool outofdate;
  final int status;
  final String latest_version, latest_aur_version;

  Package(
      {required this.id,
      required this.name,
      required this.status,
      required this.latest_version,
      required this.latest_aur_version,
      required this.outofdate});

  factory Package.fromJson(Map<String, dynamic> json) {
    return Package(
      id: json["id"] as int,
      outofdate: json["outofdate"] as bool,
      status: json["status"] as int,
      name: json["name"] as String,
      latest_version: json["latest_version"] as String,
      latest_aur_version: json["latest_aur_version"] as String,
    );
  }
}
