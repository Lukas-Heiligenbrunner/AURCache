import 'package:freezed_annotation/freezed_annotation.dart';
part 'simple_packge.g.dart';

@JsonSerializable()
class SimplePackage {
  final int id;
  final String name;
  @JsonKey(fromJson: _fromJson)
  final bool outofdate;
  final int status;
  final String latest_version, upstream_version;

  SimplePackage({
    required this.id,
    required this.name,
    required this.status,
    required this.latest_version,
    required this.upstream_version,
    required this.outofdate,
  });

  factory SimplePackage.fromJson(Map<String, dynamic> json) =>
      _$SimplePackageFromJson(json);
  Map<String, dynamic> toJson() => _$SimplePackageToJson(this);

  factory SimplePackage.dummy() => SimplePackage(
    id: 42,
    name: 'MyPackage',
    status: 0,
    latest_version: '1.0.0',
    upstream_version: '1.0.0',
    outofdate: false,
  );

  static bool _fromJson(num value) => value != 0;
}
