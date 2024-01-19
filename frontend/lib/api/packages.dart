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

  Future<void> addPackage({bool force = false, required String name}) async {
    final resp = await getRawClient()
        .post("/packages/add", data: {'force_build': force, 'name': name});
    print(resp.data);
  }

  Future<bool> deletePackage(int id) async {
    final resp = await getRawClient().post("/package/delete/$id");
    return resp.statusCode == 200;
  }
}
