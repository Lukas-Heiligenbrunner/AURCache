import 'package:aurcache/models/activity.dart';

import 'api_client.dart';

extension ActivityAPI on ApiClient {
  Future<List<Activity>> listActivities({int? pkgID, int? limit}) async {
    String uri = "/activity?";

    if (limit != null) {
      uri += "limit=$limit";
    }

    final resp = await getRawClient().get(uri);

    final responseObject = resp.data as List;
    final List<Activity> activities =
        responseObject.map((e) => Activity.fromJson(e)).toList(growable: false);
    return activities;
  }
}
