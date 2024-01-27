class AurPackage {
  final String name, version;

  AurPackage({required this.name, required this.version});

  factory AurPackage.fromJson(Map<String, dynamic> json) {
    return AurPackage(
      name: json["name"] as String,
      version: json["version"] as String,
    );
  }
}
