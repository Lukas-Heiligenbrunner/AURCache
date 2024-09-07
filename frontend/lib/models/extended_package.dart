class ExtendedPackage {
  final int id;
  final String name;
  final bool outofdate;
  final int status, last_updated, first_submitted;
  final String latest_version, latest_aur_version, aur_url;
  final String? licenses, maintainer, project_url;
  final bool aur_flagged_outdated;
  final List<String> selected_platforms;
  final List<String> selected_build_flags;

  ExtendedPackage({
    required this.id,
    required this.name,
    required this.status,
    required this.latest_version,
    required this.latest_aur_version,
    required this.outofdate,
    required this.last_updated,
    required this.first_submitted,
    required this.licenses,
    required this.maintainer,
    required this.aur_flagged_outdated,
    required this.selected_platforms,
    required this.selected_build_flags,
    required this.aur_url,
    required this.project_url,
  });

  factory ExtendedPackage.fromJson(Map<String, dynamic> json) {
    return ExtendedPackage(
      id: json["id"] as int,
      outofdate: json["outofdate"] as num != 0,
      status: json["status"] as int,
      name: json["name"] as String,
      latest_version: json["latest_version"] as String,
      latest_aur_version: json["latest_aur_version"] as String,
      last_updated: json["last_updated"] as int,
      first_submitted: json["first_submitted"] as int,
      licenses: json["licenses"] as String?,
      maintainer: json["maintainer"] as String?,
      aur_flagged_outdated: json["aur_flagged_outdated"] as bool,
      selected_platforms: (json["selected_platforms"] as List)
          .map((e) => e as String)
          .toList(growable: false),
      selected_build_flags: (json["selected_build_flags"] as List)
          .map((e) => e as String)
          .toList(growable: false),
      aur_url: json['aur_url'] as String,
      project_url: json['project_url'] as String?,
    );
  }
}
