import 'package:freezed_annotation/freezed_annotation.dart';
part 'simple_packge.g.dart';

@JsonSerializable()
class SimplePackage {
  final int id;
  final String name;
  @JsonKey(fromJson: _fromJson)
  final bool outofdate;
  final int status;
  final String latest_version, latest_aur_version;
  final int package_type;

  SimplePackage(
      {required this.id,
      required this.name,
      required this.status,
      required this.latest_version,
      required this.latest_aur_version,
      required this.outofdate,
      required this.package_type});

  factory SimplePackage.fromJson(Map<String, dynamic> json) =>
      _$SimplePackageFromJson(json);
  Map<String, dynamic> toJson() => _$SimplePackageToJson(this);

  static bool _fromJson(num value) => value != 0;
  
  String get packageTypeLabel => package_type == 0 ? 'AUR' : 'Custom';
  bool get isCustom => package_type == 1;
}
