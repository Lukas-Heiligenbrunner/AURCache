import 'package:freezed_annotation/freezed_annotation.dart';

part 'extended_package.freezed.dart';
part 'extended_package.g.dart';

bool _fromJson(num value) => value != 0;

String _toString(PackageSource src) {
  return src.toString();
}

@freezed
sealed class ExtendedPackage with _$ExtendedPackage {
  factory ExtendedPackage({
    required int id,
    required String name,
    required int status,
    // ignore: invalid_annotation_target
    @JsonKey(fromJson: _fromJson) required bool outofdate,
    required String upstream_version,
    final String? latest_version,
    required List<String> selected_platforms,
    required List<String> selected_build_flags,
    // ignore: invalid_annotation_target
    @JsonKey(toJson: _toString) required PackageSource package_source,
  }) = _ExtendedPackage;

  factory ExtendedPackage.fromJson(Map<String, dynamic> json) =>
      _$ExtendedPackageFromJson(json);

  factory ExtendedPackage.dummy() => ExtendedPackage(
    id: 42,
    name: "Dummy",
    status: 0,
    outofdate: true,
    upstream_version: "1.0.0",
    selected_platforms: ["arm64"],
    selected_build_flags: ["-S", "--noconfirm", "--dummyflag"],
    package_source: PackageSource.git(
      GitPackage(
        git_ref: "master",
        git_url: "http://dummyur.org",
        subfolder: "dummyfolder",
      ),
    ),
  );
}

@Freezed(unionKey: 'package_type', unionValueCase: FreezedUnionCase.pascal)
sealed class PackageSource with _$PackageSource {
  const factory PackageSource.aur(AurPackage aur) = Aur;
  const factory PackageSource.git(GitPackage git) = Git;
  const factory PackageSource.upload(UploadPackage upload) = Upload;

  factory PackageSource.fromJson(Map<String, dynamic> json) {
    final type = json['package_type'];
    switch (type) {
      case 'Aur':
        return PackageSource.aur(AurPackage.fromJson(json));
      case 'Git':
        return PackageSource.git(GitPackage.fromJson(json));
      case 'Upload':
        return PackageSource.upload(UploadPackage.fromJson(json));
      default:
        throw UnsupportedError('Unknown package_type: $type');
    }
  }
}

@freezed
sealed class AurPackage with _$AurPackage {
  const factory AurPackage({
    required String name,
    String? project_url,
    String? description,
    required int last_updated,
    required int first_submitted,
    String? licenses,
    String? maintainer,
    required bool aur_flagged_outdated,
    required String aur_url,
  }) = _AurPackage;

  factory AurPackage.fromJson(Map<String, dynamic> json) =>
      _$AurPackageFromJson(json);
}

@freezed
sealed class GitPackage with _$GitPackage {
  const factory GitPackage({
    required String git_url,
    required String git_ref,
    required String subfolder,
  }) = _GitPackage;

  factory GitPackage.fromJson(Map<String, dynamic> json) =>
      _$GitPackageFromJson(json);
}

@freezed
sealed class UploadPackage with _$UploadPackage {
  const factory UploadPackage() = _UploadPackage;

  factory UploadPackage.fromJson(Map<String, dynamic> json) =>
      _$UploadPackageFromJson(json);
}
