import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:riverpod_annotation/riverpod_annotation.dart';

import '../api/API.dart';
import '../models/activity.dart';

part 'activity_log.g.dart';

@riverpod
Future<List<Activity>> listActivities(Ref ref, {int? pkgID, int? limit}) async {
  String uri = "/activity?";

  if (limit != null) {
    uri += "limit=$limit";
  }

  final resp = await API.getRawClient().get(uri);

  final responseObject = resp.data as List;
  final List<Activity> activities = responseObject
      .map((e) => Activity.fromJson(e))
      .toList(growable: false);
  return activities;
}
