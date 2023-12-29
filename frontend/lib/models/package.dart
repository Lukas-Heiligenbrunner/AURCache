class Package {
  final int id;
  final String name;
  final int count;
  final int status;

  Package(
      {required this.id,
      required this.name,
      required this.count,
      required this.status});

  factory Package.fromJson(Map<String, dynamic> json) {
    return Package(
      id: json["id"] as int,
      count: json["count"] as int,
      status: json["status"] as int,
      name: json["name"] as String,
    );
  }
}
