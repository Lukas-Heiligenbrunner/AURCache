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
    final resp = await getRawClient().get("/builds/${id}");
    return Build.fromJson(resp.data);
  }

  Future<String> getOutput({int? line, required int buildID}) async {
    String uri = "/builds/output?buildid=$buildID";
    if (line != null) {
      uri += "&startline=$line";
    }
    final resp = await getRawClient().get(uri);
    return resp.data.toString();
  }
}
