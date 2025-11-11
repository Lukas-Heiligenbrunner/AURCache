import 'package:aurcache/models/extended_package.dart';

import '../models/simple_packge.dart';
import 'api_client.dart';

extension PackagesAPI on ApiClient {
  Future<List<SimplePackage>> listPackages({int? limit}) async {
    final resp = await getRawClient().get(
      "/packages/list",
      queryParameters: {'limit': limit},
    );

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

  Future<bool> patchPackage({
    required int id,
    String? name,
    bool? outofdate,
    int? status,
    String? version,
    latest_aur_version,
    latest_build,
    List<String>? platforms,
    List<String>? build_flags,
  }) async {
    final resp = await getRawClient().patch(
      "/package/$id",
      data: {
        "name": name,
        "status": status,
        "out_of_date": outofdate,
        "version": version,
        "latest_aur_version": latest_aur_version,
        "latest_build": latest_build,
        "build_flags": build_flags,
        "platforms": platforms,
      },
    );
    return resp.statusCode == 200;
  }

  Future<void> addAurPackage({
    required List<String> selectedArchs,
    required String name,
  }) async {
    final resp = await getRawClient().post(
      "/package",
      data: {
        'platforms': selectedArchs,
        'source': {'name': name, 'type': 'aur'},
      },
    );
    print(resp.data);
  }

  Future<void> addGitPackage({
    required List<String> selectedArchs,
    required String gitUrl,
    required String gitRef,
    required String subFolder,
  }) async {
    final resp = await getRawClient().post(
      "/package",
      data: {
        'platforms': selectedArchs,
        'source': {
          'ref': gitRef,
          'url': gitUrl,
          'subfolder': subFolder,
          'type': 'git',
        },
      },
    );
    print(resp.data);
  }

  Future<List<int>> updatePackage({bool force = false, required int id}) async {
    final resp = await getRawClient().post(
      "/package/$id/update",
      data: {'force': force},
    );
    print(resp.data);
    final List<int> ids = (resp.data as List)
        .map((e) => e as int)
        .toList(growable: false);
    return ids;
  }

  Future<bool> deletePackage(int id) async {
    final resp = await getRawClient().delete("/package/$id");
    return resp.statusCode == 200;
  }
}
