import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:riverpod_annotation/riverpod_annotation.dart';

import '../api/API.dart';
import '../models/settings.dart';

part 'settings.g.dart';

@riverpod
Future<ApplicationSettings> getSettings(Ref ref, {int? pkgid}) async {
  final resp = await API.getRawClient().get(
    "/settings",
    queryParameters: {'pkgid': pkgid},
  );

  return ApplicationSettings.fromJson(resp.data);
}
