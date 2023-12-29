import '../models/build.dart';
import 'api_client.dart';

extension BuildsAPI on ApiClient {
  Future<List<Build>> listAllBuilds() async {
    final resp = await getRawClient().get("/builds");

    final responseObject = resp.data as List;
    final List<Build> packages =
        responseObject.map((e) => Build.fromJson(e)).toList(growable: false);
    return packages;
  }
}
