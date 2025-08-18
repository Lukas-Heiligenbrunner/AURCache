import 'package:aurcache/models/extended_package.dart';

import '../models/simple_packge.dart';
import 'api_client.dart';

extension PackagesAPI on ApiClient {
  Future<List<SimplePackage>> listPackages({int? limit}) async {
    final resp = await getRawClient()
        .get("/packages/list", queryParameters: {'limit': limit});

    final responseObject = resp.data as List;
    final List<SimplePackage> packages = responseObject
        .map((e) => SimplePackage.fromJson(e))
        .toList(growable: false);
    return packages;
  }

  Future<ExtendedPackage> getPackage(int id) async {
    final resp = await getRawClient().get("/package/$id");

    final package = ExtendedPackage.fromJson(resp.data);
    return package;
  }

  Future<bool> patchPackage(
      {required int id,
      String? name,
      bool? outofdate,
      int? status,
      String? version,
      latest_aur_version,
      latest_build,
      List<String>? platforms,
      List<String>? build_flags}) async {
    final resp = await getRawClient().patch("/package/$id", data: {
      "name": name,
      "status": status,
      "out_of_date": outofdate,
      "version": version,
      "latest_aur_version": latest_aur_version,
      "latest_build": latest_build,
      "build_flags": build_flags,
      "platforms": platforms
    });
    return resp.statusCode == 200;
  }

  Future<void> addPackage(
      {required String name, required List<String> selectedArchs}) async {
    final resp = await getRawClient()
        .post("/package", data: {'name': name, 'platforms': selectedArchs});
    print(resp.data);
  }

  Future<void> addCustomPackage({
    required String name,
    required String version,
    required String pkgbuildContent,
    required List<String> selectedArchs,
    List<String>? buildFlags,
  }) async {
    final resp = await getRawClient().post("/package/custom", data: {
      'name': name,
      'version': version,
      'pkgbuild_content': pkgbuildContent,
      'platforms': selectedArchs,
      'build_flags': buildFlags,
    });
    print(resp.data);
  }

  Future<List<int>> updatePackage({bool force = false, required int id}) async {
    final resp = await getRawClient()
        .post("/package/$id/update", data: {'force': force});
    print(resp.data);
    final List<int> ids =
        (resp.data as List).map((e) => e as int).toList(growable: false);
    return ids;
  }

  Future<List<int>> updateCustomPackage({
    required int id,
    required String version,
    required String pkgbuildContent,
  }) async {
    final resp = await getRawClient().post("/package/$id/update-custom", data: {
      'version': version,
      'pkgbuild_content': pkgbuildContent,
    });
    print(resp.data);
    final List<int> ids =
        (resp.data as List).map((e) => e as int).toList(growable: false);
    return ids;
  }

  Future<bool> deletePackage(int id) async {
    final resp = await getRawClient().delete("/package/$id");
    return resp.statusCode == 200;
  }
}
