import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:riverpod_annotation/riverpod_annotation.dart';

import '../api/API.dart';
import '../models/extended_package.dart';
import '../models/simple_packge.dart';

part 'packages.g.dart';

@riverpod
Future<List<SimplePackage>> listPackages(Ref ref, {int? limit}) async {
  final resp = await API.getRawClient().get(
    "/packages/list",
    queryParameters: {'limit': limit},
  );

  final responseObject = resp.data as List;
  final List<SimplePackage> packages = responseObject
      .map((e) => SimplePackage.fromJson(e))
      .toList(growable: false);
  return packages;
}

@riverpod
Future<ExtendedPackage> getPackage(Ref ref, int id) async {
  final resp = await API.getRawClient().get("/package/$id");

  final package = ExtendedPackage.fromJson(resp.data);
  return package;
}
