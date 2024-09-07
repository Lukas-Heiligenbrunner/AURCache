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

  Future<void> addPackage(
      {required String name, required List<String> selectedArchs}) async {
    final resp = await getRawClient()
        .post("/package", data: {'name': name, 'platforms': selectedArchs});
    print(resp.data);
  }

  Future<int> updatePackage({bool force = false, required int id}) async {
    final resp = await getRawClient()
        .post("/package/$id/update", data: {'force': force});
    print(resp.data);

    return resp.data as int;
  }

  Future<bool> deletePackage(int id) async {
    final resp = await getRawClient().delete("/package/$id");
    return resp.statusCode == 200;
  }
}
