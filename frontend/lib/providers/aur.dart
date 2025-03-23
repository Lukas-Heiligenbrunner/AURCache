import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:riverpod_annotation/riverpod_annotation.dart';

import '../api/API.dart';
import '../models/aur_package.dart';

part 'aur.g.dart';

@riverpod
Future<List<AurPackage>> getAurPackages(Ref ref, String query) async {
  if (query.length < 3) {
    return [];
  }

  final resp = await API.getRawClient().get(
    "/search",
    queryParameters: {'query': query},
  );
  final responseObject = resp.data as List;
  final List<AurPackage> packages =
      responseObject.map((e) => AurPackage.fromJson(e)).toList(growable: false);
  return packages;
}
