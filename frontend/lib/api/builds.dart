import '../models/build.dart';
import 'api_client.dart';

extension BuildsAPI on ApiClient {
  Future<List<Build>> listAllBuilds({int? pkgID, int? limit}) async {
    String uri = "/builds?";
    if (pkgID != null) {
      uri += "pkgid=$pkgID";
    }

    if (limit != null) {
      uri += "limit=$limit";
    }

    final resp = await getRawClient().get(uri);

    final responseObject = resp.data as List;
    final List<Build> packages =
        responseObject.map((e) => Build.fromJson(e)).toList(growable: false);
    return packages;
  }

  Future<Build> getBuild(int id) async {
    final resp = await getRawClient().get("/build/$id");
    return Build.fromJson(resp.data);
  }

  Future<bool> deleteBuild(int id) async {
    final resp = await getRawClient().delete("/build/$id");
    return resp.statusCode == 400;
  }

  Future<bool> cancelBuild(int id) async {
    final resp = await getRawClient().post("/build/$id/cancel");
    return resp.statusCode == 400;
  }

  Future<String> getOutput({int? line, required int buildID}) async {
    String uri = "/build/$buildID/output";
    if (line != null) {
      uri += "?startline=$line";
    }
    final resp = await getRawClient().get(uri);
    return resp.data.toString();
  }

  Future<int> retryBuild({required int id}) async {
    final resp = await getRawClient().post("/build/$id/retry");
    return resp.data as int;
  }
}
