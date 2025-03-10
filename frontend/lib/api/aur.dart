import 'package:aurcache/models/aur_package.dart';

import 'api_client.dart';

extension AURApi on ApiClient {
  Future<List<AurPackage>> getAurPackages(String query) async {
    if (query.length < 3) {
      return [];
    }

    final resp = await getRawClient().get(
      "/search",
      queryParameters: {'query': query},
    );
    final responseObject = resp.data as List;
    final List<AurPackage> packages = responseObject
        .map((e) => AurPackage.fromJson(e))
        .toList(growable: false);
    return packages;
  }
}
