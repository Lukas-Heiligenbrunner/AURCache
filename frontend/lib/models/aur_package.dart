import 'package:freezed_annotation/freezed_annotation.dart';
part 'aur_package.g.dart';

@JsonSerializable()
class AurPackage {
  final String name, version;

  AurPackage({required this.name, required this.version});

  factory AurPackage.fromJson(Map<String, dynamic> json) =>
      _$AurPackageFromJson(json);
  Map<String, dynamic> toJson() => _$AurPackageToJson(this);
}
