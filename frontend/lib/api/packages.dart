import '../models/package.dart';
import 'api_client.dart';

extension PackagesAPI on ApiClient {
  Future<List<Package>> listPackages({int? limit}) async {
    final resp = await getRawClient()
        .get("/packages/list", queryParameters: {'limit': limit});

    final responseObject = resp.data as List;
    final List<Package> packages =
        responseObject.map((e) => Package.fromJson(e)).toList(growable: false);
    return packages;
  }

  Future<Package> getPackage(int id) async {
    final resp = await getRawClient().get("/package/$id");

    final package = Package.fromJson(resp.data);
    return package;
  }

  Future<void> addPackage({required String name}) async {
    final resp = await getRawClient().post("/package", data: {'name': name});
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
