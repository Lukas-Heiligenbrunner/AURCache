import 'package:freezed_annotation/freezed_annotation.dart';
part 'extended_package.g.dart';

@JsonSerializable()
class ExtendedPackage {
  final int id;
  final String name;
  @JsonKey(fromJson: _fromJson)
  final bool outofdate;
  final int status, last_updated, first_submitted;
  final String latest_aur_version, aur_url;
  final String? licenses, maintainer, project_url, description, latest_version;
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
    required this.description,
  });

  factory ExtendedPackage.fromJson(Map<String, dynamic> json) =>
      _$ExtendedPackageFromJson(json);
  Map<String, dynamic> toJson() => _$ExtendedPackageToJson(this);

  static bool _fromJson(num value) => value != 0;
}
