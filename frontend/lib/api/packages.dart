import '../core/models/package.dart';
import 'api_client.dart';

extension PackagesAPI on ApiClient {
  Future<List<Package>> listPackages() async {
    final resp = await getRawClient().get("/packages/list");
    print(resp.data);

    // todo error handling

    final responseObject = resp.data as List;
    final List<Package> packages =
        responseObject.map((e) => Package.fromJson(e)).toList(growable: false);
    return packages;
  }

  Future<void> addPackage({bool force = false, required String name}) async {
    final resp = await getRawClient().post("/packages/add", data: {'force_build': force, 'name': name});
    print(resp.data);
  }

}
